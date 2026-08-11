import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import {
  closeSession,
  onSessionStatus,
  onTerminalData,
  openSession,
  resizeSession,
  sessionInput,
} from '../api';
import type { Host } from '../types';
import { PowerIcon, SparklesIcon } from './Icons';

interface Props {
  host: Host;
  chatOpen: boolean;
  onToggleChat: () => void;
  onOpened: (sessionId: number) => void;
  onFailed: () => void;
  onExit: () => void;
}

export default function TerminalView({
  host,
  chatOpen,
  onToggleChat,
  onOpened,
  onFailed,
  onExit,
}: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sessionIdRef = useRef<number | null>(null);
  const onOpenedRef = useRef(onOpened);
  const onFailedRef = useRef(onFailed);
  const [connecting, setConnecting] = useState(true);

  onOpenedRef.current = onOpened;
  onFailedRef.current = onFailed;

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    let cancelled = false;
    let unData: (() => void) | undefined;
    let unStatus: (() => void) | undefined;

    const term = new Terminal({
      cursorBlink: true,
      fontFamily: 'Menlo, Monaco, "Cascadia Mono", Consolas, monospace',
      fontSize: 14,
      scrollback: 5000,
      theme: { background: '#0f1115', foreground: '#d6dee8' },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(container);
    fit.fit();
    term.focus();

    term.onData((data) => {
      const sid = sessionIdRef.current;
      if (sid === null) return;
      const bytes = new TextEncoder().encode(data);
      sessionInput(sid, Array.from(bytes)).catch(() => {});
    });

    const sendSize = () => {
      const sid = sessionIdRef.current;
      if (sid === null) return;
      const dims = fit.proposeDimensions();
      if (dims) resizeSession(sid, dims.cols, dims.rows).catch(() => {});
    };
    term.onResize(sendSize);

    const observer = new ResizeObserver(() => {
      fit.fit();
      sendSize();
    });
    observer.observe(container);

    (async () => {
      // 先注册事件监听，再发起连接，避免丢失服务器初始输出
      unData = await onTerminalData((id, data) => {
        if (id === sessionIdRef.current) term.write(data);
      });
      if (cancelled) {
        unData();
        return;
      }
      unStatus = await onSessionStatus((id, status) => {
        if (id !== sessionIdRef.current) return;
        if (status === 'exited' || status === 'closed') {
          term.writeln(`\r\n\x1b[33m[会话已结束: ${status}]\x1b[0m`);
          term.options.disableStdin = true;
        }
      });
      if (cancelled) {
        unData();
        unStatus();
        return;
      }

      const dims = fit.proposeDimensions() ?? { cols: 100, rows: 30 };
      const id = await openSession(host, dims.cols, dims.rows);
      if (cancelled) {
        closeSession(id).catch(() => {});
        unData();
        unStatus();
        return;
      }
      sessionIdRef.current = id;
      setConnecting(false);
      onOpenedRef.current(id);
      sendSize();
    })().catch((e) => {
      if (cancelled) return;
      term.writeln(`\r\n\x1b[31m[连接失败: ${e}]\x1b[0m`);
      setConnecting(false);
      onFailedRef.current();
    });

    return () => {
      cancelled = true;
      observer.disconnect();
      unData?.();
      unStatus?.();
      const sid = sessionIdRef.current;
      if (sid !== null) {
        closeSession(sid).catch(() => {});
      }
      sessionIdRef.current = null;
      term.dispose();
    };
  }, [host]);

  const handleDisconnect = () => {
    const sid = sessionIdRef.current;
    if (sid !== null) {
      closeSession(sid).catch(() => {});
    }
    onExit();
  };

  return (
    <div className="terminal-wrap">
      <div className="terminal-header">
        <div className="terminal-title-group">
          <span className="terminal-title-dot" />
          <span className="terminal-title">{host.name}</span>
          {connecting && <span className="terminal-status">连接中…</span>}
        </div>
        <div className="terminal-header-actions">
          <button
            className={`btn ai-toggle${chatOpen ? ' active' : ''}`}
            onClick={onToggleChat}
          >
            <SparklesIcon size={14} /> AI
          </button>
          <button className="btn disconnect" onClick={handleDisconnect}>
            <PowerIcon size={14} /> 断开
          </button>
        </div>
      </div>
      <div className="terminal-body" ref={containerRef} />
    </div>
  );
}
