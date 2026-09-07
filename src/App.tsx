import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import { fmtError } from './utils/errors';
import {
  checkForUpdate,
  deleteHost,
  getAppVersion,
  importSshConfig,
  listAiProviders,
  listHosts,
  mcpApprove,
  onMcpApprovalRequest,
  onHostKeyConfirm,
  onTerminalGuardApproval,
  sessionGuardApprove,
  sshConfirmHostKey,
} from './api';
import './App.css';
import type {
  AiProvider,
  Host,
  HostKeyConfirmRequest,
  McpApprovalRequest,
  TerminalGuardApproval,
  UpdateInfo,
} from './types';
import AIConfigModal from './components/AIConfigModal';
import AlertModal from './components/AlertModal';
import AuditLogModal from './components/AuditLogModal';
import ChatPanel from './components/ChatPanel';
import CommandPalette from './components/CommandPalette';
import ConfirmModal from './components/ConfirmModal';
import GuardApprovalModal from './components/GuardApprovalModal';
import HostForm from './components/HostForm';
import HostKeyModal from './components/HostKeyModal';
import InspectionPanel from './components/InspectionPanel';
import McpApprovalModal from './components/McpApprovalModal';
import McpServiceModal from './components/McpServiceModal';
import MonitorPanel from './components/MonitorPanel';
import SftpPanel from './components/SftpPanel';
import TerminalGuardModal from './components/TerminalGuardModal';
import TerminalView from './components/TerminalView';
import ToastContainer, { type ToastItem } from './components/Toast';
import logoUrl from './assets/buffterm-logo.png';
import {
  BellIcon,
  ImportIcon,
  PlusIcon,
  ChevronRightIcon,
  ListIcon,
  PencilIcon,
  ServerIcon,
  SparklesIcon,
  RefreshIcon,
  PanelLeftCloseIcon,
  PanelLeftOpenIcon,
  ShieldIcon,
  SunIcon,
  TerminalIcon,
  TrashIcon,
  MoonIcon,
  WrenchIcon,
} from './components/Icons';

interface Tab {
  key: number;
  host: Host;
  sessionId: number | null;
  status: 'connecting' | 'connected' | 'exited';
  title: string;
}

/** 右侧面板类型（chat / sftp / monitor / inspection），none 表示全部收起 */
type PanelKind = 'chat' | 'sftp' | 'monitor' | 'inspection' | 'none';

const PANEL_STORAGE_KEY = 'buffterm-panel';
const PANEL_WIDTH_STORAGE_KEY = 'buffterm-panel-width';

function readSavedPanel(): PanelKind {
  try {
    const v = localStorage.getItem(PANEL_STORAGE_KEY);
    if (
      v === 'chat' ||
      v === 'sftp' ||
      v === 'monitor' ||
      v === 'inspection' ||
      v === 'none'
    ) {
      return v;
    }
  } catch {
    /* ignore storage errors */
  }
  return 'chat';
}

