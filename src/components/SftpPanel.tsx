import { useCallback, useEffect, useState } from 'react';
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
import type { Host } from '../types';
import { fmtError } from '../utils/errors';
import ConfirmModal from './ConfirmModal';
import PromptModal from './PromptModal';
import {
  DownloadIcon,
  FileIcon,
  FolderIcon,
  FolderPlusIcon,
  RefreshIcon,
  TrashIcon,
  UploadIcon,
  XIcon,
} from './Icons';

interface Entry {
  name: string;
  isDir: boolean;
  size: string;
  mtime: string;
}

interface Props {
  host: Host;
  panelWidth?: number;
  onClose: () => void;
}

function joinPath(dir: string, name: string) {
  if (dir === '/' || dir === '') return `/${name}`;
  return `${dir.replace(/\/+$/, '')}/${name}`;
}

function baseName(p: string) {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

function formatBytes(n: number): string {
  if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${n} B`;
}

interface Transfer {
  id: string;
  name: string;
  kind: 'upload' | 'download';
  transferred: number;
  total: number;
}

function parseListing(text: string): Entry[] {
  const entries: Entry[] = [];
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (!t || t.startsWith('total ')) continue;
    const parts = t.split(/\s+/);
    if (parts.length < 9) continue;
    const perms = parts[0];
    if (!perms.startsWith('-') && !perms.startsWith('d')) continue;
    const name = parts.slice(8).join(' ');
    if (name === '.' || name === '..') continue;
    entries.push({
      name,
      isDir: perms.startsWith('d'),
      size: parts[4],
      mtime: `${parts[5]} ${parts[6]} ${parts[7]}`,
    });
  }
  return entries;
}

export default function SftpPanel({ host, panelWidth = 400, onClose }: Props) {
  const [cwd, setCwd] = useState('/');
  const [entries, setEntries] = useState<Entry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<Entry | null>(null);
  const [promptState, setPromptState] = useState<
    { kind: 'mkdir' } | { kind: 'rename'; entry: Entry } | null
  >(null);
  // 传输任务独立于 busy：传输期间不锁定浏览/刷新
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});
  const [overwriteTarget, setOverwriteTarget] = useState<{
    local: string;
    remote: string;
  } | null>(null);
  const [pathInput, setPathInput] = useState('/');

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
      setLoading(true);
      setError(null);
      try {
        const res = await sftpList(host, path);
        if (res.ok) {
          setEntries(parseListing(res.text));
          setCwd(path);
          setPathInput(path);
        } else {
          setError(res.text || '目录读取失败');
        }
      } catch (e) {
        setError(fmtError(e));
      } finally {
        setLoading(false);
      }
    },
    [host],
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

  // 启动一个传输任务：不经过 run()（不置 busy），进度走 sftp:progress 事件
  const startTransfer = async (
    kind: 'upload' | 'download',
    local: string,
    remote: string,
  ) => {
    const id = `t-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const name = kind === 'upload' ? baseName(local) : baseName(remote);
    setTransfers((prev) => ({
      ...prev,
      [id]: { id, name, kind, transferred: 0, total: 0 },
    }));
    setError(null);
    try {
      const res =
        kind === 'upload'
          ? await sftpUpload(host, local, remote, id)
          : await sftpDownload(host, remote, local, id);
      if (!res.ok) setError(res.text || '传输失败');
      else if (kind === 'upload') load(cwd);
    } catch (e) {
      const msg = fmtError(e);
      // 用户主动取消不算错误
      if (!msg.includes('取消')) setError(msg);
    } finally {
      setTransfers((prev) => {
        const next = { ...prev };
        delete next[id];
        return next;
      });
    }
  };

  const cancelTransfer = async (id: string) => {
    try {
      await sftpCancelTransfer(id);
    } catch {
      /* 忽略：任务可能已结束 */
    }
  };

  const handleUpload = async () => {
    const picked = await open({ multiple: false });
    if (!picked || typeof picked !== 'string') return;
    const remote = joinPath(cwd, baseName(picked));
    // 上传前检查远端是否已存在，存在则弹窗确认覆盖
    let exists = false;
    try {
      exists = await sftpExists(host, remote);
    } catch {
      exists = false;
    }
    if (exists) {
      setOverwriteTarget({ local: picked, remote });
      return;
    }
    void startTransfer('upload', picked, remote);
  };

  const handleDownload = async (entry: Entry) => {
    const dest = await save({ defaultPath: entry.name });
    if (!dest) return;
    const remote = joinPath(cwd, entry.name);
    void startTransfer('download', dest, remote);
  };

  const handleDelete = (entry: Entry) => {
    setConfirmDelete(entry);
  };

  const doDelete = async (entry: Entry) => {
    setConfirmDelete(null);
    const remote = joinPath(cwd, entry.name);
    await run(() => sftpDelete(host, remote), () => load(cwd));
  };

  const handleMkdir = () => {
    setPromptState({ kind: 'mkdir' });
  };

  const handleRename = (entry: Entry) => {
    setPromptState({ kind: 'rename', entry });
  };

  const submitPrompt = (value: string) => {
    const p = promptState;
    setPromptState(null);
    if (!p) return;
    if (p.kind === 'mkdir') {
      void run(() => sftpMkdir(host, joinPath(cwd, value)), () => load(cwd));
    } else if (p.kind === 'rename' && value !== p.entry.name) {
      void run(
        () =>
          sftpRename(host, joinPath(cwd, p.entry.name), joinPath(cwd, value)),
        () => load(cwd),
      );
    }
  };

  return (
    <aside className="sftp-panel" style={{ width: panelWidth }}>
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
          <button className="icon-btn" title="上传文件" onClick={handleUpload} disabled={busy}>
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
            {entries.map((entry) => (
              <div
                key={entry.name}
                className="sftp-row"
                onDoubleClick={() => entry.isDir && load(joinPath(cwd, entry.name))}
              >
                {entry.isDir ? <FolderIcon size={15} /> : <FileIcon size={15} />}
                <span
                  className="sftp-name"
                  onClick={() => entry.isDir && load(joinPath(cwd, entry.name))}
                >
                  {entry.name}
                </span>
                <span className="sftp-size">{entry.isDir ? '—' : entry.size}</span>
                <span className="sftp-mtime">{entry.mtime}</span>
                <div className="sftp-row-actions">
                  {!entry.isDir && (
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
                    ✎
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
            ))}
            {entries.length === 0 && <div className="sftp-status">空目录</div>}
          </div>
        )}
      </div>

      {overwriteTarget && (
        <ConfirmModal
          title="覆盖远端文件"
          body={`远端已存在 "${baseName(overwriteTarget.remote)}"，覆盖它吗？`}
          confirmText="覆盖"
          danger
          onConfirm={() => {
            const t = overwriteTarget;
            setOverwriteTarget(null);
            void startTransfer('upload', t.local, t.remote);
          }}
          onCancel={() => setOverwriteTarget(null)}
        />
      )}
      {confirmDelete && (
        <ConfirmModal
          title={confirmDelete.isDir ? '删除目录' : '删除文件'}
          body={`确定删除 ${confirmDelete.isDir ? '目录' : '文件'} "${confirmDelete.name}" 吗？`}
          confirmText="删除"
          danger
          onConfirm={() => doDelete(confirmDelete)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
      {promptState && (
        <PromptModal
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
