import { useEffect, useRef, useState } from 'react';
import type { KeyboardEvent } from 'react';
import { CheckIcon, ChevronDownIcon } from './Icons';

export interface SelectOption<T extends string> {
  value: T;
  label: string;
}

interface SelectProps<T extends string> {
  value: T;
  options: SelectOption<T>[];
  onChange: (value: T) => void;
  className?: string;
  ariaLabel?: string;
}

export default function Select<T extends string>({
  value,
  options,
  onChange,
  className,
  ariaLabel,
}: SelectProps<T>) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);
  const optionRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const focusTimerRef = useRef<number | null>(null);

  const selected = options.find((o) => o.value === value);

  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('mousedown', onMouseDown);
    return () => document.removeEventListener('mousedown', onMouseDown);
  }, [open]);

  // 卸载时清理聚焦定时器，避免对已卸载组件 setState
  useEffect(
    () => () => {
      if (focusTimerRef.current !== null) {
        window.clearTimeout(focusTimerRef.current);
      }
    },
    [],
  );

  const focusOption = (idx: number) => {
    optionRefs.current[idx]?.focus();
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLElement>) => {
    if (!open) {
      if (e.key === 'Enter' || e.key === ' ' || e.key === 'ArrowDown') {
        e.preventDefault();
        setOpen(true);
        focusTimerRef.current = window.setTimeout(
          () => optionRefs.current[0]?.focus(),
          0,
        );
      }
      return;
    }
    const current = optionRefs.current.findIndex(
      (el) => el === document.activeElement,
    );
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      focusOption(Math.min(current + 1, options.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      focusOption(Math.max(current - 1, 0));
    } else if (e.key === 'Escape') {
      setOpen(false);
      buttonRef.current?.focus();
    }
  };

  return (
    <div ref={rootRef} className={`select-wrap${className ? ` ${className}` : ''}`}>
      <button
        ref={buttonRef}
        type="button"
        className={`select-btn${open ? ' open' : ''}`}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={handleKeyDown}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
      >
        <span className="select-label">{selected?.label ?? value}</span>
        <ChevronDownIcon size={14} />
      </button>
      {open && (
        <div className="select-menu" role="listbox">
          {options.map((opt, idx) => (
            <button
              key={opt.value}
              ref={(el) => {
                optionRefs.current[idx] = el;
              }}
              type="button"
              role="option"
              aria-selected={opt.value === value}
              className={`select-option${opt.value === value ? ' selected' : ''}`}
              onClick={() => {
                onChange(opt.value);
                setOpen(false);
                buttonRef.current?.focus();
              }}
              onMouseDown={(e) => e.preventDefault()}
              onKeyDown={handleKeyDown}
            >
              <span>{opt.label}</span>
              {opt.value === value && <CheckIcon size={13} />}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
