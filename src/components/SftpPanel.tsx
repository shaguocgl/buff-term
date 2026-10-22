import { memo, useCallback, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { open, save } from '@tauri-apps/plugin-dialog';
import {
  onSftpProgress,
  sessionCwd,
  sftpCancelTransfer,
  sftpCheckSensitive,
  sftpDelete,
  sftpDownload,
  sftpExists,
  sftpList,
  sftpMkdir,
  sftpRename,
  sftpUpload,
} from '../api';
import type { SftpProgressPayload } from '../api';
import { useT, type TFunc } from '../i18n';
import type { Host, SftpEntry } from '../types';
import { fmtError } from '../utils/errors';
import { baseName, formatBytes, formatMtime, joinPath, parentPath } from '../utils/remote';
import ConfirmModal from './ConfirmModal';
import PromptModal from './PromptModal';
import {
  ChevronDownIcon,
  ChevronRightIcon,
  DownloadIcon,
  FileEditIcon,
  FileIcon,
  FolderIcon,
  FolderPlusIcon,
  PencilIcon,
  RefreshIcon,
  ServerIcon,
  TrashIcon,
  UploadIcon,
  XIcon,
} from './Icons';

interface Props {
  host: Host;
  /** 面板隐藏时保持挂载（传输任务与目录展开状态不重置），仅隐藏显示 */
  hidden?: boolean;
  /** 该主机当前终端会话 id：面板从隐藏切到可见时，把目录定位到该终端的 cwd */
  terminalSessionId: number | null;
  /** 悬浮按钮「在内置编辑器中打开」：在中间区打开该文件 */
  onOpenFile: (path: string) => void;
  /** 全局 Toast：敏感路径/传输失败等关键错误需要在右下角明显提示 */
  onNotify: (kind: 'success' | 'error' | 'info', message: string) => void;
  onClose: () => void;
}

interface Transfer {
  id: string;
  name: string;
  kind: 'upload' | 'download';
  transferred: number;
  total: number;
}

/** 待上传的本地文件与其远端目标路径 */
interface UploadItem {
  local: string;
  remote: string;
}

/** 目录节点的懒加载缓存：entries 缺失表示尚未加载 */
interface DirNode {
  entries?: SftpEntry[];
  loading?: boolean;
  error?: string;
}

/** 批量上传并发上限：避免一次性打开过多 SFTP 通道把连接打满 */
const UPLOAD_CONCURRENCY = 3;

/** 目录加载成功后预取的直接子目录上限（常见下一次点击就近乎零等待） */
const PREFETCH_LIMIT = 12;
/** 预取全局并发：深层 reveal 会链式触发多次预取，按全局限流避免瞬时打满 */
const PREFETCH_CONCURRENCY = 3;

/** 覆盖确认弹窗里最多列出的冲突文件名 */
const CONFLICT_PREVIEW = 8;

/** 限流并发映射：最多同时执行 limit 个任务，按原顺序返回结果 */
async function mapLimit<T, R>(
  items: T[],
  limit: number,
  task: (item: T) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length);
  let next = 0;
  const worker = async () => {
    for (let i = next++; i < items.length; i = next++) {
      results[i] = await task(items[i]);
    }
  };
  await Promise.all(
    Array.from({ length: Math.min(limit, items.length) }, worker),
  );
  return results;
}

/** 覆盖确认弹窗文案：单个冲突保持原有措辞，多个冲突列出清单 */
function overwriteBody(
  {
    conflicts,
    rest,
  }: {
    conflicts: UploadItem[];
    rest: UploadItem[];
  },
  t: TFunc,
): string {
  if (conflicts.length === 1 && rest.length === 0) {
    return t('sftp.overwriteOne', { name: baseName(conflicts[0].remote) });
  }
  const names = conflicts
    .slice(0, CONFLICT_PREVIEW)
    .map((c) => `· ${baseName(c.remote)}`);
  const more = conflicts.length - names.length;
  const lines = [t('sftp.overwriteManyHead', { n: conflicts.length }), ...names];
  if (more > 0) lines.push(t('sftp.overwriteMore', { n: more }));
  if (rest.length > 0) {
    lines.push('', t('sftp.overwriteRest', { n: rest.length }));
  }
  return lines.join('\n');
}

