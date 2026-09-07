import { useEffect, useState } from 'react';
import type { HostKeyConfirmRequest } from '../types';
import { ShieldIcon } from './Icons';

interface Props {
  request: HostKeyConfirmRequest;
  onResolve: (trust: boolean) => void;
}

// SSH 首次连接主机指纹确认弹窗：未知主机（TOFU）需用户显式信任后才记录指纹并继续。
// 倒计时与后端确认等待/连接超时保持一致（60s），超时按拒绝处理。
const CONFIRM_TIMEOUT_SECS = 60;

export default function HostKeyModal({ request, onResolve }: Props) {
  const [remaining, setRemaining] = useState(CONFIRM_TIMEOUT_SECS);
  useEffect(() => {
    const timer = window.setInterval(() => {
      setRemaining((r) => Math.max(0, r - 1));
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);
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
              主机指纹确认
            </h2>
            <p>首次连接到该服务器，请核对主机密钥指纹</p>
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
          <p className="hostkey-tip">
            请与服务器管理员提供或此前记录的指纹核对。若一致，点「信任并连接」；
            若不一致，可能存在中间人风险，请选择「取消」。
          </p>
          <div className="approval-countdown">
            <div className="approval-countdown-bar">
              <span style={{ width: `${pct}%` }} />
            </div>
            <span className="approval-countdown-text">
              {timedOut
                ? '已超时，连接被拒绝'
                : `${remaining}s 后未确认将自动拒绝`}
            </span>
          </div>
          <div className="tool-actions">
            <button
              className="btn primary small"
              onClick={() => onResolve(true)}
              disabled={timedOut}
            >
              信任并连接
            </button>
            <button
              className="btn ghost small"
              onClick={() => onResolve(false)}
              disabled={timedOut}
            >
              取消
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
