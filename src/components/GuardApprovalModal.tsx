import { useEffect, useState } from 'react';
import { useT } from '../i18n';
import type { TerminalGuardApproval } from '../types';
import { ShieldIcon } from './Icons';

interface Props {
  request: TerminalGuardApproval;
  deadline: number;
  onResolve: (allow: boolean) => void;
}

// 高危命令审批弹窗：展示倒计时（超时后端按拒绝处理），父组件在超时后负责关闭弹窗
export default function GuardApprovalModal({
  request,
  deadline,
  onResolve,
}: Props) {
  const t = useT();
  const timeoutSecs = Math.max(10, request.timeout_secs);
  const [remaining, setRemaining] = useState(() =>
    Math.max(0, Math.ceil((deadline - Date.now()) / 1000)),
  );
  useEffect(() => {
    const timer = window.setInterval(() => {
      setRemaining(Math.max(0, Math.ceil((deadline - Date.now()) / 1000)));
    }, 300);
    return () => window.clearInterval(timer);
  }, [deadline]);
  const timedOut = remaining <= 0;
  const pct =
    timeoutSecs > 0 ? Math.max(0, (remaining / timeoutSecs) * 100) : 0;

  return (
    <div className="modal-overlay">
      <div className="modal modal-sm">
        <div className="modal-header">
          <div>
            <h2>
              <span className="guard-approval-icon">
                <ShieldIcon size={15} />
              </span>{' '}
              {t('guardApproval.title')}
            </h2>
            <p>{t('guardApproval.subtitle')}</p>
          </div>
        </div>
        <div className="mcp-approval-body">
          <p className="mcp-approval-host">{request.host_label}</p>
          <pre className="mcp-approval-command">{request.command}</pre>
          {request.matched_patterns.length > 0 && (
            <p className="guard-approval-matched">
              {t('guardApproval.matched')}
              {request.matched_patterns.map((p) => (
                <code key={p}>{p}</code>
              ))}
            </p>
          )}
          <div className="approval-countdown">
            <div className="approval-countdown-bar">
              <span style={{ width: `${pct}%` }} />
            </div>
            <span className="approval-countdown-text">
              {timedOut
                ? t('guardApproval.timedOut')
                : t('guardApproval.countdown', { n: remaining })}
            </span>
          </div>
          <div className="tool-actions">
            <button
              className="btn primary small"
              onClick={() => onResolve(true)}
              disabled={timedOut}
            >
              {t('guardApproval.run')}
            </button>
            <button className="btn ghost small" onClick={() => onResolve(false)}>
              {t('common.cancel')}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