/** 路径 → 祖先目录链（含自身）：/a/b/c → ["/", "/a", "/a/b", "/a/b/c"] */
function ancestorsOf(path: string): string[] {
  const out = ['/'];
  let cur = '';
  for (const seg of path.split('/').filter(Boolean)) {
    cur += `/${seg}`;
    out.push(cur);
  }
  return out;
}

function SftpPanel({
  host,
  hidden = false,
  terminalSessionId,
  onOpenFile,
  onNotify,
  onClose,
}: Props) {
  const t = useT();
  // 当前选中的目录：上传/新建/删除等操作的目标目录；树中只有目录行可选中
  const [cwd, setCwd] = useState('/');
  // 已加载目录缓存与展开集合：懒加载——展开时才请求 sftp_list
  const [nodes, setNodes] = useState<Record<string, DirNode>>({});
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set(['/']));
  const [busy, setBusy] = useState(false);
  // 确认/输入弹窗记住操作所在的目录行：用户可选中别的目录，弹窗仍作用于触发它的那一级
  const [confirmDelete, setConfirmDelete] = useState<{
    dir: string;
    entry: SftpEntry;
  } | null>(null);
  const [promptState, setPromptState] = useState<
    | { kind: 'mkdir'; dir: string }
    | { kind: 'rename'; dir: string; entry: SftpEntry }
    | null
  >(null);
  // 传输任务独立于 busy：传输期间不锁定浏览/刷新
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});
  // 批量上传的覆盖确认：conflicts 为远端已存在的目标，rest 为可直接上传的目标
  const [overwriteBatch, setOverwriteBatch] = useState<{
    conflicts: UploadItem[];
    rest: UploadItem[];
  } | null>(null);
  // 上传目标命中敏感路径时先做一次更强确认
  const [sensitiveBatch, setSensitiveBatch] = useState<UploadItem[] | null>(null);
  // 顶部地址区：面包屑（可点击跳转）与直接输入两种形态，编辑态显示输入框
  const [editingPath, setEditingPath] = useState(false);
  const [pathInput, setPathInput] = useState('/');
  // 每个目录的加载序号：同一目录的快速重载丢弃过期响应，不同目录并发互不影响
  const loadSeqRef = useRef<Map<string, number>>(new Map());
  // 当前目录的最新值：批量上传结束时据此判断用户是否还停留在目标目录
  const cwdRef = useRef(cwd);
  const bodyRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    cwdRef.current = cwd;
  }, [cwd]);

  // 跟随后端 sftp:progress 事件刷新进度条
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    onSftpProgress((p: SftpProgressPayload) => {
      setTransfers((prev) => {
        const t = prev[p.transfer_id];
        if (!t) return prev;
        return {
          ...prev,
          [p.transfer_id]: {
            ...t,
            transferred: p.transferred,
            total: p.total || t.total,
          },
        };
      });
    }).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  /** 加载某个目录的子项；per-path 序号丢弃同一目录的过期响应。
   *  prefetch=true（用户触发的加载）成功后，后台预取直接子目录——
   *  下一次点击展开通常直接命中缓存，近乎零等待 */
  const loadDir = useCallback(
    async (path: string, prefetch = false) => {
      const seq = (loadSeqRef.current.get(path) ?? 0) + 1;
      loadSeqRef.current.set(path, seq);
      setNodes((prev) => ({
        ...prev,
        [path]: { ...prev[path], loading: true, error: undefined },
      }));
      try {
        const list = await sftpList(host.id, path);
        if (loadSeqRef.current.get(path) !== seq) return;
        setNodes((prev) => ({
          ...prev,
          [path]: { entries: list, loading: false },
        }));
        if (prefetch) prefetchDirs(path, list);
      } catch (e) {
        if (loadSeqRef.current.get(path) !== seq) return;
        setNodes((prev) => ({
          ...prev,
          [path]: { loading: false, error: fmtError(e) },
        }));
      }
    },
    [host.id],
  );

  /** 后台预取队列：全局最多 PREFETCH_CONCURRENCY 个在途请求；
   *  入队路径去重，已缓存/加载中的跳过；预取本身不再触发下级预取 */
  const prefetchQueueRef = useRef<string[]>([]);
  const prefetchRunningRef = useRef(0);
  const drainPrefetch = useCallback(() => {
    while (
      prefetchRunningRef.current < PREFETCH_CONCURRENCY &&
      prefetchQueueRef.current.length > 0
    ) {
      const p = prefetchQueueRef.current.shift()!;
      const n = nodesRef.current[p];
      if (n && (n.entries !== undefined || n.loading)) continue;
      prefetchRunningRef.current += 1;
      void loadDir(p).finally(() => {
        prefetchRunningRef.current -= 1;
        drainPrefetch();
      });
    }
  }, [loadDir]);

  const prefetchDirs = useCallback(
    (parent: string, entries: SftpEntry[]) => {
      const queued = new Set(prefetchQueueRef.current);
      const dirs = entries
        .filter((e) => e.is_dir)
        .slice(0, PREFETCH_LIMIT)
        .map((e) => joinPath(parent, e.name))
        .filter((p) => !queued.has(p));
      if (dirs.length === 0) return;
      prefetchQueueRef.current.push(...dirs);
      drainPrefetch();
    },
    [drainPrefetch],
  );

  useEffect(() => {
    void loadDir('/', true);
  }, [loadDir]);

  const nodesRef = useRef(nodes);
  useEffect(() => {
    nodesRef.current = nodes;
  }, [nodes]);

  /** 未加载过才请求：展开/定位路径上的祖先目录复用缓存（属用户触发，带预取） */
  const ensureLoaded = useCallback(
    (path: string) => {
      const node = nodesRef.current[path];
      if (!node || (node.entries === undefined && !node.loading)) {
        void loadDir(path, true);
      }
    },
    [loadDir],
  );

  /** 展开/折叠某个目录：展开时懒加载子项 */
  const toggleDir = useCallback(
    (path: string) => {
      const wasOpen = expanded.has(path);
      setExpanded((prev) => {
        const next = new Set(prev);
        if (wasOpen) next.delete(path);
        else next.add(path);
        return next;
      });
      if (!wasOpen) ensureLoaded(path);
    },
    [expanded, ensureLoaded],
  );

  /** 把某路径定位到树中：展开并加载其全部祖先，选中该目录并滚动到可见 */
  const reveal = useCallback(
    async (path: string) => {
      const chain = ancestorsOf(path);
      const ancestors = chain.slice(0, -1);
      setExpanded((prev) => {
        const next = new Set(prev);
        ancestors.forEach((p) => next.add(p));
        return next;
      });
      await Promise.all(ancestors.map(ensureLoaded));
      setCwd(path);
      requestAnimationFrame(() => {
        bodyRef.current
          ?.querySelector(`[data-path="${CSS.escape(path)}"]`)
          ?.scrollIntoView({ block: 'nearest' });
      });
    },
    [ensureLoaded],
  );

  // 面板由隐藏切到可见时，把目录定位到「当前终端」的工作目录
  const wasHiddenRef = useRef(true);
  useEffect(() => {
    if (hidden) {
      wasHiddenRef.current = true;
      return;
    }
    if (!wasHiddenRef.current) return;
    wasHiddenRef.current = false;
    if (terminalSessionId == null) return;
    void sessionCwd(terminalSessionId)
      .then((dir) => {
        // 仅接受合法的绝对路径；cwd 未知/跟踪失败时保持当前目录
        if (dir && dir.startsWith('/')) void reveal(dir);
      })
      .catch(() => {});
  }, [hidden, terminalSessionId, reveal]);

  /** 选中目录行：已选中且展开时再点折叠（根目录恒展开）；否则选中并展开 */
  const onDirClick = useCallback(
    (path: string) => {
      setCwd(path);
      if (path !== '/' && cwdRef.current === path && expanded.has(path)) {
        setExpanded((prev) => {
          const next = new Set(prev);
          next.delete(path);
          return next;
        });
        return;
      }
      if (!expanded.has(path)) {
        setExpanded((prev) => {
          const next = new Set(prev);
          next.add(path);
          return next;
        });
        ensureLoaded(path);
      }
    },
    [expanded, ensureLoaded],
  );

  /** 操作后清理某目录及其子树的缓存/展开态（删除/重命名目录时用） */
  const purgeTree = useCallback((path: string) => {
    // 预取队列里已删除/已改名的子树路径一并丢弃，避免加载出不存在的错误节点
    prefetchQueueRef.current = prefetchQueueRef.current.filter(
      (p) => p !== path && !p.startsWith(`${path}/`),
    );
    setNodes((prev) => {
      const next = { ...prev };
      for (const key of Object.keys(next)) {
        if (key === path || key.startsWith(`${path}/`)) delete next[key];
      }
      return next;
    });
    setExpanded((prev) => {
      const next = new Set(prev);
      let touched = false;
      for (const p of next) {
        if (p === path || p.startsWith(`${path}/`)) {
          next.delete(p);
          touched = true;
        }
      }
      return touched ? next : prev;
    });
    if (cwdRef.current === path || cwdRef.current.startsWith(`${path}/`)) {
      setCwd(parentPath(path));
    }
  }, []);

  /** 刷新：重拉根目录与所有展开中的目录（折叠目录的缓存随之废弃） */
  const refresh = useCallback(() => {
    for (const p of expanded) void loadDir(p, true);
    if (!expanded.has('/')) void loadDir('/', true);
  }, [expanded, loadDir]);

  const run = async (
    dir: string,
    action: () => Promise<{ ok: boolean; text: string }>,
    then: () => void,
  ) => {
    setBusy(true);
    const setErr = (error: string) =>
      setNodes((prev) => ({
        ...prev,
        [dir]: { ...prev[dir], error },
      }));
    try {
      const res = await action();
      if (res.ok) {
        then();
      } else {
        const message = res.text || t('sftp.opFailed');
        setErr(message);
        onNotify('error', message);
      }
    } catch (e) {
      const message = fmtError(e);
      setErr(message);
      onNotify('error', message);
    } finally {
      setBusy(false);
    }
  };

  // 启动一个传输任务：不经过 run()（不置 busy），进度走 sftp:progress 事件。
  // 返回失败原因（成功或用户取消返回 null），由调用方决定如何提示。
  const startTransfer = async (
    kind: 'upload' | 'download',
    local: string,
    remote: string,
  ): Promise<string | null> => {
    const id = `t-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const name = kind === 'upload' ? baseName(local) : baseName(remote);
    setTransfers((prev) => ({
      ...prev,
      [id]: { id, name, kind, transferred: 0, total: 0 },
    }));
    try {
      const res =
        kind === 'upload'
          ? await sftpUpload(host.id, local, remote, id, true)
          : await sftpDownload(host.id, remote, local, id);
      return res.ok ? null : res.text || t('sftp.transferFailed');
    } catch (e) {
      const msg = fmtError(e);
      // 用户主动取消不算错误（后端按当前语言返回“已取消 / cancelled”）
      return /取消|cancel/i.test(msg) ? null : msg;
    } finally {
      setTransfers((prev) => {
        const next = { ...prev };
        delete next[id];
        return next;
      });
    }
  };

  // 批量上传：按 UPLOAD_CONCURRENCY 限流并发，失败逐条累加提示，
  // 全部结束后若用户仍在目标目录则刷新该目录的子项
  const uploadMany = async (items: UploadItem[]) => {
    const target = cwd;
    const failures: string[] = [];
    await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      const err = await startTransfer('upload', item.local, item.remote);
      if (!err) return;
      failures.push(
        `${baseName(item.local)}${t('sftp.uploadFailedItemSep')}${err}`,
      );
      setNodes((prev) => ({
        ...prev,
        [target]: {
          ...prev[target],
          error: t('sftp.uploadFailed', {
            n: failures.length,
            list: failures.join(t('sftp.uploadFailedListSep')),
          }),
        },
      }));
    });
    if (failures.length > 0) {
      onNotify(
        'error',
        t('sftp.uploadFailed', {
          n: failures.length,
          list: failures.join(t('sftp.uploadFailedListSep')),
        }),
      );
    }
    if (cwdRef.current === target) void loadDir(target, true);
  };

  const cancelTransfer = async (id: string) => {
    try {
      await sftpCancelTransfer(id);
    } catch {
      /* 忽略：任务可能已结束 */
    }
  };

  const beginUpload = async (items: UploadItem[]) => {
    // 上传前并发检查远端是否已存在：无冲突直接上传，有冲突先弹窗确认覆盖
    // （同样限流：每个检查都会独占一条 SFTP 通道）
    const checked = await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      try {
        return { item, exists: await sftpExists(host.id, item.remote) };
      } catch {
        // 检查失败时按“已存在”处理，宁可多一次覆盖确认，也不静默覆盖
        return { item, exists: true };
      }
    });
    const conflicts = checked.filter((c) => c.exists).map((c) => c.item);
    const rest = checked.filter((c) => !c.exists).map((c) => c.item);
    if (conflicts.length > 0) {
      setOverwriteBatch({ conflicts, rest });
      return;
    }
    void uploadMany(items);
  };

  const handleUpload = async () => {
    // 支持多选：一次可选中多个文件批量上传
    const picked = await open({ multiple: true });
    const locals = picked ? (Array.isArray(picked) ? picked : [picked]) : [];
    if (locals.length === 0) return;
    const items: UploadItem[] = locals.map((local) => ({
      local,
      remote: joinPath(cwd, baseName(local)),
    }));
    const sensitive = await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      try {
        return await sftpCheckSensitive(item.remote);
      } catch {
        // 无法判断时按敏感处理，先让用户确认
        return true;
      }
    });
    if (sensitive.some(Boolean)) {
      setSensitiveBatch(items);
      return;
    }
    void beginUpload(items);
  };

  const handleDownload = async (dir: string, entry: SftpEntry) => {
    const dest = await save({ defaultPath: entry.name });
    if (!dest) return;
    const err = await startTransfer('download', dest, joinPath(dir, entry.name));
    if (err) {
      setNodes((prev) => ({
        ...prev,
        [dir]: { ...prev[dir], error: err },
      }));
      // 敏感本地路径/权限错误可能发生在下载开始前，面板内联文字容易被忽略。
      onNotify('error', err);
    }
  };

  const doDelete = async (dir: string, entry: SftpEntry) => {
    setConfirmDelete(null);
    const remote = joinPath(dir, entry.name);
    await run(
      dir,
      () => sftpDelete(host.id, remote, true),
      () => {
        if (entry.is_dir) purgeTree(remote);
        void loadDir(dir, true);
      },
    );
  };

  const submitPrompt = (value: string) => {
    // 重命名涉及敏感路径时，后端仍会要求 confirmed；前端在用户输入新名称
    // 之后已经是一次明确操作，这里不额外拦截。

    const p = promptState;
    setPromptState(null);
    if (!p) return;
    if (p.kind === 'mkdir') {
      void run(
        p.dir,
        () => sftpMkdir(host.id, joinPath(p.dir, value)),
        () => loadDir(p.dir, true),
      );
    } else if (p.kind === 'rename' && value !== p.entry.name) {
      const from = joinPath(p.dir, p.entry.name);
      const to = joinPath(p.dir, value);
      void run(
        p.dir,
        () => sftpRename(host.id, from, to, true),
        () => {
          if (p.entry.is_dir) {
            purgeTree(from);
            purgeTree(to);
          }
          void loadDir(p.dir, true);
        },
      );
    }
  };

  /** 递归渲染目录行及其展开子级 */
  const renderRows = (dir: string, depth: number): ReactNode[] => {
    const node = nodes[dir];
    const rows: ReactNode[] = [];
    const indent = depth * 14;
    if (node?.loading) {
      rows.push(
        <div
          key={`${dir}-loading`}
          className="sftp-status sftp-tree-note"
          style={{ paddingLeft: 34 + indent }}
        >
          {t('common.loading')}
        </div>,
      );
    }
    if (node?.error) {
      rows.push(
        <div
          key={`${dir}-error`}
          className="sftp-status err sftp-tree-note"
          style={{ paddingLeft: 34 + indent }}
        >
          {node.error}
        </div>,
      );
    }
    for (const entry of node?.entries ?? []) {
      const full = joinPath(dir, entry.name);
      const isExpanded = expanded.has(full);
      const isSelected = entry.is_dir && cwd === full;
      rows.push(
        <div
          key={full}
          data-path={full}
          className={`sftp-row${isSelected ? ' selected' : ''}`}
          style={{ paddingLeft: 12 + indent }}
          onClick={() => {
            if (entry.is_dir) onDirClick(full);
          }}
          onDoubleClick={() => {
            if (!entry.is_dir) onOpenFile(full);
          }}
        >
          {entry.is_dir ? (
            <button
              className="sftp-chevron"
              title={isExpanded ? t('sftp.collapse') : t('sftp.expand')}
              onClick={(e) => {
                e.stopPropagation();
                toggleDir(full);
              }}
            >
              {isExpanded ? (
                <ChevronDownIcon size={12} />
              ) : (
                <ChevronRightIcon size={12} />
              )}
            </button>
          ) : (
            <span className="sftp-chevron" />
          )}
          {entry.is_dir ? <FolderIcon size={15} /> : <FileIcon size={15} />}
          <span
            className={`sftp-name${entry.is_dir ? '' : ' plain'}`}
            title={entry.is_dir ? full : entry.name}
          >
            {entry.name}
          </span>
          {entry.is_symlink && (
            <span className="sftp-link-badge" title={t('sftp.symlink')}>link</span>
          )}
          <span className="sftp-size">
            {entry.is_dir ? '—' : formatBytes(entry.size)}
          </span>
          <span className="sftp-mtime">{formatMtime(entry.mtime)}</span>
          <div className="sftp-row-actions">
            {!entry.is_dir && (
              <button
                className="icon-btn"
                title={t('sftp.openInEditor')}
                disabled={busy}
                onClick={(e) => {
                  e.stopPropagation();
                  onOpenFile(full);
                }}
              >
                <FileEditIcon size={13} />
              </button>
            )}
            {!entry.is_dir && (
              <button
                className="icon-btn"
                title={t('sftp.download')}
                disabled={busy}
                onClick={(e) => {
                  e.stopPropagation();
                  void handleDownload(dir, entry);
                }}
              >
                <DownloadIcon size={13} />
              </button>
            )}
            <button
              className="icon-btn"
              title={t('sftp.rename')}
              disabled={busy}
              onClick={(e) => {
                e.stopPropagation();
                setPromptState({ kind: 'rename', dir, entry });
              }}
            >
              <PencilIcon size={13} />
            </button>
            <button
              className="icon-btn danger"
              title={t('common.delete')}
              disabled={busy}
              onClick={(e) => {
                e.stopPropagation();
                setConfirmDelete({ dir, entry });
              }}
            >
              <TrashIcon size={13} />
            </button>
          </div>
        </div>,
      );
      if (entry.is_dir && isExpanded) {
        rows.push(...renderRows(full, depth + 1));
      }
    }
    if (node?.entries && node.entries.length === 0) {
      rows.push(
        <div
          key={`${dir}-empty`}
          className="sftp-status sftp-tree-note"
          style={{ paddingLeft: 34 + indent }}
        >
          {t('sftp.empty')}
        </div>,
      );
    }
    return rows;
  };

  const rootNode = nodes['/'];
  const crumbs = ancestorsOf(cwd);

  return (
    <aside
      className="sftp-panel"
      style={{
        width: 'var(--right-panel-width, 400px)',
        display: hidden ? 'none' : undefined,
      }}
    >
      <div className="sftp-header">
        <div
          className="sftp-host"
          title={`${host.name} (${host.username}@${host.address}:${host.port})`}
        >
          <ServerIcon size={13} />
          <span className="sftp-host-name">{host.name}</span>
          <span className="sftp-host-addr">
            {host.username}@{host.address}:{host.port}
          </span>
        </div>
        <div className="sftp-actions">
          <button
            className="icon-btn"
            title={t('common.refresh')}
            onClick={refresh}
            disabled={busy}
          >
            <RefreshIcon size={14} />
          </button>
          <button
            className="icon-btn"
            title={t('sftp.uploadTitle')}
            onClick={handleUpload}
            disabled={busy}
          >
            <UploadIcon size={14} />
          </button>
          <button
            className="icon-btn"
            title={t('sftp.newFolder')}
            onClick={() => setPromptState({ kind: 'mkdir', dir: cwd })}
            disabled={busy}
          >
            <FolderPlusIcon size={14} />
          </button>
          <button className="icon-btn" title={t('common.close')} onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      {/* 当前目录：面包屑形态默认展示，铅笔/双击切换到路径输入 */}
      <div className="sftp-crumbbar">
        {editingPath ? (
          <input
            className="sftp-path-input"
            value={pathInput}
            autoFocus
            spellCheck={false}
            onChange={(e) => setPathInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                const target = pathInput.trim();
                if (target && target.startsWith('/')) {
                  void reveal(target);
                  setEditingPath(false);
                }
              } else if (e.key === 'Escape') {
                setEditingPath(false);
              }
            }}
            onBlur={() => setEditingPath(false)}
          />
        ) : (
          <div
            className="sftp-crumb"
            title={cwd}
            onDoubleClick={() => {
              setPathInput(cwd);
              setEditingPath(true);
            }}
          >
            {crumbs.map((seg, i) => (
              <span key={seg} className="sftp-crumb-item">
                {i > 0 && <span className="sftp-crumb-sep">/</span>}
                <button
                  className={`sftp-crumb-seg${i === crumbs.length - 1 ? ' current' : ''}`}
                  onClick={() => void reveal(seg)}
                >
                  {seg === '/' ? t('sftp.rootDir') : baseName(seg)}
                </button>
              </span>
            ))}
            <button
              className="icon-btn sftp-edit-path"
              title={t('sftp.editPath')}
              onClick={() => {
                setPathInput(cwd);
                setEditingPath(true);
              }}
            >
              <PencilIcon size={12} />
            </button>
          </div>
        )}
      </div>

      {Object.values(transfers).length > 0 && (
        <div className="sftp-transfers">
          {Object.values(transfers).length > 1 && (
            <div className="sftp-transfers-head">
              <span className="sftp-transfer-meta">
                {t('sftp.transfersCount', { n: Object.values(transfers).length })}
              </span>
              <button
                className="btn ghost small"
                onClick={() =>
                  Object.values(transfers).forEach((t) => cancelTransfer(t.id))
                }
              >
                {t('sftp.cancelAll')}
              </button>
            </div>
          )}
          {Object.values(transfers).map((tr) => {
            const pct =
              tr.total > 0
                ? Math.min(100, Math.round((tr.transferred / tr.total) * 100))
                : 0;
            return (
              <div className="sftp-transfer" key={tr.id}>
                <div className="sftp-transfer-head">
                  <span className="sftp-transfer-name">
                    {tr.kind === 'upload' ? '↑' : '↓'} {tr.name}
                  </span>
                  <span className="sftp-transfer-meta">
                    {tr.total > 0
                      ? `${pct}% · ${formatBytes(tr.transferred)}/${formatBytes(tr.total)}`
                      : formatBytes(tr.transferred)}
                  </span>
                  <button
                    className="icon-btn"
                    title={t('sftp.cancelTransfer')}
                    onClick={() => cancelTransfer(tr.id)}
                  >
                    <XIcon size={12} />
                  </button>
                </div>
                <div className="sftp-transfer-bar">
                  <span style={{ width: `${pct}%` }} />
                </div>
              </div>
            );
          })}
        </div>
      )}

      <div className="sftp-body" ref={bodyRef}>
        {/* 根目录行：恒展开，cwd=/ 时可被选中 */}
        <div
          data-path="/"
          className={`sftp-row${cwd === '/' ? ' selected' : ''}`}
          onClick={() => onDirClick('/')}
        >
          <span className="sftp-chevron">
            <ChevronDownIcon size={12} />
          </span>
          <FolderIcon size={15} />
          <span className="sftp-name" title="/">
            /
          </span>
          <span className="sftp-size">—</span>
          <span className="sftp-mtime" />
        </div>
        {renderRows('/', 1)}
        {rootNode?.entries === undefined && !rootNode?.loading && !rootNode?.error && (
          <div className="sftp-status">{t('common.loading')}</div>
        )}
      </div>

      {sensitiveBatch && (
        <ConfirmModal
          title={t('sftp.sensitiveUploadTitle')}
          body={t('sftp.sensitiveUploadBody', { n: sensitiveBatch.length })}
          confirmText={t('common.confirm')}
          danger
          onConfirm={() => {
            const items = sensitiveBatch;
            setSensitiveBatch(null);
            void beginUpload(items);
          }}
          onCancel={() => setSensitiveBatch(null)}
        />
      )}
      {overwriteBatch && (
        <ConfirmModal
          title={t('sftp.overwriteTitle')}
          body={overwriteBody(overwriteBatch, t)}
          confirmText={
            overwriteBatch.conflicts.length > 1
              ? t('sftp.overwriteAll')
              : t('sftp.overwrite')
          }
          // 无冲突文件可传时「跳过冲突项」等同于取消，故不展示该按钮
          altText={
            overwriteBatch.rest.length > 0 ? t('sftp.skipConflicts') : undefined
          }
          danger
          onConfirm={() => {
            const batch = overwriteBatch;
            setOverwriteBatch(null);
            void uploadMany([...batch.conflicts, ...batch.rest]);
          }}
          onAlt={() => {
            const batch = overwriteBatch;
            setOverwriteBatch(null);
            void uploadMany(batch.rest);
          }}
          onCancel={() => setOverwriteBatch(null)}
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title={
            confirmDelete.entry.is_dir
              ? t('sftp.deleteDirTitle')
              : t('sftp.deleteFileTitle')
          }
          body={
            (confirmDelete.entry.is_dir
              ? t('sftp.deleteDirBody', { name: confirmDelete.entry.name })
              : t('sftp.deleteFileBody', { name: confirmDelete.entry.name })) +
            (confirmDelete.entry.sensitive
              ? `\n\n${t('sftp.sensitiveDeleteWarning')}`
              : '')
          }
          confirmText={t('common.delete')}
          danger
          onConfirm={() => doDelete(confirmDelete.dir, confirmDelete.entry)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {promptState && (
        <PromptModal
          // key 按 kind 区分：新建/重命名切换时强制重建，避免输入框残留上一次的值
          key={
            promptState.kind === 'rename'
              ? `rename-${promptState.entry.name}`
              : 'mkdir'
          }
          title={
            promptState.kind === 'mkdir' ? t('sftp.newFolder') : t('sftp.rename')
          }
          hint={
            promptState.kind === 'rename' && promptState.entry.sensitive
              ? t('sftp.sensitiveRenameWarning')
              : undefined
          }
          label={
            promptState.kind === 'mkdir'
              ? t('sftp.newFolderLabel')
              : t('sftp.newNameLabel')
          }
          initialValue={
            promptState.kind === 'rename' ? promptState.entry.name : ''
          }
          placeholder={
            promptState.kind === 'mkdir'
              ? t('sftp.newFolderPlaceholder')
              : undefined
          }
          onOk={submitPrompt}
          onCancel={() => setPromptState(null)}
        />
      )}
    </aside>
  );
}

export default memo(SftpPanel);
