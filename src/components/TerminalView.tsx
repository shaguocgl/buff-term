import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import { WebLinksAddon } from '@xterm/addon-web-links';
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
import { fmtError } from '../utils/errors';
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
  theme: 'dark' | 'light';
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

const TERMINAL_THEMES = {
  dark: {
    background: '#0f1115',
    foreground: '#d6dee8',
  },
  light: {
    background: '#ffffff',
    foreground: '#1f2933',
    cursor: '#4a6cf7',
    cursorAccent: '#ffffff',
    selectionBackground: 'rgba(74, 108, 247, 0.25)',
    black: '#2d3748',
    red: '#c53030',
    green: '#2f855a',
    yellow: '#b7791f',
    blue: '#2b6cb0',
    magenta: '#b83280',
    cyan: '#0e7c86',
    white: '#6b7a8d',
    brightBlack: '#9ba8b8',
    brightRed: '#e53e3e',
    brightGreen: '#38a169',
    brightYellow: '#d69e2e',
    brightBlue: '#3182ce',
    brightMagenta: '#d53f8c',
    brightCyan: '#0ea5b7',
    brightWhite: '#e2e8f0',
  },
} as const;

function normalizeDims(dims: { cols: number; rows: number } | undefined) {
  // 按实际可视区域计算列宽，避免强制 100 列导致右侧被裁剪、输入字符“看不到”。
  // proposeDimensions 在容器未就绪时可能返回 undefined / NaN，必须全部兜底，
  // 否则 NaN 经 JSON 序列化成 null 会触发后端 “invalid type: null, expected u16”。
  const rawCols = dims?.cols;
  const rawRows = dims?.rows;
  const cols = Number.isFinite(rawCols)
    ? Math.max(2, Math.min(400, rawCols as number))
    : 100;
  const rows = Number.isFinite(rawRows)
    ? Math.max(10, Math.min(200, rawRows as number))
    : 30;
  return { cols, rows };
}

const TERMINAL_FONT_KEY = 'buffterm-term-fontsize';
const clampFontSize = (v: number) => Math.min(24, Math.max(10, v));

// 终端内搜索条：Enter 下一个 / Shift+Enter 上一个 / Esc 关闭
function TerminalSearchBar({
  searchAddon,
  onClose,
}: {
  searchAddon: SearchAddon;
  onClose: () => void;
}) {
  const [query, setQuery] = useState('');
  const [stats, setStats] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  useEffect(() => {
    const disposable = searchAddon.onDidChangeResults((r) => {
      if (r.resultCount === 0) setStats('无结果');
      else if (r.resultIndex >= 0 && r.resultCount > 0)
        setStats(`${r.resultIndex + 1}/${r.resultCount}`);
      else setStats(null);
    });
    return () => disposable.dispose();
  }, [searchAddon]);

  const doSearch = (dir: 'next' | 'prev') => {
    if (!query) return;
    if (dir === 'next') searchAddon.findNext(query);
    else searchAddon.findPrevious(query);
  };

  return (
    <div className="terminal-search">
      <input
        ref={inputRef}
        value={query}
        placeholder="搜索终端…"
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            doSearch(e.shiftKey ? 'prev' : 'next');
          } else if (e.key === 'Escape') {
            e.preventDefault();
            onClose();
          }
        }}
      />
      {stats && <span className="terminal-search-stats">{stats}</span>}
      <button
        className="icon-btn"
        title="上一个 (Shift+Enter)"
        onClick={() => doSearch('prev')}
      >
        ↑
      </button>
      <button
        className="icon-btn"
        title="下一个 (Enter)"
        onClick={() => doSearch('next')}
      >
        ↓
      </button>
      <button className="icon-btn" title="关闭 (Esc)" onClick={onClose}>
        ×
      </button>
    </div>
  );
}

