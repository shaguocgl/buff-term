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
import { useT, type TFunc } from '../i18n';
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

function SftpPanel({ host, hidden = false, onOpenFile, onClose }: Props) {
  const t = useT();
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
        setError(res.text || t('sftp.opFailed'));
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
  // 全部结束后若用户仍在目标目录则刷新一次列表
  const uploadMany = async (items: UploadItem[]) => {
    const target = cwd;
    setError(null);
    const failures: string[] = [];
    await mapLimit(items, UPLOAD_CONCURRENCY, async (item) => {
      const err = await startTransfer('upload', item.local, item.remote);
      if (!err) return;
      failures.push(
        `${baseName(item.local)}${t('sftp.uploadFailedItemSep')}${err}`,
      );
      setError(
        t('sftp.uploadFailed', {
          n: failures.length,
          list: failures.join(t('sftp.uploadFailedListSep')),
        }),
      );
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
          <button className="icon-btn" title={t('common.refresh')} onClick={() => load(cwd)} disabled={busy}>
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
          <button className="icon-btn" title={t('sftp.newFolder')} onClick={handleMkdir} disabled={busy}>
            <FolderPlusIcon size={14} />
          </button>
          <button className="icon-btn" title={t('common.close')} onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
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

      <div className="sftp-body">
        {loading && <div className="sftp-status">{t('common.loading')}</div>}
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
                    title={entry.is_dir ? t('sftp.openDir') : entry.name}
                    onClick={() => {
                      if (entry.is_dir) load(full);
                    }}
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
                        onClick={() => onOpenFile(full)}
                      >
                        <FileEditIcon size={13} />
                      </button>
                    )}
                    {!entry.is_dir && (
                      <button
                        className="icon-btn"
                        title={t('sftp.download')}
                        disabled={busy}
                        onClick={() => handleDownload(entry)}
                      >
                        <DownloadIcon size={13} />
                      </button>
                    )}
                    <button
                      className="icon-btn"
                      title={t('sftp.rename')}
                      disabled={busy}
                      onClick={() => handleRename(entry)}
                    >
                      <PencilIcon size={13} />
                    </button>
                    <button
                      className="icon-btn danger"
                      title={t('common.delete')}
                      disabled={busy}
                      onClick={() => handleDelete(entry)}
                    >
                      <TrashIcon size={13} />
                    </button>
                  </div>
                </div>
              );
            })}
            {entries.length === 0 && <div className="sftp-status">{t('sftp.empty')}</div>}
          </div>
        )}
      </div>

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
            confirmDelete.is_dir
              ? t('sftp.deleteDirTitle')
              : t('sftp.deleteFileTitle')
          }
          body={
            confirmDelete.is_dir
              ? t('sftp.deleteDirBody', { name: confirmDelete.name })
              : t('sftp.deleteFileBody', { name: confirmDelete.name })
          }
          confirmText={t('common.delete')}
          danger
          onConfirm={() => doDelete(confirmDelete)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {promptState && (
        <PromptModal
          // key 按 kind 区分：新建/重命名切换时强制重建，避免输入框残留上一次的值
          key={promptState.kind === 'rename' ? `rename-${promptState.entry.name}` : 'mkdir'}
          title={
            promptState.kind === 'mkdir' ? t('sftp.newFolder') : t('sftp.rename')
          }
          label={
            promptState.kind === 'mkdir'
              ? t('sftp.newFolderLabel')
              : t('sftp.newNameLabel')
          }
          initialValue={promptState.kind === 'rename' ? promptState.entry.name : ''}
          placeholder={
            promptState.kind === 'mkdir' ? t('sftp.newFolderPlaceholder') : undefined
          }
          onOk={submitPrompt}
          onCancel={() => setPromptState(null)}
        />
      )}
    </aside>
  );
}

export default memo(SftpPanel);
