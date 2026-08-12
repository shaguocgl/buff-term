import { useCallback, useEffect, useRef, useState } from 'react';
import {
  deleteHost,
  importSshConfig,
  listAiProviders,
  listHosts,
} from './api';
import './App.css';
import type { AiProvider, Host } from './types';
import AIConfigModal from './components/AIConfigModal';
import AlertModal from './components/AlertModal';
import AuditLogModal from './components/AuditLogModal';
import ChatPanel from './components/ChatPanel';
import HostForm from './components/HostForm';
import InspectionModal from './components/InspectionModal';
import MonitorPanel from './components/MonitorPanel';
import McpModal from './components/McpModal';
import SftpPanel from './components/SftpPanel';
import TerminalView from './components/TerminalView';
import ToastContainer, { type ToastItem } from './components/Toast';
import {
  BellIcon,
  ImportIcon,
  PlusIcon,
  ChevronRightIcon,
  ListIcon,
  PencilIcon,
  RadarIcon,
  ServerIcon,
  SparklesIcon,
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
  const [showInspection, setShowInspection] = useState(false);
  const [showMcp, setShowMcp] = useState(false);
  const [chatOpen, setChatOpen] = useState(true);
  const [sftpOpen, setSftpOpen] = useState(false);
  const [monitorOpen, setMonitorOpen] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingHost, setEditingHost] = useState<Host | null>(null);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeKey, setActiveKey] = useState<number | null>(null);
  const [loadingHostId, setLoadingHostId] = useState<string | null>(null);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
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

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">
            <TerminalIcon size={18} />
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
                    {host.proxy_jump && (
                      <span className="tag tag-proxy">⤳ {host.proxy_jump.split('@')[0]}</span>
                    )}
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
            <WrenchIcon size={15} /> MCP 工具
          </button>
          <button className="log-entry" onClick={() => setShowInspection(true)}>
            <RadarIcon size={15} /> AI 定时巡检
          </button>
          <button className="log-entry" onClick={() => setShowAlerts(true)}>
            <BellIcon size={15} /> 告警通知
          </button>
          <button className="log-entry" onClick={() => setShowLogs(true)}>
            <ListIcon size={15} /> 操作日志
          </button>
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
            <div className="tab-bar">
              {tabs.map((tab) => (
                <div
                  key={tab.key}
                  className={`tab${tab.key === activeKey ? ' active' : ''}${
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
                    onToggleChat={() => setChatOpen((v) => !v)}
                    onToggleSftp={() => {
                      setSftpOpen((v) => !v);
                      setChatOpen(false);
                      setMonitorOpen(false);
                    }}
                    onToggleMonitor={() => {
                      setMonitorOpen((v) => !v);
                      setChatOpen(false);
                      setSftpOpen(false);
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
                    onFailed={(key) => {
                      setTabs((prev) =>
                        prev.map((t) => (t.key === key ? { ...t, status: 'exited' } : t)),
                      );
                      setLoadingHostId(null);
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
            </div>
          </div>
        ) : (
          <div className="welcome">
            <div className="welcome-ring">
              <TerminalIcon size={40} />
            </div>
            <h2>选择左侧主机开始连接</h2>
            <p>
              支持密钥 / 密码认证与 ProxyJump 跳板，多标签并行连接
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

      {showInspection && (
        <InspectionModal hosts={hosts} onClose={() => setShowInspection(false)} />
      )}

      {showMcp && <McpModal onClose={() => setShowMcp(false)} />}

      <ToastContainer toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

export default App;
