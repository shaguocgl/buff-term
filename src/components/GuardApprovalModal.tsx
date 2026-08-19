import type { TerminalGuardApproval } from '../types';
import { ShieldIcon } from './Icons';

interface Props {
  request: TerminalGuardApproval;
  onResolve: (allow: boolean) => void;
}

export default function GuardApprovalModal({ request, onResolve }: Props) {
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
          <div className="tool-actions">
            <button className="btn primary small" onClick={() => onResolve(true)}>
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
