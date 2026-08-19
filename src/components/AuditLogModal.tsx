import { useCallback, useEffect, useState } from 'react';
import { listAuditLogs } from '../api';
import type { AuditLog } from '../types';
import Modal from './Modal';
import { ListIcon } from './Icons';

interface Props {
  onClose: () => void;
}

const APPROVAL_LABEL: Record<string, string> = {
  auto: '自动',
  approved: '已批准',
  denied: '已拒绝',
  timeout: '超时拒绝',
};

const STATUS_LABEL: Record<string, string> = {
  executed: '已执行',
  denied: '已拒绝',
  error: '出错',
};

function formatTime(ts: number) {
  return new Date(ts * 1000).toLocaleString('zh-CN', { hour12: false });
}

function formatDuration(ms: number | null) {
  if (ms === null) return '';
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}

export default function AuditLogModal({ onClose }: Props) {
  const [logs, setLogs] = useState<AuditLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLogs(await listAuditLogs(100));
  }, []);

  useEffect(() => {
    load()
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [load]);

  return (
    <Modal
      title="操作日志"
      subtitle="AI 工具与终端命令操作记录（最近 100 条）"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="audit-modal">
        {loading && <div className="audit-empty">加载中…</div>}
        {!loading && error && <p className="error">{error}</p>}
        {!loading && !error && logs.length === 0 && (
          <div className="audit-empty">
            <ListIcon size={28} />
            <p>暂无操作日志</p>
            <span>让 AI 执行过工具调用后，记录会显示在这里</span>
          </div>
        )}
        {!loading && logs.length > 0 && (
          <div className="audit-list">
            {logs.map((log) => (
              <div key={log.id} className="audit-item">
                <div className="audit-item-top">
                  <span className="audit-time">{formatTime(log.ts)}</span>
                  <span className="audit-host">{log.host_label}</span>
                  <span className="audit-tool">{log.tool_name}</span>
                  <span className={`badge audit-approval audit-${log.approval}`}>
                    {APPROVAL_LABEL[log.approval] ?? log.approval}
                  </span>
                  <span className={`badge audit-status audit-${log.status}`}>
                    {STATUS_LABEL[log.status] ?? log.status}
                  </span>
                  {log.duration_ms !== null && (
                    <span className="audit-duration">{formatDuration(log.duration_ms)}</span>
                  )}
                </div>
                <code className="audit-command">{log.summary}</code>
                {log.result && <pre className="audit-result">{log.result}</pre>}
              </div>
            ))}
          </div>
        )}
      </div>
    </Modal>
  );
}
