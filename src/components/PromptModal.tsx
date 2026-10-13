import { useEffect, useState } from 'react';
import { useT } from '../i18n';
import Modal from './Modal';

interface Props {
  title: string;
  label: string;
  initialValue?: string;
  placeholder?: string;
  confirmText?: string;
  onOk: (value: string) => void;
  onCancel: () => void;
}

// 统一的文本输入弹窗（替代 window.prompt），自动聚焦、Enter 提交
export default function PromptModal({
  title,
  label,
  initialValue = '',
  placeholder,
  confirmText,
  onOk,
  onCancel,
}: Props) {
  const t = useT();
  const confirmLabel = confirmText ?? t('common.ok');
  const [value, setValue] = useState(initialValue);

  // 在捕获阶段拦截 Escape：只关闭本输入框，避免连带关闭底下的父级 Modal
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

  const submit = () => {
    const v = value.trim();
    if (!v) return;
    onOk(v);
  };

  return (
    <Modal title={title} onClose={onCancel} className="modal-sm">
      <div className="confirm-body">
        <label className="confirm-field">
          <span className="confirm-label">{label}</span>
          <input
            autoFocus
            value={value}
            placeholder={placeholder}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.nativeEvent.isComposing) submit();
            }}
          />
        </label>
        <div className="tool-actions">
          <button className="btn primary small" onClick={submit}>
            {confirmLabel}
          </button>
          <button className="btn ghost small" onClick={onCancel}>
            {t('common.cancel')}
          </button>
        </div>
      </div>
    </Modal>
  );
}
