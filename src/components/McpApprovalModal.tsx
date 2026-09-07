import { useEffect, useState } from 'react';
import type { McpApprovalRequest } from '../types';

interface Props {
  request: McpApprovalRequest;
  onResolve: (allow: boolean) => void;
}

const DEFAULT_TIMEOUT = 600;

// 外部 AI（MCP）命令审批弹窗：带倒计时，超时后端按拒绝处理
export default function McpApprovalModal({ request, onResolve }: Props) {
  const timeoutSecs = request.timeout_secs ?? DEFAULT_TIMEOUT;
  const [remaining, setRemaining] = useState(timeoutSecs);
  useEffect(() => {
    const timer = window.setInterval(() => {
      setRemaining((r) => Math.max(0, r - 1));
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);
  const timedOut = remaining <= 0;
  const pct = timeoutSecs > 0 ? Math.max(0, (remaining / timeoutSecs) * 100) : 0;

  return (
    <div className="modal-overlay">
      <div className="modal modal-sm">
        <div className="modal-header">
          <div>
            <h2>外部 AI 请求执行命令</h2>
            <p>来自 MCP 服务的外部 AI 调用</p>
          </div>
        </div>
        <div className="mcp-approval-body">
          <p className="mcp-approval-host">{request.host_label}</p>
          <pre className="mcp-approval-command">{request.command}</pre>
          <div className="approval-countdown">
            <div className="approval-countdown-bar">
              <span style={{ width: `${pct}%` }} />
            </div>
            <span className="approval-countdown-text">
              {timedOut
                ? '已超时，按拒绝处理'
                : `${remaining}s 后未确认将自动拒绝`}
            </span>
          </div>
          <div className="tool-actions">
            <button
              className="btn primary small"
              onClick={() => onResolve(true)}
              disabled={timedOut}
            >
              批准执行
            </button>
            <button className="btn ghost small" onClick={() => onResolve(false)}>
              拒绝
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
