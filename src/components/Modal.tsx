import { useEffect, type ReactNode } from 'react';
import { useT } from '../i18n';
import { XIcon } from './Icons';

interface Props {
  title: string;
  subtitle?: string;
  className?: string;
  onClose: () => void;
  children: ReactNode;
}

export default function Modal({ title, subtitle, className, onClose, children }: Props) {
  const t = useT();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      className="modal-overlay"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className={`modal${className ? ` ${className}` : ''}`}>
        <div className="modal-header">
          <div>
            <h2>{title}</h2>
            {subtitle && <p>{subtitle}</p>}
          </div>
          <button className="icon-btn" onClick={onClose} aria-label={t('common.close')}>
            <XIcon />
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}
