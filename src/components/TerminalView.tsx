import { useCallback, useEffect, useRef, useState } from 'react';
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
import {
  ActivityIcon,
  FolderIcon,
  PowerIcon,
  RefreshIcon,
  SparklesIcon,
} from './Icons';

interface Props {
  host: Host;
  tabKey: number;
  chatOpen: boolean;
  sftpOpen: boolean;
  monitorOpen: boolean;
  onToggleChat: () => void;
  onToggleSftp: () => void;
  onToggleMonitor: () => void;
  onOpened: (tabKey: number, sessionId: number) => void;
  onFailed: (tabKey: number, message: string) => void;
  onExited: (tabKey: number) => void;
  onDisconnect: (tabKey: number) => void;
}

function normalizeDims(dims: { cols: number; rows: number } | undefined) {
  // docker compose 动态进度单行约 100 字符，列宽不足会换行堆叠，
  // 因此最小列宽固定为 100（窄窗口下内容不换行，超出部分裁剪）
  const cols = dims && dims.cols >= 100 && dims.cols <= 400 ? dims.cols : 100;
  const rows = dims && dims.rows >= 10 && dims.rows <= 200 ? dims.rows : 30;
  return { cols, rows };
}

export default function TerminalView({
  host,
  tabKey,
  chatOpen,
  sftpOpen,
  monitorOpen,
  onToggleChat,
  onToggleSftp,
  onToggleMonitor,
  onOpened,
  onFailed,
  onExited,
  onDisconnect,
}: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sessionIdRef = useRef<number | null>(null);
  const inputBufRef = useRef<number[]>([]);
  const inputChainRef = useRef<Promise<void>>(Promise.resolve());
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const disposedRef = useRef(false);
  const onOpenedRef = useRef(onOpened);
  const onFailedRef = useRef(onFailed);
  const onExitedRef = useRef(onExited);
  const onDisconnectRef = useRef(onDisconnect);
  const [connecting, setConnecting] = useState(true);
  const [exited, setExited] = useState(false);

  onOpenedRef.current = onOpened;
  onFailedRef.current = onFailed;
  onExitedRef.current = onExited;
  onDisconnectRef.current = onDisconnect;

  const applyDims = useCallback(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    const { cols, rows } = normalizeDims(fit.proposeDimensions());
    // 保持 xterm 与 PTY 列宽一致，避免窄容器下 fit 把 term 缩到 100 以下
    if (term.cols !== cols || term.rows !== rows) {
      term.resize(cols, rows);
    }
    const sid = sessionIdRef.current;
    if (sid !== null) resizeSession(sid, cols, rows).catch(() => {});
  }, []);

  const connect = useCallback(async () => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    const { cols, rows } = normalizeDims(fit.proposeDimensions());
    const id = await openSession(host, cols, rows);
    if (disposedRef.current) {
      closeSession(id).catch(() => {});
      return;
    }
    sessionIdRef.current = id;
    setConnecting(false);
    setExited(false);
    onOpenedRef.current(tabKey, id);
    applyDims();
  }, [host, tabKey, applyDims]);

  const connectRef = useRef(connect);
  connectRef.current = connect;

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    let disposed = false;
    disposedRef.current = false;
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
    applyDims();
    term.focus();
    termRef.current = term;
    fitRef.current = fit;

    const flushInput = () => {
      const buf = inputBufRef.current;
      if (buf.length === 0) return;
      inputBufRef.current = [];
      const data = buf.splice(0);
      inputChainRef.current = inputChainRef.current
        .then(() => {
          const sid = sessionIdRef.current;
          if (sid === null) return;
          return sessionInput(sid, data);
        })
        .catch(() => {});
    };

    term.onData((data) => {
      const bytes = Array.from(new TextEncoder().encode(data));
      if (inputBufRef.current.length + bytes.length > 512) {
        flushInput();
      }
      inputBufRef.current.push(...bytes);
    });
    const inputTimer = window.setInterval(flushInput, 16);
    term.onResize(applyDims);

    const observer = new ResizeObserver(() => {
      fit.fit();
      applyDims();
    });
    observer.observe(container);

    (async () => {
      // 先注册事件监听，再发起连接，避免丢失服务器初始输出
      unData = await onTerminalData((id, data) => {
        if (id === sessionIdRef.current) term.write(data);
      });
      if (disposed) {
        unData();
        return;
      }
      unStatus = await onSessionStatus((id, status) => {
        if (id !== sessionIdRef.current) return;
        if (status === 'exited') {
          term.writeln(`\r\n\x1b[33m[会话已结束: exited]\x1b[0m`);
          term.options.disableStdin = true;
          setExited(true);
          onExitedRef.current(tabKey);
        } else if (status === 'closed') {
          term.writeln(`\r\n\x1b[33m[会话已结束: closed]\x1b[0m`);
          term.options.disableStdin = true;
          setExited(true);
        }
      });
      if (disposed) {
        unData();
        unStatus();
        return;
      }
      await connectRef.current();
    })().catch((e) => {
      if (disposed) return;
      const message = String(e);
      term.writeln(`\r\n\x1b[31m[连接失败: ${message}]\x1b[0m`);
      setConnecting(false);
      setExited(true);
      onFailedRef.current(tabKey, message);
    });

    return () => {
      disposed = true;
      disposedRef.current = true;
      window.clearInterval(inputTimer);
      flushInput();
      observer.disconnect();
      unData?.();
      unStatus?.();
      const sid = sessionIdRef.current;
      if (sid !== null) {
        closeSession(sid).catch(() => {});
      }
      sessionIdRef.current = null;
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [host, tabKey, applyDims]);

  const handleDisconnect = () => {
    const sid = sessionIdRef.current;
    if (sid !== null) {
      closeSession(sid).catch(() => {});
    }
    onDisconnectRef.current(tabKey);
  };

  const handleReconnect = () => {
    setConnecting(true);
    termRef.current?.reset();
    connectRef.current().catch((e) => {
      const message = String(e);
      termRef.current?.writeln(`\r\n\x1b[31m[重连失败: ${message}]\x1b[0m`);
      setConnecting(false);
      setExited(true);
      onFailedRef.current(tabKey, message);
    });
  };

  return (
    <div className="terminal-wrap">
      <div className="terminal-header">
        <div className="terminal-title-group">
          <span className="terminal-title-dot" />
          <span className="terminal-title">{host.name}</span>
          {connecting && <span className="terminal-status">连接中…</span>}
          {exited && !connecting && (
            <button className="btn reconnect" onClick={handleReconnect}>
              <RefreshIcon size={13} /> 重连
            </button>
          )}
        </div>
        <div className="terminal-header-actions">
          <button
            className={`btn ai-toggle${chatOpen ? ' active' : ''}`}
            onClick={onToggleChat}
          >
            <SparklesIcon size={14} /> AI
          </button>
          <button
            className={`btn ai-toggle${sftpOpen ? ' active' : ''}`}
            onClick={onToggleSftp}
          >
            <FolderIcon size={14} /> 文件
          </button>
          <button
            className={`btn ai-toggle${monitorOpen ? ' active' : ''}`}
            onClick={onToggleMonitor}
          >
            <ActivityIcon size={14} /> 监控
          </button>
          <button className="btn disconnect" onClick={handleDisconnect}>
            <PowerIcon size={14} /> 断开
          </button>
        </div>
      </div>
      <div className="terminal-body" ref={containerRef}>
        {connecting && (
          <div className="terminal-overlay">
            <div className="spinner" />
            <span>正在连接 {host.name}…</span>
          </div>
        )}
      </div>
    </div>
  );
}
