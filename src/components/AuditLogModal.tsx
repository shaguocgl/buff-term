import { useCallback, useEffect, useMemo, useState } from 'react';
import { clearAuditLogs, listAuditLogs } from '../api';
import { useI18n, type DictKey } from '../i18n';
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

const APPROVAL_LABEL: Record<string, DictKey> = {
  auto: 'audit.approval.auto',
  approved: 'audit.approval.approved',
  denied: 'audit.approval.denied',
  timeout: 'audit.approval.timeout',
};

const STATUS_LABEL: Record<string, DictKey> = {
  executed: 'audit.status.executed',
  denied: 'audit.status.denied',
  error: 'audit.status.error',
  ok: 'audit.status.ok',
};

/** 操作来源标签：区分 AI Agent / 终端防护 / MCP 服务 / 修复执行 / 文件编辑 */
const SOURCE_LABEL: Record<string, DictKey> = {
  agent: 'audit.source.agent',
  guard: 'audit.source.guard',
  mcp: 'audit.source.mcp',
  remediation: 'audit.source.remediation',
  sftp: 'audit.source.sftp',
};

/** 来源筛选项，value 为空串表示“全部” */
const SOURCE_FILTERS: { value: string; labelKey: DictKey }[] = [
  { value: '', labelKey: 'common.all' },
  { value: 'agent', labelKey: 'audit.source.agent' },
  { value: 'guard', labelKey: 'audit.source.guard' },
  { value: 'mcp', labelKey: 'audit.source.mcp' },
  { value: 'remediation', labelKey: 'audit.source.remediation' },
  { value: 'sftp', labelKey: 'audit.source.sftp' },
];

function formatTime(ts: number, locale: string) {
  return new Date(ts * 1000).toLocaleString(locale, { hour12: false });
}

function formatDuration(ms: number | null) {
  if (ms === null) return '';
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}

export default function AuditLogModal({ onClose, showToast }: Props) {
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
  /** 取标签文案：未知值原样展示（后端可能新增来源 / 状态） */
  const labelOf = (map: Record<string, DictKey>, key: string) =>
    map[key] ? t(map[key]) : key;
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
        removed > 0 ? t('audit.cleared', { n: removed }) : t('audit.nothingToClear'),
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
      title={t('audit.title')}
      subtitle={t('audit.subtitle')}
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
                {t(item.labelKey)}
              </button>
            ))}
          </div>
          <button
            className="btn ghost small audit-clear"
            onClick={() => setConfirmClear(true)}
            disabled={clearing || logs.length === 0}
          >
            {clearing ? t('audit.clearing') : t('audit.clear')}
          </button>
        </div>
        {loading && <div className="audit-empty">{t('common.loading')}</div>}
        {!loading && error && <p className="error">{error}</p>}
        {!loading && !error && visible.length === 0 && (
          <div className="audit-empty">
            <ListIcon size={28} />
            <p>{t('audit.emptyTitle')}</p>
            <span>{t('audit.emptyHint')}</span>
          </div>
        )}
        {!loading && visible.length > 0 && (
          <div className="audit-list">
            {visible.map((log) => (
              <div key={log.id} className="audit-item">
                <div className="audit-item-top">
                  <span className="audit-time">{formatTime(log.ts, locale)}</span>
                  <span className="audit-host">{log.host_label}</span>
                  <span className={`badge audit-source audit-source-${log.source}`}>
                    {labelOf(SOURCE_LABEL, log.source)}
                  </span>
                  <span className="audit-tool">{log.tool_name}</span>
                  <span className={`badge audit-approval audit-${log.approval}`}>
                    {labelOf(APPROVAL_LABEL, log.approval)}
                  </span>
                  <span className={`badge audit-status audit-${log.status}`}>
                    {labelOf(STATUS_LABEL, log.status)}
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
          title={t('audit.clearTitle')}
          body={t('audit.clearBody')}
          confirmText={t('audit.clear')}
          danger
          onConfirm={handleClear}
          onCancel={() => setConfirmClear(false)}
        />
      )}
    </Modal>
  );
}