export default function TerminalView({
  host,
  tabKey,
  theme,
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
  const searchAddonRef = useRef<SearchAddon | null>(null);
  const disposedRef = useRef(false);
  const onOpenedRef = useRef(onOpened);
  const onFailedRef = useRef(onFailed);
  const onExitedRef = useRef(onExited);
  const onDisconnectRef = useRef(onDisconnect);
  const zoomRef = useRef<(delta: number) => void>(() => {});
  const [connecting, setConnecting] = useState(true);
  const [exited, setExited] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [fontSize, setFontSize] = useState(() => {
    const parsed = parseInt(
      localStorage.getItem(TERMINAL_FONT_KEY) || '14',
      10,
    );
    return Number.isFinite(parsed) ? clampFontSize(parsed) : 14;
  });
  const fontSizeRef = useRef(fontSize);
  fontSizeRef.current = fontSize;
  const [autoReconnect, setAutoReconnect] = useState(
    () => localStorage.getItem('buffterm-autoreconnect') === '1',
  );
  const autoReconnectRef = useRef(autoReconnect);
  autoReconnectRef.current = autoReconnect;
  const reconnectTimerRef = useRef<number | null>(null);
  const reconnectAttemptsRef = useRef(0);
  // 同一次断开（closed+exited 双事件）只调度一次重连
  const reconnectScheduledRef = useRef(false);
  const scheduleReconnectRef = useRef<() => void>(() => {});

  onOpenedRef.current = onOpened;
  onFailedRef.current = onFailed;
  onExitedRef.current = onExited;
  onDisconnectRef.current = onDisconnect;

  // 字号缩放：Cmd/Ctrl + = / - / 0，持久化到 localStorage
  const changeFontSize = useCallback((delta: number) => {
    setFontSize((prev) => {
      const next = clampFontSize(delta === 0 ? 14 : prev + delta);
      try {
        localStorage.setItem(TERMINAL_FONT_KEY, String(next));
      } catch {
        /* ignore storage errors */
      }
      return next;
    });
  }, []);
  zoomRef.current = changeFontSize;

  const closeSearch = useCallback(() => {
    setSearchOpen(false);
    searchAddonRef.current?.clearDecorations();
    termRef.current?.focus();
  }, []);

  // 全局 Cmd+F（App.tsx 派发）打开搜索条；仅可见标签页响应
  useEffect(() => {
    const onOpenSearch = () => {
      const container = containerRef.current;
      if (container && container.offsetParent !== null) setSearchOpen(true);
    };
    window.addEventListener('buffterm:open-terminal-search', onOpenSearch);
    return () =>
      window.removeEventListener(
        'buffterm:open-terminal-search',
        onOpenSearch,
      );
  }, []);

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

  // 读取 xterm 当前光标行（readline 提示符 + 完整命令，含补全结果）。
  // 回车时作为权威命令传给后端拦截判定，避免本地追踪 / 回显重建在补全等场景失真。
  const readConsoleLine = useCallback((): string | null => {
    const term = termRef.current;
    if (!term) return null;
    const buf = term.buffer.active;
    // cursorY 是相对于视口顶部的行号，getLine 需要缓冲区绝对行号；
    // 终端滚动（如 cat 输出多行）后两者会偏移，必须加上 baseY。
    const line = buf.getLine(buf.baseY + buf.cursorY);
    if (!line) return null;
    const text = line.translateToString(true).trimEnd();
    return text || null;
  }, []);

  const sendInput = useCallback(
    (data: number[], consoleLine?: string | null) => {
      inputChainRef.current = inputChainRef.current
        .then(() => {
          const sid = sessionIdRef.current;
          if (sid === null) {
            // 连接建立前用户已经开始输入时，先放入待发送队列，连接成功后统一补发
            pendingInputRef.current.push(...data);
            return;
          }
          // 全屏应用（vim/htop/less）期间透传模式与按键同路同步，避免独立调用乱序
          const passthrough = termRef.current?.buffer.active.type === 'alternate';
          return sessionInput(sid, data, passthrough, consoleLine ?? null);
        })
        .catch(() => {});
    },
    [],
  );

  const connect = useCallback(async () => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    const { cols, rows } = normalizeDims(fit.proposeDimensions());
    const id = await openSession(host.id, cols, rows);
    if (disposedRef.current) {
      closeSession(id).catch(() => {});
      return;
    }
    sessionIdRef.current = id;
    // 会话退出时 disableStdin 被置 true，重连成功后必须恢复，
    // 否则 xterm 自身的按键处理（方向键等）在重连后仍然被禁用。
    term.options.disableStdin = false;
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

  const cancelPendingReconnect = useCallback(() => {
    if (reconnectTimerRef.current !== null) {
      window.clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    reconnectScheduledRef.current = false;
  }, []);

  // 自动重连：会话结束后按指数退避重试（1s→2s→…→上限 30s，最多 5 次）。
  // 不 reset 终端以保留 scrollback；重连成功后重置计数。
  // 注意：后端读循环无法区分网络断开与 shell 正常退出（都发 exited），
  // 因此该行为由用户显式开启（默认关），退出命令同样会触发重连。
  const scheduleReconnect = useCallback(() => {
    if (reconnectScheduledRef.current) return;
    const attempt = reconnectAttemptsRef.current;
    if (attempt >= 5) {
      termRef.current?.writeln(
        '\r\n\x1b[31m[自动重连失败：已达最大重试次数，请手动重连]\x1b[0m',
      );
      return;
    }
    reconnectScheduledRef.current = true;
    const delayMs = Math.min(30_000, 1000 * 2 ** attempt);
    termRef.current?.writeln(
      `\r\n\x1b[90m[${Math.round(delayMs / 1000)}s 后自动重连（第 ${attempt + 1}/5 次）…]\x1b[0m`,
    );
    reconnectTimerRef.current = window.setTimeout(() => {
      reconnectTimerRef.current = null;
      reconnectScheduledRef.current = false;
      reconnectAttemptsRef.current += 1;
      setConnecting(true);
      connectRef.current()
        .then(() => {
          reconnectAttemptsRef.current = 0;
          termRef.current?.writeln('\r\n\x1b[32m[已自动重新连接]\x1b[0m');
        })
        .catch((e) => {
          setConnecting(false);
          setExited(true);
          const message = fmtError(e);
          termRef.current?.writeln(`\r\n\x1b[31m[重连失败: ${message}]\x1b[0m`);
          scheduleReconnectRef.current();
        });
    }, delayMs);
  }, []);
  scheduleReconnectRef.current = scheduleReconnect;

  const toggleAutoReconnect = () => {
    setAutoReconnect((v) => {
      const next = !v;
      try {
        localStorage.setItem('buffterm-autoreconnect', next ? '1' : '0');
      } catch {
        /* ignore storage errors */
      }
      if (!next) cancelPendingReconnect();
      return next;
    });
  };

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
      fontSize: fontSizeRef.current,
      scrollback: 5000,
      theme: TERMINAL_THEMES[theme],
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    const searchAddon = new SearchAddon();
    const webLinks = new WebLinksAddon((event, uri) => {
      event.preventDefault();
      openUrl(uri).catch(() => {});
    });
    term.loadAddon(searchAddon);
    term.loadAddon(webLinks);
    searchAddonRef.current = searchAddon;
    term.open(container);
    fit.fit();
    applyDims();
    term.focus();
    termRef.current = term;
    fitRef.current = fit;

    // 普通可打印字符直接从原生 keydown 截获发送（避免 WKWebView 丢字），
    // onData 收到对应单字节 ASCII 时直接忽略，避免 xterm 补发导致双显。
    // Cmd/Ctrl + =/-/0 作为终端字号缩放，其余组合键交给系统/浏览器。
    //
    // 边界标记：
    // - composingRef：IME 组合期间（compositionstart ~ compositionend）
    // - suppressKeyRef：compositionend 后短暂抑制「最终字符 keydown」，
    //   因为部分浏览器会在组合结束后补发一个 isComposing=false 的 keydown，
    //   而组合文本已通过 onData 发出，若不抑制会双写
    // - pasteRef：paste 事件期间，粘贴的单个 ASCII 字符必须走 onData
    //   （不经过 keydown），不能被「忽略单字节」逻辑丢掉
    let composing = false;
    let suppressKey = false;
    let pasting = false;
    const handleKeyDownCapture = (event: KeyboardEvent) => {
      if (event.ctrlKey || event.metaKey) {
        if (event.key === '=' || event.key === '+') {
          event.preventDefault();
          event.stopPropagation();
          zoomRef.current(1);
          return;
        }
        if (event.key === '-') {
          event.preventDefault();
          event.stopPropagation();
          zoomRef.current(-1);
          return;
        }
        if (event.key === '0') {
          event.preventDefault();
          event.stopPropagation();
          zoomRef.current(0);
          return;
        }
        return;
      }
      if (
        composing ||
        suppressKey ||
        event.isComposing ||
        event.key === 'Process' ||
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
    container.addEventListener('keydown', handleKeyDownCapture, true);

    const handleCompositionStart = () => {
      composing = true;
    };
    const handleCompositionEnd = () => {
      composing = false;
      suppressKey = true;
      window.setTimeout(() => {
        suppressKey = false;
      }, 50);
    };
    const handlePaste = () => {
      pasting = true;
    };
    container.addEventListener('compositionstart', handleCompositionStart);
    container.addEventListener('compositionend', handleCompositionEnd);
    container.addEventListener('paste', handlePaste, true);

    term.onData((data) => {
      const bytes = Array.from(new TextEncoder().encode(data));
      // 粘贴数据（含单个 ASCII 字符）必须发送：粘贴不经过 keydown 直发路径
      const fromPaste = pasting;
      pasting = false;
      // 可打印 ASCII 单字节由原生 keydown 直发，这里忽略 xterm 的补发；
      // 控制键、粘贴、输入法等多字节/特殊输入仍走 onData。
      if (
        !fromPaste &&
        bytes.length === 1 &&
        bytes[0] >= 0x20 &&
        bytes[0] <= 0x7e
      ) {
        return;
      }
      // 单次回车：读取当前控制台行（含 readline 补全/历史/编辑后的最终命令），
      // 作为拦截判定的权威命令；多字节粘贴不读取，保持原有多行逐行判定。
      let consoleLine: string | null = null;
      if (bytes.length === 1 && (bytes[0] === 0x0d || bytes[0] === 0x0a)) {
        consoleLine = readConsoleLine();
      }
      sendInput(bytes, consoleLine);
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
          if (autoReconnectRef.current) scheduleReconnectRef.current();
        } else if (status === 'closed') {
          // 手动断开：不自动重连（标签页即将关闭）
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
      const message = fmtError(e);
      term.writeln(`\r\n\x1b[31m[连接失败: ${message}]\x1b[0m`);
      setConnecting(false);
      setExited(true);
      onFailedRef.current(tabKey, message);
    });

    return () => {
      disposed = true;
      disposedRef.current = true;
      if (reconnectTimerRef.current !== null) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      reconnectScheduledRef.current = false;
      observer.disconnect();
      container.removeEventListener('keydown', handleKeyDownCapture, true);
      container.removeEventListener('compositionstart', handleCompositionStart);
      container.removeEventListener('compositionend', handleCompositionEnd);
      container.removeEventListener('paste', handlePaste, true);
      unData?.();
      unStatus?.();
      const sid = sessionIdRef.current;
      if (sid !== null) {
        closeSession(sid).catch(() => {});
      }
      sessionIdRef.current = null;
      searchAddon.dispose();
      webLinks.dispose();
      searchAddonRef.current = null;
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [host, tabKey, applyDims, sendInput]);

  // 字号变化时同步 xterm 并重新适配容器
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    if (term && term.options.fontSize !== fontSize) {
      term.options.fontSize = fontSize;
      fit?.fit();
      applyDims();
    }
  }, [fontSize, applyDims]);

  // 主题切换时同步更新 xterm 配色（terminal 已在上面 effect 中创建）
  useEffect(() => {
    const term = termRef.current;
    if (term) {
      term.options.theme = TERMINAL_THEMES[theme];
    }
  }, [theme]);

  // 高危命令审批弹窗关闭后，把键盘焦点还给（可见的）终端，免去手动点击
  useEffect(() => {
    const onRefocus = () => {
      const term = termRef.current;
      const container = containerRef.current;
      // 非激活标签页 display:none（offsetParent 为 null），不抢焦点
      if (term && container && container.offsetParent !== null) {
        term.focus();
      }
    };
    window.addEventListener('buffterm:refocus-terminal', onRefocus);
    return () => window.removeEventListener('buffterm:refocus-terminal', onRefocus);
  }, []);

  const handleDisconnect = () => {
    cancelPendingReconnect();
    reconnectAttemptsRef.current = 0;
    const sid = sessionIdRef.current;
    if (sid !== null) {
      closeSession(sid).catch(() => {});
    }
    onDisconnectRef.current(tabKey);
  };

  const handleReconnect = () => {
    cancelPendingReconnect();
    reconnectAttemptsRef.current = 0;
    setConnecting(true);
    termRef.current?.reset();
    connectRef.current().catch((e) => {
      const message = fmtError(e);
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
        {exited && !connecting && (
          <label
            className="autoreconnect-toggle"
            title="会话结束后自动尝试重连（最多 5 次，指数退避）。注意：手动输入 exit 退出也会触发"
            onMouseDown={(e) => e.stopPropagation()}
          >
            <input
              type="checkbox"
              checked={autoReconnect}
              onChange={toggleAutoReconnect}
            />
            自动重连
          </label>
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
        {searchOpen && searchAddonRef.current && (
          <TerminalSearchBar
            searchAddon={searchAddonRef.current}
            onClose={closeSearch}
          />
        )}
      </div>
    </div>
  );
}
