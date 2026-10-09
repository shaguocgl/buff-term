import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openUrl } from '@tauri-apps/plugin-opener';
import { fmtError } from './utils/errors';
import { baseName } from './utils/remote';
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
  recordLaunch,
  sessionGuardApprove,
  sshConfirmHostKey,
  starPromptDismiss,
  starPromptSnooze,
} from './api';
import './App.css';
import type {
  AiModel,
  AiProvider,
  Host,
  HostKeyConfirmRequest,
  ImportResult,
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
import StarPromptModal from './components/StarPromptModal';
import TerminalGuardModal from './components/TerminalGuardModal';
import TerminalView from './components/TerminalView';
import ToastContainer, { type ToastItem } from './components/Toast';
import WindowControls, {
  toggleWindowMaximize,
  useCustomWindowControls,
} from './components/WindowControls';
import logoUrl from './assets/buffterm-logo.png';
import {
  BellIcon,
  ImportIcon,
  PlusIcon,
  ChevronDownIcon,
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
  TrashIcon,
  MoonIcon,
  WrenchIcon,
} from './components/Icons';

/** 终端标签：一台主机对应一个 SSH 会话 */
interface TerminalTab {
  kind: 'terminal';
  key: number;
  host: Host;
  sessionId: number | null;
  status: 'connecting' | 'connected' | 'exited';
  title: string;
}

/** 文件标签：中间区内置编辑器打开的远端文件 */
interface FileTab {
  kind: 'file';
  key: number;
  host: Host;
  /** 远端文件绝对路径 */
  path: string;
  title: string;
}

/** 中间工作区标签：终端会话或远端文件编辑器 */
type Tab = TerminalTab | FileTab;

/** 右侧面板类型（chat / sftp / monitor / inspection），none 表示全部收起 */
type PanelKind = 'chat' | 'sftp' | 'monitor' | 'inspection' | 'none';

type ApprovalQueueItem =
  | { kind: 'mcp'; id: string; deadline: number; request: McpApprovalRequest }
  | {
      kind: 'guard';
      id: string;
      deadline: number;
      request: TerminalGuardApproval;
    }
  | {
      kind: 'host-key';
      id: string;
      deadline: number;
      request: HostKeyConfirmRequest;
    };

const PANEL_STORAGE_KEY = 'buffterm-panel';
const PANEL_WIDTH_STORAGE_KEY = 'buffterm-panel-width';

// 文件编辑器（CodeMirror）体积较大且只有打开文件才用得到，按需加载：
// 首屏只加载终端与面板所需代码
const FileEditor = lazy(() => import('./components/FileEditor'));

// 稳定的空数组引用：未配置平台时避免每次渲染生成新数组，导致面板 memo 失效
const EMPTY_MODELS: AiModel[] = [];

/** GitHub 项目仓库地址（Star 引导弹窗跳转目标） */
const REPO_URL = 'https://github.com/shaguocgl/buff-term';

// React.StrictMode 开发模式下 mount effect 会执行两次；
// 启动类一次性动作（自动更新检查、启动次数统计）据此模块级标记去重
let startupDone = false;

/**
 * 是否按在标签栏的横向滚动条上。
 * 滚动条是元素自身的一部分（事件 target 仍是该元素而非子节点），
 * 只能按几何位置判断：元素内容区下沿、厚度等于滚动条高度的一条带。
 * 命中时交给原生滚动，避免被标题栏拖动逻辑吞掉。
 */
function isOnHorizontalScrollbar(target: HTMLElement, clientY: number): boolean {
  const box = target.closest<HTMLElement>('.tab-list');
  if (!box) return false;
  const barHeight = box.offsetHeight - box.clientHeight;
  if (barHeight <= 0) return false;
  return clientY >= box.getBoundingClientRect().bottom - barHeight;
}

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
  const [approvalQueue, setApprovalQueue] = useState<ApprovalQueueItem[]>([]);
  const [chatOpen, setChatOpen] = useState(true);
  const [sftpOpen, setSftpOpen] = useState(false);
  const [monitorOpen, setMonitorOpen] = useState(false);
  const [inspectionOpen, setInspectionOpen] = useState(false);
  const [hostSearch, setHostSearch] = useState('');
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [showForm, setShowForm] = useState(false);
  const [editingHost, setEditingHost] = useState<Host | null>(null);
  const [deleteHostTarget, setDeleteHostTarget] = useState<Host | null>(null);
  // 导入 ~/.ssh/config 的预览统计：非 null 时展示导入确认弹窗
  const [importPreview, setImportPreview] = useState<ImportResult | null>(null);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeKey, setActiveKey] = useState<number | null>(null);
  // 文件标签的未保存标记：关闭标签前据此弹确认；ref 保证 closeTab 引用稳定
  const [dirtyTabs, setDirtyTabs] = useState<Record<number, boolean>>({});
  const dirtyTabsRef = useRef(dirtyTabs);
  dirtyTabsRef.current = dirtyTabs;
  // 待确认关闭的标签（有未保存改动）
  const [pendingCloseKey, setPendingCloseKey] = useState<number | null>(null);
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
  // Star 引导弹窗：非 null 时展示，值为当前累计启动次数（用于感谢文案）
  const [starPromptLaunches, setStarPromptLaunches] = useState<number | null>(
    null,
  );
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
  // 底部菜单（MCP 服务 / 通知配置 / 终端防护 / 操作审计 / 检查更新）收起状态：
  // 主机较多时可收起，把纵向空间让给主机列表
  const [footerCollapsed, setFooterCollapsed] = useState(() => {
    try {
      return localStorage.getItem('buffterm-sidebar-footer-collapsed') === '1';
    } catch {
      return false;
    }
  });
  // 无边框窗口（Windows）下，标签栏右侧自绘最小化 / 最大化 / 关闭按钮
  const { custom: customWindowControls, maximized: windowMaximized } =
    useCustomWindowControls();
  const toastSeq = useRef(0);
  const tabSeq = useRef(0);
  // 右侧面板宽度通过 CSS 变量下发：拖动时直接改 DOM，避免每帧 setState 重渲染整棵面板树
  const workbenchBodyRef = useRef<HTMLDivElement | null>(null);

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

  // 终端「问 AI」：把选中文本交给当前标签的 AI 面板，并确保 AI 面板已展开。
  // seq 递增用于区分相同内容的重复插入。
  const chatInsertSeq = useRef(0);
  const [chatInsert, setChatInsert] = useState<{ text: string; seq: number } | null>(
    null,
  );
  const handleAddToChat = useCallback(
    (text: string) => {
      const trimmed = text.trim();
      if (!trimmed) return;
      applyPanel('chat');
      setChatInsert({ text: trimmed, seq: ++chatInsertSeq.current });
    },
    [applyPanel],
  );

  // 面板 props 稳定化：配合 React.memo，避免 App 因无关状态（toast / 审批队列等）
  // 变化时连带重渲染面板（ChatPanel 每次渲染都会重新解析 Markdown，代价高）
  const closePanel = useCallback(() => applyPanel('none'), [applyPanel]);
  const openAiConfig = useCallback(() => setShowAi(true), []);
  const handleModelSwitched = useCallback(() => {
    refreshAi().catch(() => {});
  }, [refreshAi]);
  const handleInsertConsumed = useCallback(() => setChatInsert(null), []);

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
    try {
      localStorage.setItem(
        'buffterm-sidebar-footer-collapsed',
        footerCollapsed ? '1' : '0',
      );
    } catch {
      /* ignore storage errors */
    }
  }, [footerCollapsed]);

  useEffect(() => {
    getAppVersion().then(setAppVersion).catch(() => {});
  }, []);

  // 启动时静默检查一次更新（发现新版则 toast 提示，左下角入口同步变为「下载更新」），
  // 并累计启动次数：超过 5 次后弹出 GitHub Star 引导（可以后再说 / 不再提示）。
  useEffect(() => {
    if (startupDone) return;
    startupDone = true;
    checkForUpdate()
      .then((next) => {
        setUpdateInfo(next);
        setAppVersion(next.current_version);
        if (next.update_available) {
          showToast(
            'info',
            `发现新版本 v${next.latest_version}，可在左下角菜单下载更新。`,
          );
        }
      })
      .catch(() => {});
    recordLaunch()
      .then((s) => {
        if (s.show_star_prompt) setStarPromptLaunches(s.launches);
      })
      .catch(() => {});
  }, [showToast]);

  // 把面板宽度同步到 CSS 变量（首帧即生效，避免宽度跳变）
  useLayoutEffect(() => {
    workbenchBodyRef.current?.style.setProperty(
      '--right-panel-width',
      `${rightPanelWidth}px`,
    );
  }, [rightPanelWidth]);

  const enqueueApproval = useCallback((item: ApprovalQueueItem) => {
    const now = Date.now();
    setApprovalQueue((prev) => [
      ...prev.filter(
        (p) => p.deadline > now && !(p.kind === item.kind && p.id === item.id),
      ),
      item,
    ]);
  }, []);

  useEffect(() => {
    let unsubs: (() => void)[] = [];
    let cancelled = false;
    Promise.all([
      onMcpApprovalRequest((req) =>
        enqueueApproval({
          kind: 'mcp',
          id: req.request_id,
          deadline: Date.now() + (req.timeout_secs ?? 600) * 1000,
          request: req,
        }),
      ),
      onTerminalGuardApproval((req) =>
        enqueueApproval({
          kind: 'guard',
          id: req.request_id,
          deadline: Date.now() + Math.max(10, req.timeout_secs) * 1000,
          request: req,
        }),
      ),
      onHostKeyConfirm((req) =>
        enqueueApproval({
          kind: 'host-key',
          id: req.key,
          deadline: Date.now() + 60_000,
          request: req,
        }),
      ),
    ]).then((fns) => {
      if (cancelled) fns.forEach((fn) => fn());
      else unsubs = fns;
    });
    return () => {
      cancelled = true;
      unsubs.forEach((fn) => fn());
    };
  }, [enqueueApproval]);

  const activeApproval = approvalQueue[0];

  useEffect(() => {
    if (approvalQueue.length === 0) return;
    const nextDeadline = Math.min(
      ...approvalQueue.map((item) => item.deadline),
    );
    const dropExpired = () => {
      const now = Date.now();
      setApprovalQueue((prev) =>
        prev.filter((item) => item.deadline > now),
      );
    };
    const delay = nextDeadline - Date.now();
    if (delay <= 0) {
      dropExpired();
      return;
    }
    const timer = window.setTimeout(dropExpired, delay);
    return () => window.clearTimeout(timer);
  }, [approvalQueue]);

  const resolveApproval = async (
    item: ApprovalQueueItem,
    allow: boolean,
  ) => {
    try {
      switch (item.kind) {
        case 'mcp':
          await mcpApprove(item.request.request_id, allow);
          break;
        case 'guard':
          await sessionGuardApprove(
            item.request.session_id,
            item.request.request_id,
            allow,
          );
          break;
        case 'host-key':
          await sshConfirmHostKey(item.request.key, allow);
          break;
      }
    } catch (e) {
      showToast('error', fmtError(e));
    } finally {
      setApprovalQueue((prev) =>
        prev.filter((p) => !(p.kind === item.kind && p.id === item.id)),
      );
      if (item.kind === 'guard') {
        // 审批/取消后把键盘焦点还给终端，避免需要手动点击才能继续输入
        window.setTimeout(() => {
          window.dispatchEvent(new CustomEvent('buffterm:refocus-terminal'));
        }, 0);
      }
    }
  };

  const startWindowDrag = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    if (!target) return;
    if (target.closest('button, input, textarea, select, .tab')) return;
    // 标签过多时标签栏会出现横向滚动条：拖它应滚动标签，而不是拖动窗口
    if (isOnHorizontalScrollbar(target, event.clientY)) return;
    event.preventDefault();
    // 无边框窗口下原生标题栏不存在，双击标题栏区域补上“最大化 / 还原”
    if (customWindowControls && event.detail === 2) {
      toggleWindowMaximize();
      return;
    }
    getCurrentWindow().startDragging();
  };

  const activeProvider = aiProviders.find((p) => p.enabled) ?? null;
  const activeModelLabel =
    activeProvider?.models.find((m) => m.is_active)?.label ??
    activeProvider?.models[0]?.label ??
    '';

  const activeTab = tabs.find((t) => t.key === activeKey) ?? null;

  // 最近一次激活的终端标签：切到文件标签时右侧面板继续挂在它上面，
  // 保持挂载（目录 / 传输 / AI 会话状态不重置），只是被隐藏
  const lastTerminalTabRef = useRef<TerminalTab | null>(null);
  useEffect(() => {
    if (activeTab?.kind === 'terminal') lastTerminalTabRef.current = activeTab;
  }, [activeTab]);
  const terminalActive = activeTab?.kind === 'terminal';
  const panelTab = useMemo(() => {
    if (activeTab?.kind === 'terminal') return activeTab;
    const remembered = lastTerminalTabRef.current;
    if (!remembered) return null;
    // 记住的终端标签可能已被关闭，需要回查当前标签列表
    return (
      (tabs.find(
        (t) => t.kind === 'terminal' && t.key === remembered.key,
      ) as TerminalTab | undefined) ?? null
    );
  }, [activeTab, tabs]);
  // 文件面板只需要主机信息：文件标签同样有 host，编辑文件时也能继续浏览目录
  const sftpPanelHost = activeTab?.host ?? panelTab?.host ?? null;
  // 所有有标签页的主机（去重）：每台主机各挂一个文件面板实例并保持挂载，
  // 在不同主机的标签间来回切换时目录位置 / 传输任务不会被重置
  const sftpHosts = useMemo(() => {
    const seen = new Map<string, Host>();
    for (const t of tabs) if (!seen.has(t.host.id)) seen.set(t.host.id, t.host);
    return [...seen.values()];
  }, [tabs]);

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
      (t) => t.kind === 'terminal' && t.host.id === host.id && t.status === 'connected',
    );
    if (existing) {
      setActiveKey(existing.key);
      restorePanel();
      return;
    }
    const key = ++tabSeq.current;
    setTabs((prev) => [
      ...prev,
      {
        kind: 'terminal',
        key,
        host,
        sessionId: null,
        status: 'connecting',
        title: host.name,
      },
    ]);
    setActiveKey(key);
    setLoadingHostId(host.id);
    restorePanel();
  };

  // 文件面板点击文件：在中间区打开；同一主机同一路径复用已有标签
  const handleOpenFile = useCallback((host: Host, path: string) => {
    const existing = tabsRef.current.find(
      (t) => t.kind === 'file' && t.host.id === host.id && t.path === path,
    );
    if (existing) {
      setActiveKey(existing.key);
      return;
    }
    const key = ++tabSeq.current;
    setTabs((prev) => [
      ...prev,
      { kind: 'file', key, host, path, title: baseName(path) },
    ]);
    setActiveKey(key);
  }, []);

  // 文件编辑器的脏状态上报（引用稳定，避免每次渲染重挂 FileEditor 的监听）
  const handleFileDirtyChange = useCallback((key: number, dirty: boolean) => {
    setDirtyTabs((prev) => {
      if (!!prev[key] === dirty) return prev;
      const next = { ...prev };
      if (dirty) next[key] = true;
      else delete next[key];
      return next;
    });
  }, []);

  // useCallback + refs 保证引用稳定：避免每次 render 重建导致全局快捷键 effect 反复卸载/挂载
  const doCloseTab = useCallback((key: number) => {
    const idx = tabsRef.current.findIndex((t) => t.key === key);
    const closing = tabsRef.current[idx];
    setTabs((prev) => prev.filter((t) => t.key !== key));
    setDirtyTabs((prev) => {
      if (!(key in prev)) return prev;
      const next = { ...prev };
      delete next[key];
      return next;
    });
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

  // 关闭标签：文件标签有未保存改动时先弹确认，确认后走 doCloseTab
  const closeTab = useCallback(
    (key: number) => {
      if (dirtyTabsRef.current[key]) {
        setPendingCloseKey(key);
        return;
      }
      doCloseTab(key);
    },
    [doCloseTab],
  );

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
        // 代码编辑器（.cm-editor）内同样不劫持：K/T/W 交给编辑器自身处理
        'input, textarea, select, [role="dialog"], .modal, .cm-editor',
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

  /** 导入结果的补充说明：跳过重名 / 忽略的规则块 */
  const importDetail = (r: ImportResult) => {
    const parts: string[] = [];
    if (r.skipped > 0) parts.push(`跳过重名 ${r.skipped} 台`);
    if (r.ignored > 0) parts.push(`忽略 ${r.ignored} 条通配规则或空 Host 行`);
    return parts.length > 0 ? `（${parts.join('，')}）` : '';
  };

  // 导入前先预览统计：没有可导入的主机时直接提示，否则弹确认窗口
  const handleImport = async () => {
    try {
      const preview = await importSshConfig(true);
      if (preview.imported === 0) {
        showToast(
          'info',
          `~/.ssh/config 中没有可导入的主机${importDetail(preview)}`,
        );
        return;
      }
      setImportPreview(preview);
    } catch (e) {
      showToast('error', fmtError(e));
    }
  };

  const confirmImport = async () => {
    setImportPreview(null);
    try {
      const result = await importSshConfig();
      await refresh();
      showToast(
        'success',
        `已从 ~/.ssh/config 导入 ${result.imported} 台主机${importDetail(result)}`,
      );
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
      // 拖动期间只改 CSS 变量，不触发 React 重渲染：
      // 否则每个 mousemove 都会重渲染全部右侧面板与终端（含 Markdown 重新解析），导致卡死
      workbenchBodyRef.current?.style.setProperty(
        '--right-panel-width',
        `${newWidth}px`,
      );
    };
    const onMouseUp = () => {
      setResizing(false);
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.userSelect = '';
      document.body.style.cursor = '';
      // 拖动结束才提交到 state，保证与 CSS 变量一致（并供其它逻辑/持久化使用）
      setRightPanelWidth(latestWidth);
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

  // Star 引导弹窗三个出口：去 Star / 以后再说 / 不再提示。
  // 语义见 StarPromptModal：X、Esc、遮罩关闭一律按「以后再说」延后处理
  const handleStar = () => {
    setStarPromptLaunches(null);
    void starPromptDismiss();
    openUrl(REPO_URL).catch(() => {
      showToast('error', '无法打开浏览器，请手动访问 GitHub 项目页');
    });
  };
  const handleStarLater = () => {
    setStarPromptLaunches(null);
    void starPromptSnooze();
  };
  const handleStarNever = () => {
    setStarPromptLaunches(null);
    void starPromptDismiss();
  };

  // 分隔条可见性：文件面板只要打开且当前有主机上下文就显示；
  // 其余三个面板依赖终端会话，仅在终端标签激活时显示
  const sessionPanelVisible =
    terminalActive &&
    panelTab !== null &&
    panelTab.sessionId !== null &&
    (chatOpen || monitorOpen || inspectionOpen);
  const rightPanelVisible = (sftpOpen && sftpPanelHost !== null) || sessionPanelVisible;

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
                        (t) =>
                          t.kind === 'terminal' &&
                          t.host.id === host.id &&
                          t.status === 'connected',
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
              <span className="rail-tip">操作审计</span>
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
              (t) =>
                t.kind === 'terminal' &&
                t.host.id === host.id &&
                t.status === 'connected',
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
          <button
            className="footer-toggle"
            onClick={() => setFooterCollapsed((v) => !v)}
            title={
              footerCollapsed
                ? '展开菜单'
                : '收起菜单，把纵向空间让给主机列表'
            }
            aria-expanded={!footerCollapsed}
            aria-controls="sidebar-footer-menu"
          >
            <span>{footerCollapsed ? '展开' : '收起'}</span>
            <span className="footer-toggle-right">
              {footerCollapsed && updateInfo?.update_available && (
                <span className="footer-toggle-badge">有更新</span>
              )}
              {footerCollapsed ? (
                <ChevronRightIcon size={14} />
              ) : (
                <ChevronDownIcon size={14} />
              )}
            </span>
          </button>

          {!footerCollapsed && (
            <div className="footer-menu" id="sidebar-footer-menu">
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
                <ListIcon size={15} /> 操作审计
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
                  {updateInfo?.release_found &&
                    !updateInfo.update_available &&
                    ' · 已是最新'}
                </span>
              </div>
            </div>
          )}

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
            <div
              className={`tab-bar${
                customWindowControls ? ' with-window-controls' : ''
              }`}
              onMouseDown={startWindowDrag}
            >
              <div className="tab-list">
                {tabs.map((tab) => {
                  const isTerminal = tab.kind === 'terminal';
                  return (
                    <div
                      key={tab.key}
                      className={`tab${tab.key === activeKey ? ' active' : ''}${
                        isTerminal && tab.status === 'connecting'
                          ? ' connecting'
                          : ''
                      }${
                        isTerminal && tab.status === 'exited' ? ' exited' : ''
                      }${dirtyTabs[tab.key] ? ' dirty' : ''}`}
                      onClick={() => setActiveKey(tab.key)}
                      title={isTerminal ? tab.title : tab.path}
                    >
                      <span
                        className={`tab-dot${isTerminal ? '' : ' tab-dot-file'}`}
                      />
                      <span className="tab-title">{tab.title}</span>
                      {dirtyTabs[tab.key] && (
                        <span className="tab-dirty" title="有未保存的改动">
                          ●
                        </span>
                      )}
                      {isTerminal && tab.status === 'exited' && (
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
                  );
                })}
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
              {customWindowControls && (
                <WindowControls maximized={windowMaximized} />
              )}
            </div>

            <div className="workbench-body" ref={workbenchBodyRef}>
              {tabs.map((tab) => (
                <div
                  key={tab.key}
                  className={`tab-pane${tab.key === activeKey ? ' active' : ''}`}
                >
                  {tab.kind === 'terminal' ? (
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
                            t.kind === 'terminal' && t.key === key
                              ? { ...t, sessionId: id, status: 'connected' }
                              : t,
                          ),
                        );
                        setLoadingHostId(null);
                        restorePanel();
                      }}
                      onFailed={(key, message) => {
                        setTabs((prev) =>
                          prev.map((t) =>
                            t.kind === 'terminal' && t.key === key
                              ? { ...t, status: 'exited' }
                              : t,
                          ),
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
                          prev.map((t) =>
                            t.kind === 'terminal' && t.key === key
                              ? { ...t, status: 'exited' }
                              : t,
                          ),
                        );
                      }}
                      onDisconnect={(key) => closeTab(key)}
                      onAddToChat={handleAddToChat}
                    />
                  ) : (
                    <Suspense
                      fallback={
                        <div className="file-editor-loading">
                          正在加载编辑器…
                        </div>
                      }
                    >
                      <FileEditor
                        host={tab.host}
                        path={tab.path}
                        tabKey={tab.key}
                        theme={theme}
                        sftpOpen={sftpOpen}
                        onToggleSftp={() => applyPanel(sftpOpen ? 'none' : 'sftp')}
                        onDirtyChange={handleFileDirtyChange}
                        onClose={() => closeTab(tab.key)}
                        onNotify={showToast}
                      />
                    </Suspense>
                  )}
                </div>
              ))}

              {rightPanelVisible && (
                <div
                  className={`resize-handle${resizing ? ' resizing' : ''}`}
                  onMouseDown={handleResizeStart}
                />
              )}

              {/* 四个右侧面板保持挂载（用 display 隐藏而非卸载）：
                  切换面板 / 切到文件标签都不会中断正在运行的 AI 会话 / SFTP 传输 /
                  巡检任务，再次打开时状态与进度仍然保留 */}
              {panelTab && panelTab.sessionId !== null && (
                <ChatPanel
                  key={panelTab.sessionId}
                  sessionId={panelTab.sessionId}
                  hostId={panelTab.host.id}
                  hostName={panelTab.title}
                  hidden={!chatOpen || !terminalActive}
                  insertText={chatInsert}
                  onInsertConsumed={handleInsertConsumed}
                  providerLabel={
                    activeProvider
                      ? `${activeProvider.name} · ${activeModelLabel}`
                      : ''
                  }
                  providerConfigured={!!activeProvider}
                  models={activeProvider?.models ?? EMPTY_MODELS}
                  providerId={activeProvider?.id ?? null}
                  onOpenConfig={openAiConfig}
                  onModelSwitched={handleModelSwitched}
                  onClose={closePanel}
                />
              )}

              {/* 文件面板按主机挂载（每台有标签页的主机一个实例，全部保持挂载）：
                  编辑文件时仍可用、关闭编辑器或在主机间切换标签后目录都不重置 */}
              {sftpHosts.map((host) => (
                <SftpPanel
                  key={`sftp-${host.id}`}
                  host={host}
                  hidden={!sftpOpen || sftpPanelHost?.id !== host.id}
                  onOpenFile={(path) => handleOpenFile(host, path)}
                  onClose={closePanel}
                />
              ))}

              {panelTab && panelTab.sessionId !== null && (
                <MonitorPanel
                  key={`mon-${panelTab.sessionId}`}
                  host={panelTab.host}
                  hidden={!monitorOpen || !terminalActive}
                  onClose={closePanel}
                />
              )}

              {panelTab && panelTab.sessionId !== null && (
                <InspectionPanel
                  key={`inspect-${panelTab.sessionId}`}
                  host={panelTab.host}
                  hidden={!inspectionOpen || !terminalActive}
                  onClose={closePanel}
                />
              )}
            </div>
          </div>
        ) : (
          <div className="welcome" onMouseDown={startWindowDrag}>
            {/* 无标签页时没有标签栏，窗口按钮固定到右上角，避免无法关闭 / 最小化 */}
            {customWindowControls && (
              <WindowControls maximized={windowMaximized} />
            )}
            <div className="welcome-logo brand-mark">
              <img className="brand-logo" src={logoUrl} alt="buffTerm" />
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
          showToast={showToast}
        />
      )}

      {showLogs && (
        <AuditLogModal onClose={() => setShowLogs(false)} showToast={showToast} />
      )}

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

      {activeApproval?.kind === 'mcp' && (
        <McpApprovalModal
          key={`mcp-${activeApproval.id}`}
          request={activeApproval.request}
          deadline={activeApproval.deadline}
          onResolve={(allow) => resolveApproval(activeApproval, allow)}
        />
      )}

      {activeApproval?.kind === 'guard' && (
        <GuardApprovalModal
          key={`guard-${activeApproval.id}`}
          request={activeApproval.request}
          deadline={activeApproval.deadline}
          onResolve={(allow) => resolveApproval(activeApproval, allow)}
        />
      )}

      {activeApproval?.kind === 'host-key' && (
        <HostKeyModal
          key={`host-key-${activeApproval.id}`}
          request={activeApproval.request}
          deadline={activeApproval.deadline}
          onResolve={(trust) => resolveApproval(activeApproval, trust)}
        />
      )}

      {importPreview && (
        <ConfirmModal
          title="导入 ~/.ssh/config"
          body={[
            `将从 ~/.ssh/config 导入 ${importPreview.imported} 台主机。`,
            importPreview.skipped > 0
              ? `其中 ${importPreview.skipped} 台与现有主机重名，将自动跳过。`
              : '',
            '导入的主机不含密码与私钥口令，需要时请在主机编辑中补充。',
          ]
            .filter(Boolean)
            .join('\n')}
          confirmText={`导入 ${importPreview.imported} 台`}
          onConfirm={confirmImport}
          onCancel={() => setImportPreview(null)}
        />
      )}

      {starPromptLaunches !== null && (
        <StarPromptModal
          launches={starPromptLaunches}
          onStar={handleStar}
          onLater={handleStarLater}
          onNever={handleStarNever}
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

      {pendingCloseKey !== null && (
        <ConfirmModal
          title="关闭标签"
          body="该文件有未保存的改动，关闭后这些改动将丢失。确定关闭吗？"
          confirmText="丢弃并关闭"
          danger
          onConfirm={() => {
            const key = pendingCloseKey;
            setPendingCloseKey(null);
            doCloseTab(key);
          }}
          onCancel={() => setPendingCloseKey(null)}
        />
      )}

      <ToastContainer toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

export default App;
