import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import {
  checkForUpdate,
  deleteHost,
  getAppVersion,
  importSshConfig,
  listAiProviders,
  listHosts,
  mcpApprove,
  onMcpApprovalRequest,
  onSessionNotice,
  onTerminalGuardApproval,
  sessionGuardApprove,
} from './api';
import './App.css';
import type {
  AiProvider,
  Host,
  McpApprovalRequest,
  TerminalGuardApproval,
  UpdateInfo,
} from './types';
import AIConfigModal from './components/AIConfigModal';
import AlertModal from './components/AlertModal';
import AuditLogModal from './components/AuditLogModal';
import ChatPanel from './components/ChatPanel';
import GuardApprovalModal from './components/GuardApprovalModal';
import HostForm from './components/HostForm';
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
  const [chatOpen, setChatOpen] = useState(true);
  const [sftpOpen, setSftpOpen] = useState(false);
  const [monitorOpen, setMonitorOpen] = useState(false);
  const [inspectionOpen, setInspectionOpen] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingHost, setEditingHost] = useState<Host | null>(null);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeKey, setActiveKey] = useState<number | null>(null);
  const [loadingHostId, setLoadingHostId] = useState<string | null>(null);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [updateInfo, setUpdateInfo] = useState<UpdateInfo | null>(null);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [rightPanelWidth, setRightPanelWidth] = useState(384);
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

  useEffect(() => {
    refresh().catch((e) => showToast('error', String(e)));
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
    onSessionNotice((_sessionId, message) => showToast('info', message)).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, [showToast]);

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
    let un: (() => void) | undefined;
    let cancelled = false;
    onTerminalGuardApproval((req) => setGuardApproval(req)).then((fn) => {
      if (cancelled) fn();
      else un = fn;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, []);

  const resolveMcpApproval = async (allow: boolean) => {
    const req = mcpApproval;
    if (!req) return;
    try {
      await mcpApprove(req.request_id, allow);
    } catch (e) {
      showToast('error', String(e));
    } finally {
      setMcpApproval(null);
    }
  };

  const resolveGuardApproval = async (allow: boolean) => {
    const req = guardApproval;
    if (!req) return;
    try {
      await sessionGuardApprove(req.session_id, req.request_id, allow);
    } catch (e) {
      showToast('error', String(e));
    } finally {
      setGuardApproval(null);
      // 审批/取消后把键盘焦点还给终端，避免需要手动点击才能继续输入
      window.setTimeout(() => {
        window.dispatchEvent(new CustomEvent('buffterm:refocus-terminal'));
      }, 0);
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

  const handleConnect = (host: Host) => {
    const existing = tabs.find(
      (t) => t.host.id === host.id && t.status === 'connected',
    );
    if (existing) {
      setActiveKey(existing.key);
      setChatOpen(true);
      setSftpOpen(false);
      setMonitorOpen(false);
      setInspectionOpen(false);
      return;
    }
    const key = ++tabSeq.current;
    setTabs((prev) => [
      ...prev,
      { key, host, sessionId: null, status: 'connecting', title: host.name },
    ]);
    setActiveKey(key);
    setLoadingHostId(host.id);
    setChatOpen(true);
    setSftpOpen(false);
    setMonitorOpen(false);
    setInspectionOpen(false);
  };

  const closeTab = (key: number) => {
    const idx = tabs.findIndex((t) => t.key === key);
    setTabs((prev) => prev.filter((t) => t.key !== key));
    if (activeKey === key) {
      const remaining = tabs.filter((t) => t.key !== key);
      const neighbor = remaining[Math.min(idx, remaining.length - 1)];
      setActiveKey(neighbor?.key ?? null);
    }
  };

  const handleDelete = async (host: Host) => {
    if (!window.confirm(`确定删除主机 "${host.name}" 吗？`)) return;
    try {
      await deleteHost(host.id);
      await refresh();
      showToast('success', `已删除 ${host.name}`);
    } catch (e) {
      showToast('error', String(e));
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
      showToast('error', String(e));
    }
  };

  const handleResizeStart = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    setResizing(true);
    const startX = event.clientX;
    const startWidth = rightPanelWidth;
    const onMouseMove = (e: globalThis.MouseEvent) => {
      const delta = startX - e.clientX;
      const maxW = window.innerWidth * 0.5;
      const newWidth = Math.max(280, Math.min(maxW, startWidth + delta));
      setRightPanelWidth(newWidth);
    };
    const onMouseUp = () => {
      setResizing(false);
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.userSelect = '';
      document.body.style.cursor = '';
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
      showToast('error', String(e));
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
                  {hosts.length > 0 && <span className="count">{hosts.length}</span>}
                </div>
                {hosts.length === 0 ? (
                  <div className="rail-popover-empty">还没有主机</div>
                ) : (
                  <div className="rail-host-list">
                    {hosts.map((host) => {
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
                                handleDelete(host);
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
          <span className="count">{hosts.length}</span>
        </div>

        <div className="host-list">
          {hosts.length === 0 && (
            <div className="host-empty">
              <ServerIcon size={28} />
              <p>还没有主机</p>
              <span>新建一台，或从 ssh config 导入</span>
            </div>
          )}

          {hosts.map((host) => {
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
                        handleDelete(host);
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
                    onToggleChat={() => {
                      setChatOpen((v) => !v);
                      setSftpOpen(false);
                      setMonitorOpen(false);
                      setInspectionOpen(false);
                    }}
                    onToggleSftp={() => {
                      setSftpOpen((v) => !v);
                      setChatOpen(false);
                      setMonitorOpen(false);
                      setInspectionOpen(false);
                    }}
                    onToggleMonitor={() => {
                      setMonitorOpen((v) => !v);
                      setChatOpen(false);
                      setSftpOpen(false);
                      setInspectionOpen(false);
                    }}
                    onToggleInspection={() => {
                      setInspectionOpen((v) => !v);
                      setChatOpen(false);
                      setSftpOpen(false);
                      setMonitorOpen(false);
                    }}
                    onOpened={(key, id) => {
                      setTabs((prev) =>
                        prev.map((t) =>
                          t.key === key ? { ...t, sessionId: id, status: 'connected' } : t,
                        ),
                      );
                      setLoadingHostId(null);
                      setChatOpen(true);
                      setSftpOpen(false);
                      setMonitorOpen(false);
                      setInspectionOpen(false);
                    }}
                    onFailed={(key, message) => {
                      setTabs((prev) =>
                        prev.map((t) => (t.key === key ? { ...t, status: 'exited' } : t)),
                      );
                      setLoadingHostId(null);
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

              {activeTab && activeTab.sessionId !== null && chatOpen && (
                <ChatPanel
                  key={activeTab.sessionId}
                  sessionId={activeTab.sessionId}
                  hostId={activeTab.host.id}
                  hostName={activeTab.title}
                  panelWidth={rightPanelWidth}
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
                  onClose={() => setChatOpen(false)}
                />
              )}

              {activeTab && activeTab.sessionId !== null && sftpOpen && (
                <SftpPanel
                  key={`sftp-${activeTab.sessionId}`}
                  host={activeTab.host}
                  onClose={() => setSftpOpen(false)}
                  panelWidth={rightPanelWidth}
                />
              )}

              {activeTab && activeTab.sessionId !== null && monitorOpen && (
                <MonitorPanel
                  key={`mon-${activeTab.sessionId}`}
                  host={activeTab.host}
                  onClose={() => setMonitorOpen(false)}
                  panelWidth={rightPanelWidth}
                />
              )}

              {activeTab && activeTab.sessionId !== null && inspectionOpen && (
                <InspectionPanel
                  key={`inspect-${activeTab.sessionId}`}
                  host={activeTab.host}
                  onClose={() => setInspectionOpen(false)}
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
              <span>⛨ 凭据入钥匙串</span>
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

      <ToastContainer toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

export default App;
