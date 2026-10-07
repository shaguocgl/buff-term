import { memo, useCallback, useEffect, useRef, useState } from 'react';
import { open, save } from '@tauri-apps/plugin-dialog';
import {
  onSftpProgress,
  sftpCancelTransfer,
  sftpDelete,
  sftpDownload,
  sftpExists,
  sftpList,
  sftpMkdir,
  sftpRename,
  sftpUpload,
} from '../api';
import type { SftpProgressPayload } from '../api';
import type { Host, SftpEntry } from '../types';
import { fmtError } from '../utils/errors';
import { baseName, formatBytes, formatMtime, joinPath } from '../utils/remote';
import ConfirmModal from './ConfirmModal';
import PromptModal from './PromptModal';
import {
  DownloadIcon,
  FileEditIcon,
  FileIcon,
  FolderIcon,
  FolderPlusIcon,
  PencilIcon,
  RefreshIcon,
  TrashIcon,
  UploadIcon,
  XIcon,
} from './Icons';

interface Props {
  host: Host;
  /** 面板隐藏时保持挂载（传输任务继续），仅隐藏显示 */
  hidden?: boolean;
  /** 悬浮按钮「在内置编辑器中打开」：在中间区打开该文件 */
  onOpenFile: (path: string) => void;
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

/** 批量上传并发上限：避免一次性打开过多 SFTP 通道把连接打满 */
const UPLOAD_CONCURRENCY = 3;

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
function overwriteBody({
  conflicts,
  rest,
}: {
  conflicts: UploadItem[];
  rest: UploadItem[];
}): string {
  if (conflicts.length === 1 && rest.length === 0) {
    return `远端已存在 "${baseName(conflicts[0].remote)}"，覆盖它吗？`;
  }
  const names = conflicts
    .slice(0, CONFLICT_PREVIEW)
    .map((c) => `· ${baseName(c.remote)}`);
  const more = conflicts.length - names.length;
  const lines = [`以下 ${conflicts.length} 个文件在远端已存在：`, ...names];
  if (more > 0) lines.push(`· 还有 ${more} 个未列出`);
  if (rest.length > 0) lines.push('', `其余 ${rest.length} 个文件将直接上传。`);
  return lines.join('\n');
}

function SftpPanel({ host, hidden = false, onOpenFile, onClose }: Props) {
  const [cwd, setCwd] = useState('/');
  const [entries, setEntries] = useState<SftpEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<SftpEntry | null>(null);
  const [promptState, setPromptState] = useState<
    { kind: 'mkdir' } | { kind: 'rename'; entry: SftpEntry } | null
  >(null);
  // 传输任务独立于 busy：传输期间不锁定浏览/刷新
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});
  // 批量上传的覆盖确认：conflicts 为远端已存在的目标，rest 为可直接上传的目标
  const [overwriteBatch, setOverwriteBatch] = useState<{
    conflicts: UploadItem[];
    rest: UploadItem[];
  } | null>(null);
  const [pathInput, setPathInput] = useState('/');
  // 目录请求序号：快速连续切换目录时忽略过期响应，避免旧目录覆盖新目录
  const loadSeqRef = useRef(0);
  // 当前目录的最新值：批量上传结束时据此判断用户是否还停留在目标目录
  const cwdRef = useRef(cwd);

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

  const load = useCallback(
    async (path: string) => {
      const seq = ++loadSeqRef.current;
      setLoading(true);
      setError(null);
      try {
        const list = await sftpList(host.id, path);
        if (seq !== loadSeqRef.current) return; // 已有更新的请求，丢弃过期响应
        setEntries(list);
        setCwd(path);
        setPathInput(path);
      } catch (e) {
        if (seq !== loadSeqRef.current) return;
        setError(fmtError(e));
      } finally {
        if (seq === loadSeqRef.current) setLoading(false);
      }
    },
    // 只依赖 host.id：同一主机的 Host 对象引用可能随标签增删而变化，
    // 若依赖整个对象会导致 load 重建 → 挂载 effect 重跑 → 目录被重置回 /
    [host.id],
  );

  useEffect(() => {
    load('/');
  }, [load]);

  const run = async (
    action: () => Promise<{ ok: boolean; text: string }>,
    then: () => void,
  ) => {
    setBusy(true);
    setError(null);
    try {
      const res = await action();
      if (res.ok) {
        then();
      } else {
        setError(res.text || '操作失败');
      }
    } catch (e) {
      setError(fmtError(e));
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
          ? await sftpUpload(host.id, local, remote, id)
          : await sftpDownload(host.id, remote, local, id);
      return res.ok ? null : res.text || '传输失败';
    } catch (e) {
      const msg = fmtError(e);
      // 用户主动取消不算错误
      return msg.includes('取消') ? null : msg;
    } finally {
      setTransfers((prev) => {
        const next = { ...prev };
        delete next[id];
        return next;
      });
    }
  };

  // 批量上传：按 UPLOAD_CONCURRENCY 限流并发，失败逐条累加提示，
  // 全部结束后若用户仍在目标目录则刷新一次列表
  const uploadMany = async (items: UploadItem[]) => {
    const target = cwd;
    setError(null);
    const failures: string[] = [];
    await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      const err = await startTransfer('upload', item.local, item.remote);
      if (!err) return;
      failures.push(`${baseName(item.local)}：${err}`);
      setError(`上传失败（${failures.length}）：${failures.join('；')}`);
    });
    if (cwdRef.current === target) load(target);
  };

  const cancelTransfer = async (id: string) => {
    try {
      await sftpCancelTransfer(id);
    } catch {
      /* 忽略：任务可能已结束 */
    }
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
    // 上传前并发检查远端是否已存在：无冲突直接上传，有冲突先弹窗确认覆盖
    // （同样限流：每个检查都会独占一条 SFTP 通道）
    const checked = await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      try {
        return { item, exists: await sftpExists(host.id, item.remote) };
      } catch {
        return { item, exists: false };
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

  const handleDownload = async (entry: SftpEntry) => {
    const dest = await save({ defaultPath: entry.name });
    if (!dest) return;
    setError(null);
    const err = await startTransfer('download', dest, joinPath(cwd, entry.name));
    if (err) setError(err);
  };

  const handleDelete = (entry: SftpEntry) => {
    setConfirmDelete(entry);
  };

  const doDelete = async (entry: SftpEntry) => {
    setConfirmDelete(null);
    const remote = joinPath(cwd, entry.name);
    await run(() => sftpDelete(host.id, remote), () => load(cwd));
  };

  const handleMkdir = () => {
    setPromptState({ kind: 'mkdir' });
  };

  const handleRename = (entry: SftpEntry) => {
    setPromptState({ kind: 'rename', entry });
  };

  const submitPrompt = (value: string) => {
    const p = promptState;
    setPromptState(null);
    if (!p) return;
    if (p.kind === 'mkdir') {
      void run(() => sftpMkdir(host.id, joinPath(cwd, value)), () => load(cwd));
    } else if (p.kind === 'rename' && value !== p.entry.name) {
      void run(
        () =>
          sftpRename(host.id, joinPath(cwd, p.entry.name), joinPath(cwd, value)),
        () => load(cwd),
      );
    }
  };

  return (
    <aside
      className="sftp-panel"
      style={{
        width: 'var(--right-panel-width, 400px)',
        display: hidden ? 'none' : undefined,
      }}
    >
      <div className="sftp-header">
        <input
          className="sftp-path-input"
          value={pathInput}
          title={cwd}
          spellCheck={false}
          onChange={(e) => setPathInput(e.target.value)}
          onFocus={() => setPathInput(cwd)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              const target = pathInput.trim();
              if (target && target.startsWith('/')) load(target);
              else setPathInput(cwd);
            } else if (e.key === 'Escape') {
              setPathInput(cwd);
              (e.target as HTMLInputElement).blur();
            }
          }}
          onBlur={() => setPathInput(cwd)}
        />
        <div className="sftp-actions">
          <button className="icon-btn" title="刷新" onClick={() => load(cwd)} disabled={busy}>
            <RefreshIcon size={14} />
          </button>
          <button
            className="icon-btn"
            title="上传文件（可多选批量上传）"
            onClick={handleUpload}
            disabled={busy}
          >
            <UploadIcon size={14} />
          </button>
          <button className="icon-btn" title="新建文件夹" onClick={handleMkdir} disabled={busy}>
            <FolderPlusIcon size={14} />
          </button>
          <button className="icon-btn" title="关闭" onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      {Object.values(transfers).length > 0 && (
        <div className="sftp-transfers">
          {Object.values(transfers).length > 1 && (
            <div className="sftp-transfers-head">
              <span className="sftp-transfer-meta">
                {Object.values(transfers).length} 个传输进行中
              </span>
              <button
                className="btn ghost small"
                onClick={() =>
                  Object.values(transfers).forEach((t) => cancelTransfer(t.id))
                }
              >
                全部取消
              </button>
            </div>
          )}
          {Object.values(transfers).map((t) => {
            const pct =
              t.total > 0
                ? Math.min(100, Math.round((t.transferred / t.total) * 100))
                : 0;
            return (
              <div className="sftp-transfer" key={t.id}>
                <div className="sftp-transfer-head">
                  <span className="sftp-transfer-name">
                    {t.kind === 'upload' ? '↑' : '↓'} {t.name}
                  </span>
                  <span className="sftp-transfer-meta">
                    {t.total > 0
                      ? `${pct}% · ${formatBytes(t.transferred)}/${formatBytes(t.total)}`
                      : formatBytes(t.transferred)}
                  </span>
                  <button
                    className="icon-btn"
                    title="取消传输"
                    onClick={() => cancelTransfer(t.id)}
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

      <div className="sftp-body">
        {loading && <div className="sftp-status">加载中…</div>}
        {!loading && error && <div className="sftp-status err">{error}</div>}
        {!loading && !error && (
          <div className="sftp-list">
            {cwd !== '/' && (
              <div className="sftp-row" onClick={() => load(cwd.replace(/\/[^/]*$/, '') || '/')}>
                <FolderIcon size={15} />
                <span className="sftp-name">..</span>
              </div>
            )}
            {entries.map((entry) => {
              const full = joinPath(cwd, entry.name);
              return (
                <div
                  key={entry.name}
                  className="sftp-row"
                  // 双击进入目录 / 打开文件；单击不再触发，文件的开合统一走悬浮按钮
                  onDoubleClick={() => {
                    if (entry.is_dir) load(full);
                    else onOpenFile(full);
                  }}
                >
                  {entry.is_dir ? <FolderIcon size={15} /> : <FileIcon size={15} />}
                  <span
                    className={`sftp-name${entry.is_dir ? '' : ' plain'}`}
                    title={entry.is_dir ? '打开目录' : entry.name}
                    onClick={() => {
                      if (entry.is_dir) load(full);
                    }}
                  >
                    {entry.name}
                  </span>
                  {entry.is_symlink && (
                    <span className="sftp-link-badge" title="符号链接">link</span>
                  )}
                  <span className="sftp-size">
                    {entry.is_dir ? '—' : formatBytes(entry.size)}
                  </span>
                  <span className="sftp-mtime">{formatMtime(entry.mtime)}</span>
                  <div className="sftp-row-actions">
                    {!entry.is_dir && (
                      <button
                        className="icon-btn"
                        title="在内置编辑器中打开"
                        disabled={busy}
                        onClick={() => onOpenFile(full)}
                      >
                        <FileEditIcon size={13} />
                      </button>
                    )}
                    {!entry.is_dir && (
                      <button
                        className="icon-btn"
                        title="下载"
                        disabled={busy}
                        onClick={() => handleDownload(entry)}
                      >
                        <DownloadIcon size={13} />
                      </button>
                    )}
                    <button
                      className="icon-btn"
                      title="重命名"
                      disabled={busy}
                      onClick={() => handleRename(entry)}
                    >
                      <PencilIcon size={13} />
                    </button>
                    <button
                      className="icon-btn danger"
                      title="删除"
                      disabled={busy}
                      onClick={() => handleDelete(entry)}
                    >
                      <TrashIcon size={13} />
                    </button>
                  </div>
                </div>
              );
            })}
            {entries.length === 0 && <div className="sftp-status">空目录</div>}
          </div>
        )}
      </div>

      {overwriteBatch && (
        <ConfirmModal
          title="覆盖远端文件"
          body={overwriteBody(overwriteBatch)}
          confirmText={overwriteBatch.conflicts.length > 1 ? '全部覆盖' : '覆盖'}
          // 无冲突文件可传时「跳过冲突项」等同于取消，故不展示该按钮
          altText={overwriteBatch.rest.length > 0 ? '跳过冲突项' : undefined}
          danger
          onConfirm={() => {
            const t = overwriteBatch;
            setOverwriteBatch(null);
            void uploadMany([...t.conflicts, ...t.rest]);
          }}
          onAlt={() => {
            const t = overwriteBatch;
            setOverwriteBatch(null);
            void uploadMany(t.rest);
          }}
          onCancel={() => setOverwriteBatch(null)}
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title={confirmDelete.is_dir ? '删除目录' : '删除文件'}
          body={`确定删除 ${confirmDelete.is_dir ? '目录' : '文件'} "${confirmDelete.name}" 吗？`}
          confirmText="删除"
          danger
          onConfirm={() => doDelete(confirmDelete)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {promptState && (
        <PromptModal
          // key 按 kind 区分：新建/重命名切换时强制重建，避免输入框残留上一次的值
          key={promptState.kind === 'rename' ? `rename-${promptState.entry.name}` : 'mkdir'}
          title={promptState.kind === 'mkdir' ? '新建文件夹' : '重命名'}
          label={promptState.kind === 'mkdir' ? '文件夹名称' : '新名称'}
          initialValue={promptState.kind === 'rename' ? promptState.entry.name : ''}
          placeholder={promptState.kind === 'mkdir' ? '例如 logs' : undefined}
          onOk={submitPrompt}
          onCancel={() => setPromptState(null)}
        />
      )}
    </aside>
  );
}

export default memo(SftpPanel);
