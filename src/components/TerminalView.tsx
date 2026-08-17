import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
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
  InspectIcon,
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
  inspectionOpen: boolean;
  onToggleChat: () => void;
  onToggleSftp: () => void;
  onToggleMonitor: () => void;
  onToggleInspection: () => void;
  onOpened: (tabKey: number, sessionId: number) => void;
  onFailed: (tabKey: number, message: string) => void;
  onExited: (tabKey: number) => void;
  onDisconnect: (tabKey: number) => void;
}

function normalizeDims(dims: { cols: number; rows: number } | undefined) {
  // 按实际可视区域计算列宽，避免强制 100 列导致右侧被裁剪、输入字符“看不到”
  const cols = Math.max(2, Math.min(400, dims?.cols ?? 100));
  const rows = Math.max(10, Math.min(200, dims?.rows ?? 30));
  return { cols, rows };
}

export default function TerminalView({
  host,
  tabKey,
  chatOpen,
  sftpOpen,
  monitorOpen,
  inspectionOpen,
  onToggleChat,
  onToggleSftp,
  onToggleMonitor,
  onToggleInspection,
  onOpened,
  onFailed,
  onExited,
  onDisconnect,
}: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sessionIdRef = useRef<number | null>(null);
  const pendingInputRef = useRef<number[]>([]);
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
    // 保持 xterm 与 PTY 列宽一致，并让终端跟随实际可视区域
    if (term.cols !== cols || term.rows !== rows) {
      term.resize(cols, rows);
    }
    const sid = sessionIdRef.current;
    if (sid !== null) resizeSession(sid, cols, rows).catch(() => {});
  }, []);

  const sendInput = useCallback((data: number[]) => {
    inputChainRef.current = inputChainRef.current
      .then(() => {
        const sid = sessionIdRef.current;
        if (sid === null) {
          // 连接建立前用户已经开始输入时，先放入待发送队列，连接成功后统一补发
          pendingInputRef.current.push(...data);
          return;
        }
        return sessionInput(sid, data);
      })
      .catch(() => {});
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
    const pending = pendingInputRef.current.splice(0);
    if (pending.length > 0) {
      sendInput(pending);
    }
    applyDims();
  }, [host, tabKey, applyDims, sendInput]);

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

    // 开发模式下 WKWebView 的 xterm.js input/keypress 路径可能丢字，
    // 普通可打印字符直接从原生 keydown 截获发送；发布构建中 xterm 输入正常，
    // 若再走该路径会与 onData 重复发送导致字符双显，因此仅开发模式启用。
    const handleKeyDownCapture = (event: KeyboardEvent) => {
      if (
        event.isComposing ||
        event.key === 'Process' ||
        event.ctrlKey ||
        event.metaKey ||
        event.altKey
      ) {
        return;
      }
      if (event.key.length === 1) {
        event.preventDefault();
        event.stopPropagation();
        sendInput(Array.from(new TextEncoder().encode(event.key)));
      }
    };
    if (import.meta.env.DEV) {
      container.addEventListener('keydown', handleKeyDownCapture, true);
    }

    term.onData((data) => {
      sendInput(Array.from(new TextEncoder().encode(data)));
    });
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
      observer.disconnect();
      if (import.meta.env.DEV) {
        container.removeEventListener('keydown', handleKeyDownCapture, true);
      }
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
  }, [host, tabKey, applyDims, sendInput]);

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

  const startWindowDrag = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    if (!target) return;
    if (target.closest('button, input, textarea, select, .tab')) return;
    event.preventDefault();
    getCurrentWindow().startDragging();
  };

  return (
    <div className="terminal-wrap">
      <div className="terminal-header" onMouseDown={startWindowDrag}>
        {exited && !connecting && (
          <button className="btn reconnect" onClick={handleReconnect}>
            <RefreshIcon size={13} /> 重连
          </button>
        )}
        <div className="terminal-header-actions">
          <button
            className={`btn ai-toggle${chatOpen ? ' active' : ''}`}
            onClick={onToggleChat}
          >
            <SparklesIcon size={14} /> <span className="toolbar-label">AI</span>
          </button>
          <button
            className={`btn ai-toggle${sftpOpen ? ' active' : ''}`}
            onClick={onToggleSftp}
          >
            <FolderIcon size={14} /> <span className="toolbar-label">文件</span>
          </button>
          <button
            className={`btn ai-toggle${monitorOpen ? ' active' : ''}`}
            onClick={onToggleMonitor}
          >
            <ActivityIcon size={14} /> <span className="toolbar-label">监控</span>
          </button>
          <button
            className={`btn ai-toggle${inspectionOpen ? ' active' : ''}`}
            onClick={onToggleInspection}
          >
            <InspectIcon size={14} /> <span className="toolbar-label">巡检</span>
          </button>
          <button className="btn disconnect" onClick={handleDisconnect}>
            <PowerIcon size={14} /> <span className="toolbar-label">断开</span>
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
