import { useEffect, useState } from 'react';
import type { TerminalGuardApproval } from '../types';
import { ShieldIcon } from './Icons';

interface Props {
  request: TerminalGuardApproval;
  onResolve: (allow: boolean) => void;
}

// 高危命令审批弹窗：展示倒计时（超时后端按拒绝处理），父组件在超时后负责关闭弹窗
export default function GuardApprovalModal({ request, onResolve }: Props) {
  const [remaining, setRemaining] = useState(request.timeout_secs);
  useEffect(() => {
    const timer = window.setInterval(() => {
      setRemaining((r) => Math.max(0, r - 1));
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);
  const timedOut = remaining <= 0;
  const pct =
    request.timeout_secs > 0
      ? Math.max(0, (remaining / request.timeout_secs) * 100)
      : 0;

  return (
    <div className="modal-overlay">
      <div className="modal modal-sm">
        <div className="modal-header">
          <div>
            <h2>
              <span className="guard-approval-icon">
                <ShieldIcon size={15} />
              </span>{' '}
              高危命令确认
            </h2>
            <p>终端输入的命令命中危险规则，执行前需要你确认</p>
          </div>
        </div>
        <div className="mcp-approval-body">
          <p className="mcp-approval-host">{request.host_label}</p>
          <pre className="mcp-approval-command">{request.command}</pre>
          {request.matched_patterns.length > 0 && (
            <p className="guard-approval-matched">
              命中规则：
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
              {timedOut ? '已超时，按拒绝处理' : `${remaining}s 后未确认将自动拒绝`}
            </span>
          </div>
          <div className="tool-actions">
            <button
              className="btn primary small"
              onClick={() => onResolve(true)}
              disabled={timedOut}
            >
              确认执行
            </button>
            <button className="btn ghost small" onClick={() => onResolve(false)}>
              取消
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