function App() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [aiProviders, setAiProviders] = useState<AiProvider[]>([]);
  const [showAi, setShowAi] = useState(false);
  const [showLogs, setShowLogs] = useState(false);
  const [showAlerts, setShowAlerts] = useState(false);
  const [showMcp, setShowMcp] = useState(false);
  const [showTerminalGuard, setShowTerminalGuard] = useState(false);
  const [mcpApproval, setMcpApproval] = useState<McpApprovalRequest | null>(null);
  const [guardApproval, setGuardApproval] =
    useState<TerminalGuardApproval | null>(null);
  const [hostKeyApproval, setHostKeyApproval] =
    useState<HostKeyConfirmRequest | null>(null);
  const [chatOpen, setChatOpen] = useState(true);
  const [sftpOpen, setSftpOpen] = useState(false);
  const [monitorOpen, setMonitorOpen] = useState(false);
  const [inspectionOpen, setInspectionOpen] = useState(false);
  const [hostSearch, setHostSearch] = useState('');
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingHost, setEditingHost] = useState<Host | null>(null);
  const [deleteHostTarget, setDeleteHostTarget] = useState<Host | null>(null);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeKey, setActiveKey] = useState<number | null>(null);
  // 供全局快捷键 effect 读取最新状态（refs 让 effect 依赖保持稳定，listener 只挂一次）
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const activeKeyRef = useRef(activeKey);
  activeKeyRef.current = activeKey;
  const [loadingHostId, setLoadingHostId] = useState<string | null>(null);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [updateInfo, setUpdateInfo] = useState<UpdateInfo | null>(null);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [rightPanelWidth, setRightPanelWidth] = useState(() => {
    try {
      const saved = parseInt(
        localStorage.getItem(PANEL_WIDTH_STORAGE_KEY) || '',
        10,
      );
      if (Number.isFinite(saved)) return Math.max(280, Math.min(720, saved));
    } catch {
      /* ignore storage errors */
    }
    return 384;
  });
  const [resizing, setResizing] = useState(false);
  const [theme, setTheme] = useState<'dark' | 'light'>(() => {
    try {
      return localStorage.getItem('buffterm-theme') === 'light' ? 'light' : 'dark';
    } catch {
      return 'dark';
    }
  });
  const [collapsed, setCollapsed] = useState(() => {
    try {
      return localStorage.getItem('buffterm-sidebar-collapsed') === '1';
    } catch {
      return false;
    }
  });
  const toastSeq = useRef(0);
  const tabSeq = useRef(0);

  const dismissToast = useCallback((id: number) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const showToast = useCallback(
    (kind: ToastItem['kind'], message: string) => {
      const id = ++toastSeq.current;
      setToasts((prev) => [...prev, { id, kind, message }]);
      window.setTimeout(() => dismissToast(id), 4600);
    },
    [dismissToast],
  );

  const refresh = useCallback(async () => {
    setHosts(await listHosts());
  }, []);

  const refreshAi = useCallback(async () => {
    setAiProviders(await listAiProviders());
  }, []);

  // 打开指定面板并持久化选择；连接主机/重连时按上次选择恢复，不再强制切回 AI 面板
  const applyPanel = useCallback((panel: PanelKind) => {
    setChatOpen(panel === 'chat');
    setSftpOpen(panel === 'sftp');
    setMonitorOpen(panel === 'monitor');
    setInspectionOpen(panel === 'inspection');
    try {
      localStorage.setItem(PANEL_STORAGE_KEY, panel);
    } catch {
      /* ignore storage errors */
    }
  }, []);

  const restorePanel = useCallback(() => {
    applyPanel(readSavedPanel());
  }, [applyPanel]);

  useEffect(() => {
    refresh().catch((e) => showToast('error', fmtError(e)));
    refreshAi().catch(() => {});
  }, [refresh, refreshAi, showToast]);

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', theme);
    try {
      localStorage.setItem('buffterm-theme', theme);
    } catch {
      /* ignore storage errors */
    }
  }, [theme]);

  useEffect(() => {
    try {
      localStorage.setItem('buffterm-sidebar-collapsed', collapsed ? '1' : '0');
    } catch {
      /* ignore storage errors */
    }
  }, [collapsed]);

  useEffect(() => {
    getAppVersion().then(setAppVersion).catch(() => {});
  }, []);

  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    onMcpApprovalRequest((req) => setMcpApproval(req)).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  useEffect(() => {
    let unGuard: (() => void) | undefined;
    let unKey: (() => void) | undefined;
    let cancelled = false;
    onTerminalGuardApproval((req) => setGuardApproval(req)).then((fn) => {
      if (cancelled) fn();
      else unGuard = fn;
    });
    onHostKeyConfirm((req) => setHostKeyApproval(req)).then((fn) => {
      if (cancelled) fn();
      else unKey = fn;
    });
    return () => {
      cancelled = true;
      unGuard?.();
      unKey?.();
    };
  }, []);

  // guard 审批弹窗超时自动关闭（后端同时按拒绝处理并写审计，无需前端补发 deny）
  useEffect(() => {
    if (!guardApproval) return;
    const timer = window.setTimeout(
      () => setGuardApproval(null),
      Math.max(10, guardApproval.timeout_secs) * 1000,
    );
    return () => window.clearTimeout(timer);
  }, [guardApproval]);

  // MCP 审批弹窗超时自动关闭（后端超时按拒绝处理）
  useEffect(() => {
    if (!mcpApproval) return;
    const secs = mcpApproval.timeout_secs ?? 600;
    const timer = window.setTimeout(() => setMcpApproval(null), secs * 1000);
    return () => window.clearTimeout(timer);
  }, [mcpApproval]);

  // 主机指纹确认弹窗超时自动关闭（后端 60s 超时按拒绝处理，与倒计时一致）
  useEffect(() => {
    if (!hostKeyApproval) return;
    const timer = window.setTimeout(() => setHostKeyApproval(null), 60_000);
    return () => window.clearTimeout(timer);
  }, [hostKeyApproval]);

  const resolveMcpApproval = async (allow: boolean) => {
    const req = mcpApproval;
    if (!req) return;
    try {
      await mcpApprove(req.request_id, allow);
    } catch (e) {
      showToast('error', fmtError(e));
    } finally {
      // 只关闭自己对应的弹窗，避免并发审批时误关新请求的弹窗
      setMcpApproval((cur) => (cur?.request_id === req.request_id ? null : cur));
    }
  };

  const resolveGuardApproval = async (allow: boolean) => {
    const req = guardApproval;
    if (!req) return;
    try {
      await sessionGuardApprove(req.session_id, req.request_id, allow);
    } catch (e) {
      showToast('error', fmtError(e));
    } finally {
      setGuardApproval((cur) => (cur?.request_id === req.request_id ? null : cur));
      // 审批/取消后把键盘焦点还给终端，避免需要手动点击才能继续输入
      window.setTimeout(() => {
        window.dispatchEvent(new CustomEvent('buffterm:refocus-terminal'));
      }, 0);
    }
  };

  const resolveHostKeyApproval = async (trust: boolean) => {
    const req = hostKeyApproval;
    if (!req) return;
    setHostKeyApproval(null);
    try {
      await sshConfirmHostKey(req.key, trust);
    } catch (e) {
      showToast('error', fmtError(e));
    }
  };

  const startWindowDrag = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    if (!target) return;
    if (target.closest('button, input, textarea, select, .tab')) return;
    event.preventDefault();
    getCurrentWindow().startDragging();
  };

  const activeProvider = aiProviders.find((p) => p.enabled) ?? null;
  const activeModelLabel =
    activeProvider?.models.find((m) => m.is_active)?.label ??
    activeProvider?.models[0]?.label ??
    '';

  const activeTab = tabs.find((t) => t.key === activeKey) ?? null;

  // 主机搜索过滤（侧栏与 rail 弹层共用）：按名称 / 地址 / 备注
  const filteredHosts = useMemo(() => {
    const q = hostSearch.trim().toLowerCase();
    if (!q) return hosts;
    return hosts.filter(
      (h) =>
        h.name.toLowerCase().includes(q) ||
        h.address.toLowerCase().includes(q) ||
        (h.notes ?? '').toLowerCase().includes(q),
    );
  }, [hosts, hostSearch]);

  const handleConnect = (host: Host) => {
    const existing = tabs.find(
      (t) => t.host.id === host.id && t.status === 'connected',
    );
    if (existing) {
      setActiveKey(existing.key);
      restorePanel();
      return;
    }
    const key = ++tabSeq.current;
    setTabs((prev) => [
      ...prev,
      { key, host, sessionId: null, status: 'connecting', title: host.name },
    ]);
    setActiveKey(key);
    setLoadingHostId(host.id);
    restorePanel();
  };

  // useCallback + refs 保证引用稳定：避免每次 render 重建导致全局快捷键 effect 反复卸载/挂载
  const closeTab = useCallback((key: number) => {
    const idx = tabsRef.current.findIndex((t) => t.key === key);
    const closing = tabsRef.current[idx];
    setTabs((prev) => prev.filter((t) => t.key !== key));
    if (activeKeyRef.current === key) {
      const remaining = tabsRef.current.filter((t) => t.key !== key);
      const neighbor = remaining[Math.min(idx, remaining.length - 1)];
      setActiveKey(neighbor?.key ?? null);
    }
    // 连接中的主机被关闭时清理 loading 指示
    if (closing) {
      setLoadingHostId((cur) => (cur === closing.host.id ? null : cur));
    }
  }, []);

  // 全局快捷键：Cmd/Ctrl+K 命令面板、Cmd/Ctrl+F 终端搜索、Cmd/Ctrl+T 新建主机、
  // Cmd/Ctrl+W 关闭标签、Ctrl+Tab / Ctrl+Shift+Tab 切换标签。
  // 注意：macOS 系统菜单可能拦截 Cmd+W/Cmd+T（按键到不了 WebView），此时改用 Ctrl 组合键。
  // 焦点保护：输入框/弹窗/终端内不劫持 K/T/W（这些组合在终端里是 shell 编辑键，
  // 如 Ctrl+W 删词、Ctrl+K 删行），否则会双重触发——既误关标签又污染 shell 输入。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      const target = e.target as HTMLElement | null;
      const inField = !!target?.closest(
        'input, textarea, select, [role="dialog"], .modal',
      );
      const inTerminal = !!target?.closest('.xterm');
      const mod = e.metaKey || e.ctrlKey;
      if (mod && (e.key === 'k' || e.key === 'K')) {
        if (inField || inTerminal) return;
        e.preventDefault();
        setPaletteOpen((v) => !v);
      } else if (mod && (e.key === 'f' || e.key === 'F')) {
        // 终端内 Cmd+F 打开终端搜索（终端功能）；输入框内交给系统/浏览器
        if (inField) return;
        e.preventDefault();
        window.dispatchEvent(new CustomEvent('buffterm:open-terminal-search'));
      } else if (mod && (e.key === 't' || e.key === 'T')) {
        if (inField || inTerminal) return;
        e.preventDefault();
        setEditingHost(null);
        setShowForm(true);
      } else if (mod && (e.key === 'w' || e.key === 'W')) {
        if (inField || inTerminal) return;
        e.preventDefault();
        const current = activeKeyRef.current;
        if (current !== null) closeTab(current);
      } else if (e.ctrlKey && e.key === 'Tab') {
        e.preventDefault();
        const tabsNow = tabsRef.current;
        if (tabsNow.length === 0) return;
        const idx = tabsNow.findIndex((t) => t.key === activeKeyRef.current);
        const dir = e.shiftKey ? -1 : 1;
        const next = tabsNow[(idx + dir + tabsNow.length) % tabsNow.length];
        if (next) setActiveKey(next.key);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [closeTab]);

  const handleDelete = async (host: Host) => {
    setDeleteHostTarget(null);
    try {
      await deleteHost(host.id);
      await refresh();
      showToast('success', `已删除 ${host.name}`);
    } catch (e) {
      showToast('error', fmtError(e));
    }
  };

  const handleImport = async () => {
    try {
      const result = await importSshConfig();
      await refresh();
      if (result.imported > 0) {
        showToast(
          'success',
          `已从 ~/.ssh/config 导入 ${result.imported} 台主机` +
            (result.skipped > 0 ? `，跳过 ${result.skipped} 台重名` : ''),
        );
      } else if (result.skipped > 0) {
        showToast('info', `主机均已存在，跳过 ${result.skipped} 台`);
      } else {
        showToast('info', '~/.ssh/config 中没有可导入的主机');
      }
    } catch (e) {
      showToast('error', fmtError(e));
    }
  };

  const handleResizeStart = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    setResizing(true);
    const startX = event.clientX;
    const startWidth = rightPanelWidth;
    let latestWidth = startWidth;
    const onMouseMove = (e: globalThis.MouseEvent) => {
      const delta = startX - e.clientX;
      const maxW = window.innerWidth * 0.5;
      const newWidth = Math.max(280, Math.min(maxW, startWidth + delta));
      latestWidth = newWidth;
      setRightPanelWidth(newWidth);
    };
    const onMouseUp = () => {
      setResizing(false);
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.userSelect = '';
      document.body.style.cursor = '';
      try {
        localStorage.setItem(
          PANEL_WIDTH_STORAGE_KEY,
          String(Math.round(latestWidth)),
        );
      } catch {
        /* ignore storage errors */
      }
    };
    document.body.style.userSelect = 'none';
    document.body.style.cursor = 'col-resize';
    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', onMouseUp);
  };

  const handleUpdateCheck = async () => {
    if (updateInfo?.update_available) {
      openUrl(updateInfo.release_url).catch(() => {
        showToast('error', '无法打开浏览器，请手动访问 GitHub Release 页面');
      });
      return;
    }
    setCheckingUpdate(true);
    try {
      const next = await checkForUpdate();
      setUpdateInfo(next);
      setAppVersion(next.current_version);
      showToast(
        'info',
        !next.release_found
          ? 'GitHub 尚未发布可下载版本。'
          : next.update_available
          ? `发现新版本 v${next.latest_version}，点击“下载更新”前往 GitHub。`
          : '当前已是最新版本。',
      );
    } catch (e) {
      showToast('error', fmtError(e));
    } finally {
      setCheckingUpdate(false);
    }
  };

  const rightPanelVisible =
    activeTab !== null &&
    activeTab.sessionId !== null &&
    (chatOpen || sftpOpen || monitorOpen || inspectionOpen);

  return (
    <div className="app">
      {collapsed ? (
        <aside className="rail">
          <div className="rail-top">
            <div className="rail-logo brand-mark">
              <img className="brand-logo" src={logoUrl} alt="buffTerm" />
            </div>
            <button
              className="rail-btn"
              onClick={() => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))}
              title={theme === 'dark' ? '切换到日间模式' : '切换到夜间模式'}
              aria-label={theme === 'dark' ? '切换到日间模式' : '切换到夜间模式'}
            >
              {theme === 'dark' ? <SunIcon size={16} /> : <MoonIcon size={16} />}
            </button>
            <button
              className="rail-btn"
              onClick={() => setCollapsed(false)}
              title="展开侧边栏"
              aria-label="展开侧边栏"
            >
              <PanelLeftOpenIcon size={16} />
            </button>
          </div>

          <div className="rail-middle">
            <div className="rail-icon-wrap">
              <button className="rail-btn" aria-label="主机">
                <ServerIcon size={18} />
              </button>
              <div className="rail-popover">
                <div className="rail-popover-title">
                  主机
                  {hosts.length > 0 && (
                    <span className="count">
                      {hostSearch.trim()
                        ? `${filteredHosts.length}/${hosts.length}`
                        : hosts.length}
                    </span>
                  )}
                </div>
                {hosts.length > 0 && (
                  <input
                    className="host-search"
                    placeholder="搜索主机…"
                    value={hostSearch}
                    spellCheck={false}
                    onChange={(e) => setHostSearch(e.target.value)}
                  />
                )}
                {hosts.length === 0 ? (
                  <div className="rail-popover-empty">还没有主机</div>
                ) : filteredHosts.length === 0 ? (
                  <div className="rail-popover-empty">没有匹配的主机</div>
                ) : (
                  <div className="rail-host-list">
                    {filteredHosts.map((host) => {
                      const active = tabs.some(
                        (t) => t.host.id === host.id && t.status === 'connected',
                      );
                      return (
                        <div
                          key={host.id}
                          className={`rail-host-item${active ? ' active' : ''}`}
                          onClick={() => handleConnect(host)}
                        >
                          <div className="host-avatar">
                            {host.name.slice(0, 1).toUpperCase()}
                          </div>
                          <div className="host-meta">
                            <div className="host-name-row">
                              <span className="host-name">{host.name}</span>
                              {active && (
                                <span className="status-dot" title="已连接">
                                  <span />
                                </span>
                              )}
                            </div>
                            <span className="host-addr">
                              {host.username}@{host.address}:{host.port}
                            </span>
                          </div>
                          <div className="rail-host-actions">
                            <button
                              className="icon-btn"
                              title="编辑主机"
                              onClick={(e) => {
                                e.stopPropagation();
                                setEditingHost(host);
                                setShowForm(true);
                              }}
                            >
                              <PencilIcon size={13} />
                            </button>
                            <button
                              className="icon-btn danger"
                              title="删除主机"
                              onClick={(e) => {
                                e.stopPropagation();
                                setDeleteHostTarget(host);
                              }}
                            >
                              <TrashIcon size={14} />
                            </button>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            </div>
          </div>

          <div className="rail-bottom">
            <div className="rail-icon-wrap">
              <button
                className="rail-btn"
                onClick={() => {
                  setEditingHost(null);
                  setShowForm(true);
                }}
              >
                <PlusIcon size={16} />
              </button>
              <span className="rail-tip">新建主机</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={handleImport}>
                <ImportIcon size={16} />
              </button>
              <span className="rail-tip">导入 ~/.ssh/config</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowMcp(true)}>
                <WrenchIcon size={16} />
              </button>
              <span className="rail-tip">MCP 服务</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowAlerts(true)}>
                <BellIcon size={16} />
              </button>
              <span className="rail-tip">通知配置</span>
            </div>
            <div className="rail-icon-wrap">
              <button
                className="rail-btn"
                onClick={() => setShowTerminalGuard(true)}
              >
                <ShieldIcon size={16} />
              </button>
              <span className="rail-tip">终端防护</span>
            </div>

            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowLogs(true)}>
                <ListIcon size={16} />
              </button>
              <span className="rail-tip">操作日志</span>
            </div>
            <div className="rail-icon-wrap">
              <button
                className="rail-btn"
                onClick={handleUpdateCheck}
                disabled={checkingUpdate}
              >
                <RefreshIcon size={16} />
              </button>
              <span className="rail-tip">
                {checkingUpdate
                  ? '正在检查更新…'
                  : updateInfo?.update_available
                    ? `下载更新 v${updateInfo.latest_version}`
                    : '检查更新'}
              </span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowAi(true)}>
                <SparklesIcon size={16} />
              </button>
              <span className="rail-tip">AI Agent</span>
            </div>
          </div>
        </aside>
      ) : (
        <aside className="sidebar">
        <div className="brand" onMouseDown={startWindowDrag}>
          <div className="brand-mark">
            <img className="brand-logo" src={logoUrl} alt="buffTerm" />
          </div>
          <div className="brand-text">
            <span className="brand-name">buffTerm</span>
            <span className="brand-sub">SSH Agent · 本地优先</span>
          </div>
          <button
            className="theme-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setTheme((t) => (t === 'dark' ? 'light' : 'dark'));
            }}
            title={theme === 'dark' ? '切换到日间模式' : '切换到夜间模式'}
            aria-label={theme === 'dark' ? '切换到日间模式' : '切换到夜间模式'}
          >
            {theme === 'dark' ? <SunIcon size={16} /> : <MoonIcon size={16} />}
          </button>
          <button
            className="sidebar-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setCollapsed(true);
            }}
            title="收起侧边栏"
            aria-label="收起侧边栏"
          >
            <PanelLeftCloseIcon size={16} />
          </button>
        </div>

        <div className="sidebar-actions">
          <button
            className="btn primary block"
            onClick={() => {
              setEditingHost(null);
              setShowForm(true);
            }}
          >
            <PlusIcon size={16} /> 新建主机
          </button>
          <button className="btn secondary block" onClick={handleImport}>
            <ImportIcon size={16} /> 导入 ~/.ssh/config
          </button>
        </div>

        <div className="section-title">
          <span>主机</span>
          <span className="count">
            {hostSearch.trim()
              ? `${filteredHosts.length}/${hosts.length}`
              : hosts.length}
          </span>
        </div>

        <div className="host-list">
          {hosts.length > 0 && (
            <input
              className="host-search"
              placeholder="搜索主机，或按 Cmd+K"
              value={hostSearch}
              spellCheck={false}
              onChange={(e) => setHostSearch(e.target.value)}
            />
          )}
          {hosts.length === 0 && (
            <div className="host-empty">
              <ServerIcon size={28} />
              <p>还没有主机</p>
              <span>新建一台，或从 ssh config 导入</span>
            </div>
          )}

          {hosts.length > 0 && filteredHosts.length === 0 && (
            <div className="host-empty">
              <p>没有匹配的主机</p>
              <span>换个关键词试试</span>
            </div>
          )}

          {filteredHosts.map((host) => {
            const active = tabs.some(
              (t) => t.host.id === host.id && t.status === 'connected',
            );
            return (
              <div
                key={host.id}
                className={`host-card${active ? ' active' : ''}`}
                onClick={() => handleConnect(host)}
              >
                <div className="host-avatar">{host.name.slice(0, 1).toUpperCase()}</div>
                <div className="host-meta">
                  <div className="host-name-row">
                    <span className="host-name">{host.name}</span>
                    {active && (
                      <span className="status-dot" title="已连接">
                        <span />
                      </span>
                    )}
                  </div>
                  <span className="host-addr">
                    {host.username}@{host.address}:{host.port}
                  </span>
                  <div className="host-tags">
                    <span className={`tag tag-${host.auth_type}`}>
                      {host.auth_type === 'key' ? '密钥' : '密码'}
                    </span>
                  </div>
                </div>
                {loadingHostId === host.id ? (
                  <div className="spinner" />
                ) : (
                  <>
                    <button
                      className="icon-btn"
                      title="编辑主机"
                      onClick={(e) => {
                        e.stopPropagation();
                        setEditingHost(host);
                        setShowForm(true);
                      }}
                    >
                      <PencilIcon size={14} />
                    </button>
                    <button
                      className="icon-btn danger"
                      title="删除主机"
                      onClick={(e) => {
                        e.stopPropagation();
                        setDeleteHostTarget(host);
                      }}
                    >
                      <TrashIcon size={15} />
                    </button>
                  </>
                )}
              </div>
            );
          })}
        </div>

        <div className="sidebar-footer">
          <button className="log-entry" onClick={() => setShowMcp(true)}>
            <WrenchIcon size={15} /> MCP 服务
          </button>
          <button className="log-entry" onClick={() => setShowAlerts(true)}>
            <BellIcon size={15} /> 通知配置
          </button>
          <button
            className="log-entry"
            onClick={() => setShowTerminalGuard(true)}
          >
            <ShieldIcon size={15} /> 终端防护
          </button>

          <button className="log-entry" onClick={() => setShowLogs(true)}>
            <ListIcon size={15} /> 操作日志
          </button>
          <div className="version-entry">
            <button
              className={`log-entry version-check${
                updateInfo?.update_available ? ' update-ready' : ''
              }`}
              onClick={handleUpdateCheck}
              disabled={checkingUpdate}
              title={
                updateInfo?.update_available
                  ? `下载 v${updateInfo.latest_version}`
                  : '检查 GitHub 最新发布版本'
              }
            >
              <RefreshIcon size={15} />
              <span>
                {checkingUpdate
                  ? '正在检查更新…'
                  : updateInfo?.update_available
                    ? `下载更新 v${updateInfo.latest_version}`
                    : '检查更新'}
              </span>
            </button>
            <span className="version-current">
              当前版本 v{appVersion ?? '—'}
              {updateInfo?.release_found && !updateInfo.update_available && ' · 已是最新'}
            </span>
          </div>
          <button className="ai-entry" onClick={() => setShowAi(true)}>
            <span className="ai-entry-icon">
              <SparklesIcon size={16} />
            </span>
            <span className="ai-entry-text">
              <span className="ai-entry-title">AI Agent</span>
              <span className="ai-entry-sub">
                {activeProvider
                  ? `${activeProvider.name} · ${activeModelLabel}`
                  : '未配置模型平台'}
              </span>
            </span>
            <ChevronRightIcon size={15} />
          </button>
        </div>
      </aside>
      )}

      <main className="main">
        {tabs.length > 0 ? (
          <div className="workbench">
            <div className="tab-bar" onMouseDown={startWindowDrag}>
              {tabs.map((tab) => (
                <div
                  key={tab.key}
                  className={`tab${tab.key === activeKey ? ' active' : ''}${
                    tab.status === 'connecting' ? ' connecting' : ''
                  }${
                    tab.status === 'exited' ? ' exited' : ''
                  }`}
                  onClick={() => setActiveKey(tab.key)}
                >
                  <span className="tab-dot" />
                  <span className="tab-title">{tab.title}</span>
                  {tab.status === 'exited' && (
                    <span className="tab-alert" title="连接已断开">
                      !
                    </span>
                  )}
                  <button
                    className="tab-close"
                    title="关闭标签"
                    onClick={(e) => {
                      e.stopPropagation();
                      closeTab(tab.key);
                    }}
                  >
                    ×
                  </button>
                </div>
              ))}
              <button
                className="tab-new"
                title="新建连接"
                onClick={() => {
                  setEditingHost(null);
                  setShowForm(true);
                }}
              >
                <PlusIcon size={13} />
              </button>
            </div>

            <div className="workbench-body">
              {tabs.map((tab) => (
                <div
                  key={tab.key}
                  className={`tab-pane${tab.key === activeKey ? ' active' : ''}`}
                >
                  <TerminalView
                    host={tab.host}
                    tabKey={tab.key}
                    theme={theme}
                    chatOpen={chatOpen}
                    sftpOpen={sftpOpen}
                    monitorOpen={monitorOpen}
                    inspectionOpen={inspectionOpen}
                    onToggleChat={() => applyPanel(chatOpen ? 'none' : 'chat')}
                    onToggleSftp={() => applyPanel(sftpOpen ? 'none' : 'sftp')}
                    onToggleMonitor={() =>
                      applyPanel(monitorOpen ? 'none' : 'monitor')
                    }
                    onToggleInspection={() =>
                      applyPanel(inspectionOpen ? 'none' : 'inspection')
                    }
                    onOpened={(key, id) => {
                      setTabs((prev) =>
                        prev.map((t) =>
                          t.key === key ? { ...t, sessionId: id, status: 'connected' } : t,
                        ),
                      );
                      setLoadingHostId(null);
                      restorePanel();
                    }}
                    onFailed={(key, message) => {
                      setTabs((prev) =>
                        prev.map((t) => (t.key === key ? { ...t, status: 'exited' } : t)),
                      );
                      // 只清当前失败主机的 loading，避免快速连两台主机时
                      // 前一个失败回调误清后一个的 loading
                      setLoadingHostId((cur) => {
                        const tab = tabs.find((t) => t.key === key);
                        return tab && cur === tab.host.id ? null : cur;
                      });
                      showToast('error', `连接失败: ${message}`);
                    }}
                    onExited={(key) => {
                      setTabs((prev) =>
                        prev.map((t) => (t.key === key ? { ...t, status: 'exited' } : t)),
                      );
                    }}
                    onDisconnect={(key) => closeTab(key)}
                  />
                </div>
              ))}

              {rightPanelVisible && (
                <div
                  className={`resize-handle${resizing ? ' resizing' : ''}`}
                  onMouseDown={handleResizeStart}
                />
              )}

              {/* 四个右侧面板保持挂载（用 display 隐藏而非卸载）：
                  切换面板不会中断正在运行的 AI 会话 / SFTP 传输 / 巡检任务，
                  再次打开时状态与进度仍然保留 */}
              {activeTab && activeTab.sessionId !== null && (
                <ChatPanel
                  key={activeTab.sessionId}
                  sessionId={activeTab.sessionId}
                  hostId={activeTab.host.id}
                  hostName={activeTab.title}
                  panelWidth={rightPanelWidth}
                  hidden={!chatOpen}
                  providerLabel={
                    activeProvider
                      ? `${activeProvider.name} · ${activeModelLabel}`
                      : ''
                  }
                  providerConfigured={!!activeProvider}
                  models={activeProvider?.models ?? []}
                  providerId={activeProvider?.id ?? null}
                  onOpenConfig={() => setShowAi(true)}
                  onModelSwitched={() => {
                    refreshAi().catch(() => {});
                  }}
                  onClose={() => applyPanel('none')}
                />
              )}

              {activeTab && activeTab.sessionId !== null && (
                <SftpPanel
                  key={`sftp-${activeTab.sessionId}`}
                  host={activeTab.host}
                  hidden={!sftpOpen}
                  onClose={() => applyPanel('none')}
                  panelWidth={rightPanelWidth}
                />
              )}

              {activeTab && activeTab.sessionId !== null && (
                <MonitorPanel
                  key={`mon-${activeTab.sessionId}`}
                  host={activeTab.host}
                  hidden={!monitorOpen}
                  onClose={() => applyPanel('none')}
                  panelWidth={rightPanelWidth}
                />
              )}

              {activeTab && activeTab.sessionId !== null && (
                <InspectionPanel
                  key={`inspect-${activeTab.sessionId}`}
                  host={activeTab.host}
                  hidden={!inspectionOpen}
                  onClose={() => applyPanel('none')}
                  panelWidth={rightPanelWidth}
                />
              )}
            </div>
          </div>
        ) : (
          <div className="welcome" onMouseDown={startWindowDrag}>
            <div className="welcome-ring">
              <TerminalIcon size={40} />
            </div>
            <h2>选择左侧主机开始连接</h2>
            <p>
              支持密钥 / 密码认证，多标签并行连接
              <br />
              首次连接请按终端提示确认主机指纹
            </p>
            <div className="welcome-hints">
              <span>⇥ 多标签会话</span>
              <span>⛨ 凭据本地加密</span>
              <span>⛨ AI 自动审批</span>
            </div>
          </div>
        )}
      </main>

      {showForm && (
        <HostForm
          initial={editingHost}
          onSaved={() => {
            setShowForm(false);
            setEditingHost(null);
            refresh().catch(() => {});
            showToast('success', `主机已保存`);
          }}
          onCancel={() => {
            setShowForm(false);
            setEditingHost(null);
          }}
        />
      )}

      {showAi && (
        <AIConfigModal
          onClose={() => setShowAi(false)}
          onSaved={() => {
            refreshAi().catch(() => {});
          }}
        />
      )}

      {showLogs && <AuditLogModal onClose={() => setShowLogs(false)} />}

      {paletteOpen && (
        <CommandPalette
          hosts={hosts}
          onConnect={handleConnect}
          onAction={(action) => {
            switch (action) {
              case 'new-host':
                setEditingHost(null);
                setShowForm(true);
                break;
              case 'import-ssh':
                void handleImport();
                break;
              case 'ai-config':
                setShowAi(true);
                break;
              case 'logs':
                setShowLogs(true);
                break;
              case 'mcp':
                setShowMcp(true);
                break;
              case 'guard':
                setShowTerminalGuard(true);
                break;
              case 'alerts':
                setShowAlerts(true);
                break;
            }
          }}
          onClose={() => setPaletteOpen(false)}
        />
      )}

      {showAlerts && <AlertModal onClose={() => setShowAlerts(false)} />}

      {showMcp && (
        <McpServiceModal hosts={hosts} onClose={() => setShowMcp(false)} />
      )}

      {showTerminalGuard && (
        <TerminalGuardModal onClose={() => setShowTerminalGuard(false)} />
      )}

      {mcpApproval && (
        <McpApprovalModal
          request={mcpApproval}
          onResolve={resolveMcpApproval}
        />
      )}

      {guardApproval && (
        <GuardApprovalModal
          request={guardApproval}
          onResolve={resolveGuardApproval}
        />
      )}

      {hostKeyApproval && (
        <HostKeyModal
          request={hostKeyApproval}
          onResolve={resolveHostKeyApproval}
        />
      )}

      {deleteHostTarget && (
        <ConfirmModal
          title="删除主机"
          body={`确定删除主机 "${deleteHostTarget.name}" 吗？`}
          confirmText="删除"
          danger
          onConfirm={() => handleDelete(deleteHostTarget)}
          onCancel={() => setDeleteHostTarget(null)}
        />
      )}

      <ToastContainer toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

export default App;
