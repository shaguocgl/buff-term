import { useEffect } from 'react';
import Modal from './Modal';

interface Props {
  title: string;
  body: string;
  confirmText?: string;
  cancelText?: string;
  /** 可选第三按钮（如批量上传冲突时的「跳过冲突项」），不传则不渲染 */
  altText?: string;
  /** 危险操作：确认按钮显示为红色警示 */
  danger?: boolean;
  onConfirm: () => void;
  onAlt?: () => void;
  onCancel: () => void;
}

// 统一的确认弹窗（替代 window.confirm，风格与应用一致）
export default function ConfirmModal({
  title,
  body,
  confirmText = '确认',
  cancelText = '取消',
  altText,
  danger = false,
  onConfirm,
  onAlt,
  onCancel,
}: Props) {
  // 在捕获阶段拦截 Escape：只关闭本确认框，避免连带关闭底下的父级 Modal
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onCancel();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onCancel]);

  return (
    <Modal title={title} onClose={onCancel} className="modal-sm">
      <div className="confirm-body">
        <p className="confirm-text">{body}</p>
        <div className="tool-actions">
          <button
            className={`btn ${danger ? 'danger' : 'primary'} small`}
            onClick={onConfirm}
          >
            {confirmText}
          </button>
          {altText && onAlt && (
            <button className="btn ghost small" onClick={onAlt}>
              {altText}
            </button>
          )}
          <button className="btn ghost small" onClick={onCancel}>
            {cancelText}
          </button>
        </div>
      </div>
    </Modal>
  );
}
