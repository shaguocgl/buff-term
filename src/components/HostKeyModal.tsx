import { useEffect, useState } from 'react';
import { useT } from '../i18n';
import type { HostKeyConfirmRequest } from '../types';
import { ShieldIcon } from './Icons';

interface Props {
  request: HostKeyConfirmRequest;
  deadline: number;
  onResolve: (trust: boolean) => void;
}

// SSH 首次连接主机指纹确认弹窗：未知主机（TOFU）需用户显式信任后才记录指纹并继续。
// 倒计时与后端确认等待/连接超时保持一致（60s），超时按拒绝处理。
const CONFIRM_TIMEOUT_SECS = 60;

export default function HostKeyModal({ request, deadline, onResolve }: Props) {
  const t = useT();
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
  const pct = Math.max(0, (remaining / CONFIRM_TIMEOUT_SECS) * 100);

  return (
    <div className="modal-overlay">
      <div className="modal modal-sm">
        <div className="modal-header">
          <div>
            <h2>
              <span className="guard-approval-icon">
                <ShieldIcon size={15} />
              </span>{' '}
              {t('hostKey.title')}
            </h2>
            <p>{t('hostKey.subtitle')}</p>
          </div>
        </div>
        <div className="mcp-approval-body">
          <p className="mcp-approval-host">
            {request.host}:{request.port}
          </p>
          <div className="hostkey-fingerprint">
            <span className="hostkey-type">{request.key_type}</span>
            <code>{request.fingerprint}</code>
          </div>
          <p className="hostkey-tip">{t('hostKey.tip')}</p>
          <div className="approval-countdown">
            <div className="approval-countdown-bar">
              <span style={{ width: `${pct}%` }} />
            </div>
            <span className="approval-countdown-text">
              {timedOut
                ? t('hostKey.timedOut')
                : t('guardApproval.countdown', { n: remaining })}
            </span>
          </div>
          <div className="tool-actions">
            <button
              className="btn primary small"
              onClick={() => onResolve(true)}
              disabled={timedOut}
            >
              {t('hostKey.trust')}
            </button>
            <button
              className="btn ghost small"
              onClick={() => onResolve(false)}
              disabled={timedOut}
            >
              {t('common.cancel')}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
