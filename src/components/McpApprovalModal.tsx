import type { McpApprovalRequest } from '../types';

interface Props {
  request: McpApprovalRequest;
  onResolve: (allow: boolean) => void;
}

export default function McpApprovalModal({ request, onResolve }: Props) {
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
          <div className="tool-actions">
            <button className="btn primary small" onClick={() => onResolve(true)}>
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
