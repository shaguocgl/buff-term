import { useEffect, useMemo, useRef, useState } from 'react';
import { useT, type DictKey } from '../i18n';
import type { Host } from '../types';

interface Props {
  hosts: Host[];
  onConnect: (host: Host) => void;
  /** 动作标识：new-host / import-ssh / ai-config / logs / mcp / guard / alerts */
  onAction: (action: string) => void;
  onClose: () => void;
}

interface PaletteItem {
  key: string;
  title: string;
  subtitle: string;
  kind: 'host' | 'action';
  host?: Host;
  action?: string;
}

const ACTIONS: { key: string; labelKey: DictKey }[] = [
  { key: 'new-host', labelKey: 'palette.action.newHost' },
  { key: 'import-ssh', labelKey: 'palette.action.importSsh' },
  { key: 'ai-config', labelKey: 'palette.action.aiConfig' },
  { key: 'logs', labelKey: 'palette.action.logs' },
  { key: 'mcp', labelKey: 'palette.action.mcp' },
  { key: 'guard', labelKey: 'palette.action.guard' },
  { key: 'alerts', labelKey: 'palette.action.alerts' },
];

// Cmd+K 快速连接 / 命令面板：主机与常用动作的模糊搜索入口
export default function CommandPalette({
  hosts,
  onConnect,
  onAction,
  onClose,
}: Props) {
  const t = useT();
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const items = useMemo<PaletteItem[]>(() => {
    const q = query.trim().toLowerCase();
    const match = (text: string) => !q || text.toLowerCase().includes(q);
    const hostItems: PaletteItem[] = hosts
      .filter(
        (h) =>
          match(h.name) ||
          match(h.address) ||
          match(h.notes ?? ''),
      )
      .map((h) => ({
        key: `host-${h.id}`,
        title: h.name,
        subtitle: `${h.username}@${h.address}:${h.port}`,
        kind: 'host' as const,
        host: h,
      }));
    const actionItems: PaletteItem[] = ACTIONS.map((a) => ({
      key: a.key,
      title: t(a.labelKey),
      subtitle: t('palette.action'),
      kind: 'action' as const,
      action: a.key,
    })).filter((a) => match(a.title));
    return [...hostItems, ...actionItems];
  }, [hosts, query, t]);

  useEffect(() => {
    setActive(0);
  }, [query]);

  // 列表缩短（主机删除/过滤）时避免选中项越界
  useEffect(() => {
    if (active >= items.length && items.length > 0) {
      setActive(Math.max(0, items.length - 1));
    }
  }, [items.length, active]);

  useEffect(() => {
    const el = listRef.current?.querySelector(`[data-idx="${active}"]`);
    el?.scrollIntoView({ block: 'nearest' });
  }, [active]);

  const run = (item: PaletteItem) => {
    onClose();
    if (item.kind === 'host' && item.host) onConnect(item.host);
    else if (item.action) onAction(item.action);
  };

  return (
    <div
      className="palette-overlay"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="palette">
        <input
          ref={inputRef}
          className="palette-input"
          value={query}
          placeholder={t('palette.placeholder')}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') {
              e.preventDefault();
              if (items.length > 0) {
                setActive((a) => Math.min(a + 1, items.length - 1));
              }
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              if (items.length > 0) {
                setActive((a) => Math.max(a - 1, 0));
              }
            } else if (e.key === 'Enter') {
              e.preventDefault();
              if (items[active]) run(items[active]);
            } else if (e.key === 'Escape') {
              e.preventDefault();
              onClose();
            }
          }}
        />
        <div className="palette-list" ref={listRef}>
          {items.length === 0 && (
            <div className="palette-empty">{t('palette.empty')}</div>
          )}
          {items.map((item, idx) => (
            <div
              key={item.key}
              data-idx={idx}
              className={`palette-item${idx === active ? ' active' : ''}`}
              onMouseEnter={() => setActive(idx)}
              onClick={() => run(item)}
            >
              <span className={`palette-item-icon ${item.kind}`}>
                {item.kind === 'host' ? '⇥' : '⚙'}
              </span>
              <span className="palette-item-title">{item.title}</span>
              <span className="palette-item-sub">{item.subtitle}</span>
            </div>
          ))}
        </div>
        <div className="palette-footer">{t('palette.footer')}</div>
      </div>
    </div>
  );
}
