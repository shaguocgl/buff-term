import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
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
} from './api';
import './App.css';
import type { AiProvider, Host, McpApprovalRequest, UpdateInfo } from './types';
import AIConfigModal from './components/AIConfigModal';
import AlertModal from './components/AlertModal';
import AuditLogModal from './components/AuditLogModal';
import ChatPanel from './components/ChatPanel';
import HostForm from './components/HostForm';
import InspectionPanel from './components/InspectionPanel';
import McpApprovalModal from './components/McpApprovalModal';
import McpServiceModal from './components/McpServiceModal';
import MonitorPanel from './components/MonitorPanel';
import SftpPanel from './components/SftpPanel';
import TerminalView from './components/TerminalView';
import ToastContainer, { type ToastItem } from './components/Toast';
import logoUrl from './assets/keywisp-logo.svg';
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
  TerminalIcon,
  TrashIcon,
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
  const [mcpApproval, setMcpApproval] = useState<McpApprovalRequest | null>(null);
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

  const handleUpdateCheck = async () => {
    if (updateInfo?.update_available) {
      window.open(updateInfo.release_url, '_blank', 'noopener,noreferrer');
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

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand" onMouseDown={startWindowDrag}>
          <div className="brand-mark">
            <img className="brand-logo" src={logoUrl} alt="KeyWisp" />
          </div>
          <div className="brand-text">
            <span className="brand-name">KeyWisp</span>
            <span className="brand-sub">SSH Agent · 本地优先</span>
          </div>
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

              {activeTab && activeTab.sessionId !== null && chatOpen && (
                <ChatPanel
                  key={activeTab.sessionId}
                  sessionId={activeTab.sessionId}
                  hostId={activeTab.host.id}
                  hostName={activeTab.title}
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
                />
              )}

              {activeTab && activeTab.sessionId !== null && monitorOpen && (
                <MonitorPanel
                  key={`mon-${activeTab.sessionId}`}
                  host={activeTab.host}
                  onClose={() => setMonitorOpen(false)}
                />
              )}

              {activeTab && activeTab.sessionId !== null && inspectionOpen && (
                <InspectionPanel
                  key={`inspect-${activeTab.sessionId}`}
                  host={activeTab.host}
                  onClose={() => setInspectionOpen(false)}
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

      {mcpApproval && (
        <McpApprovalModal
          request={mcpApproval}
          onResolve={resolveMcpApproval}
        />
      )}

      <ToastContainer toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

export default App;
