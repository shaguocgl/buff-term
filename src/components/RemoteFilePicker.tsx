import { useCallback, useEffect, useRef, useState } from 'react';
import { sftpList } from '../api';
import type { SftpEntry } from '../types';
import { fmtError } from '../utils/errors';
import { baseName, formatBytes, formatMtime, joinPath, parentPath } from '../utils/remote';
import { FileIcon, FolderIcon, RefreshIcon } from './Icons';
import Modal from './Modal';

interface Props {
  hostId: string;
  /** 已引用的路径，列表中标记为已选 */
  selectedPaths: string[];
  onSelect: (path: string) => void;
  onClose: () => void;
}

// 远端文件选择弹窗：只读浏览服务器目录，选中文件后把路径加入 AI 引用。
export default function RemoteFilePicker({
  hostId,
  selectedPaths,
  onSelect,
  onClose,
}: Props) {
  const [cwd, setCwd] = useState('/');
  const [entries, setEntries] = useState<SftpEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [pathInput, setPathInput] = useState('/');
  const [picked, setPicked] = useState<string | null>(null);
  // 目录请求序号：快速切换目录时丢弃过期响应，避免旧目录覆盖新目录
  const loadSeqRef = useRef(0);

  const load = useCallback(
    async (path: string) => {
      const seq = ++loadSeqRef.current;
      setLoading(true);
      setError(null);
      try {
        const list = await sftpList(hostId, path);
        if (seq !== loadSeqRef.current) return;
        setEntries(list);
        setCwd(path);
        setPathInput(path);
        setPicked(null);
      } catch (e) {
        if (seq !== loadSeqRef.current) return;
        setError(fmtError(e));
      } finally {
        if (seq === loadSeqRef.current) setLoading(false);
      }
    },
    [hostId],
  );

  useEffect(() => {
    load('/');
  }, [load]);

  const enterDir = (name: string) => load(joinPath(cwd, name));

  const confirm = () => {
    if (!picked) return;
    onSelect(picked);
    onClose();
  };

  return (
    <Modal title="选择服务器文件" subtitle={cwd} onClose={onClose} className="modal-filepicker">
      <div className="filepicker-bar">
        <input
          className="filepicker-path"
          value={pathInput}
          title={cwd}
          spellCheck={false}
          onChange={(e) => setPathInput(e.target.value)}
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
        <button
          className="icon-btn"
          title="刷新"
          onClick={() => load(cwd)}
          disabled={loading}
        >
          <RefreshIcon size={14} />
        </button>
      </div>

      <div className="filepicker-body">
        {loading && <div className="sftp-status">加载中…</div>}
        {!loading && error && <div className="sftp-status err">{error}</div>}
        {!loading && !error && (
          <div className="sftp-list">
            {cwd !== '/' && (
              <div className="sftp-row" onClick={() => load(parentPath(cwd))}>
                <FolderIcon size={15} />
                <span className="sftp-name">..</span>
              </div>
            )}
            {entries.map((entry) => {
              const full = joinPath(cwd, entry.name);
              const already = !entry.is_dir && selectedPaths.includes(full);
              const active = !entry.is_dir && picked === full;
              return (
                <div
                  key={entry.name}
                  className={`sftp-row${active ? ' active' : ''}`}
                  onDoubleClick={() => {
                    if (entry.is_dir) return;
                    setPicked(full);
                    onSelect(full);
                    onClose();
                  }}
                  onClick={() => {
                    if (entry.is_dir) enterDir(entry.name);
                    else setPicked(full);
                  }}
                >
                  {entry.is_dir ? <FolderIcon size={15} /> : <FileIcon size={15} />}
                  <span className="sftp-name">
                    {entry.name}
                    {already && <span className="filepicker-badge">已引用</span>}
                  </span>
                  <span className="sftp-size">
                    {entry.is_dir ? '—' : formatBytes(entry.size)}
                  </span>
                  <span className="sftp-mtime">{formatMtime(entry.mtime)}</span>
                </div>
              );
            })}
            {entries.length === 0 && <div className="sftp-status">空目录</div>}
          </div>
        )}
      </div>

      <div className="filepicker-foot">
        <span className="filepicker-picked" title={picked ?? ''}>
          {picked ? `已选择：${baseName(picked)}` : '点击选择一个文件'}
        </span>
        <div className="tool-actions">
          <button className="btn primary small" onClick={confirm} disabled={!picked}>
            添加引用
          </button>
          <button className="btn ghost small" onClick={onClose}>
            取消
          </button>
        </div>
      </div>
    </Modal>
  );
}
