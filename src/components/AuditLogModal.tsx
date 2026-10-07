import { useCallback, useEffect, useMemo, useState } from 'react';
import { clearAuditLogs, listAuditLogs } from '../api';
import type { AuditLog } from '../types';
import { fmtError } from '../utils/errors';
import ConfirmModal from './ConfirmModal';
import Modal from './Modal';
import { ListIcon } from './Icons';
import type { ToastItem } from './Toast';

interface Props {
  onClose: () => void;
  /** 全局提示：清空等操作后给出反馈 */
  showToast: (kind: ToastItem['kind'], message: string) => void;
}

/** 单次拉取的审计条数（后端上限同为 1000） */
const AUDIT_LIMIT = 1000;

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
  ok: '正常',
};

/** 操作来源标签：区分 AI Agent / 终端防护 / MCP 服务 / 修复执行 / 文件编辑 */
const SOURCE_LABEL: Record<string, string> = {
  agent: 'AI Agent',
  guard: '终端防护',
  mcp: 'MCP 服务',
  remediation: '修复执行',
  sftp: '文件编辑',
};

/** 来源筛选项，value 为空串表示“全部” */
const SOURCE_FILTERS: { value: string; label: string }[] = [
  { value: '', label: '全部' },
  { value: 'agent', label: 'AI Agent' },
  { value: 'guard', label: '终端防护' },
  { value: 'mcp', label: 'MCP 服务' },
  { value: 'remediation', label: '修复执行' },
  { value: 'sftp', label: '文件编辑' },
];

function formatTime(ts: number) {
  return new Date(ts * 1000).toLocaleString('zh-CN', { hour12: false });
}

function formatDuration(ms: number | null) {
  if (ms === null) return '';
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}

export default function AuditLogModal({ onClose, showToast }: Props) {
  const [logs, setLogs] = useState<AuditLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [source, setSource] = useState('');
  const [confirmClear, setConfirmClear] = useState(false);
  const [clearing, setClearing] = useState(false);

  const load = useCallback(async () => {
    setLogs(await listAuditLogs(AUDIT_LIMIT));
  }, []);

  useEffect(() => {
    load()
      .catch((e) => setError(fmtError(e)))
      .finally(() => setLoading(false));
  }, [load]);

  const handleClear = async () => {
    setConfirmClear(false);
    setClearing(true);
    try {
      const removed = await clearAuditLogs();
      setLogs([]);
      showToast(
        'success',
        removed > 0 ? `已清空 ${removed} 条操作审计记录` : '没有可清空的操作审计记录',
      );
    } catch (e) {
      showToast('error', fmtError(e));
    } finally {
      setClearing(false);
    }
  };

  const visible = useMemo(
    () => (source ? logs.filter((log) => log.source === source) : logs),
    [logs, source],
  );

  return (
    <Modal
      title="操作审计"
      subtitle="AI Agent / 终端防护 / MCP 服务 / 修复执行 / 文件编辑的操作记录（最近 1000 条）"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="audit-modal">
        <div className="audit-toolbar">
          <div className="segmented audit-source-filter">
            {SOURCE_FILTERS.map((item) => (
              <button
                key={item.value}
                type="button"
                className={source === item.value ? 'seg active' : 'seg'}
                onClick={() => setSource(item.value)}
              >
                {item.label}
              </button>
            ))}
          </div>
          <button
            className="btn ghost small audit-clear"
            onClick={() => setConfirmClear(true)}
            disabled={clearing || logs.length === 0}
          >
            {clearing ? '清空中…' : '清空'}
          </button>
        </div>
        {loading && <div className="audit-empty">加载中…</div>}
        {!loading && error && <p className="error">{error}</p>}
        {!loading && !error && visible.length === 0 && (
          <div className="audit-empty">
            <ListIcon size={28} />
            <p>暂无操作审计记录</p>
            <span>执行 AI 工具、终端防护拦截、MCP 调用或修复操作后，记录会显示在这里</span>
          </div>
        )}
        {!loading && visible.length > 0 && (
          <div className="audit-list">
            {visible.map((log) => (
              <div key={log.id} className="audit-item">
                <div className="audit-item-top">
                  <span className="audit-time">{formatTime(log.ts)}</span>
                  <span className="audit-host">{log.host_label}</span>
                  <span className={`badge audit-source audit-source-${log.source}`}>
                    {SOURCE_LABEL[log.source] ?? log.source}
                  </span>
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

      {confirmClear && (
        <ConfirmModal
          title="清空操作审计"
          body="将删除全部操作审计记录（含 AI Agent、终端防护、MCP 服务与修复执行），此操作不可撤销。确定清空吗？"
          confirmText="清空"
          danger
          onConfirm={handleClear}
          onCancel={() => setConfirmClear(false)}
        />
      )}
    </Modal>
  );
}
