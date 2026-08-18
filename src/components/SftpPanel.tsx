import { useCallback, useEffect, useState } from 'react';
import { open, save } from '@tauri-apps/plugin-dialog';
import {
  sftpDelete,
  sftpDownload,
  sftpList,
  sftpMkdir,
  sftpRename,
  sftpUpload,
} from '../api';
import type { Host } from '../types';
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

  const load = useCallback(
    async (path: string) => {
      setLoading(true);
      setError(null);
      try {
        const res = await sftpList(host, path);
        if (res.ok) {
          setEntries(parseListing(res.text));
          setCwd(path);
        } else {
          setError(res.text || '目录读取失败');
        }
      } catch (e) {
        setError(String(e));
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
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleUpload = async () => {
    const picked = await open({ multiple: false });
    if (!picked || typeof picked !== 'string') return;
    const remote = joinPath(cwd, baseName(picked));
    await run(() => sftpUpload(host, picked, remote), () => load(cwd));
  };

  const handleDownload = async (entry: Entry) => {
    const dest = await save({ defaultPath: entry.name });
    if (!dest) return;
    const remote = joinPath(cwd, entry.name);
    await run(() => sftpDownload(host, remote, dest), () => load(cwd));
  };

  const handleDelete = async (entry: Entry) => {
    if (!window.confirm(`确定删除 ${entry.isDir ? '目录' : '文件'} "${entry.name}" 吗？`)) return;
    const remote = joinPath(cwd, entry.name);
    await run(() => sftpDelete(host, remote), () => load(cwd));
  };

  const handleMkdir = async () => {
    const name = window.prompt('新建文件夹名称：');
    if (!name) return;
    await run(() => sftpMkdir(host, joinPath(cwd, name.trim())), () => load(cwd));
  };

  const handleRename = async (entry: Entry) => {
    const name = window.prompt('重命名为：', entry.name);
    if (!name || name === entry.name) return;
    await run(
      () =>
        sftpRename(host, joinPath(cwd, entry.name), joinPath(cwd, name.trim())),
      () => load(cwd),
    );
  };

  return (
    <aside className="sftp-panel" style={{ width: panelWidth }}>
      <div className="sftp-header">
        <div className="sftp-path" title={cwd}>
          {cwd}
        </div>
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
    </aside>
  );
}
