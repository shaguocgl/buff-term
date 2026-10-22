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
import { useI18n } from './i18n';
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
  onHostConnectionChanged,
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

/** 按主机保存的面板选择（host.id → 面板类型）：每台主机独立记忆，互不影响 */
const PANELS_STORAGE_KEY = 'buffterm-panels';
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

/** 面板类型校验：读取本地存储时过滤非法 / 损坏值 */
function isPanelKind(v: unknown): v is PanelKind {
  return (
    v === 'chat' ||
    v === 'sftp' ||
    v === 'monitor' ||
    v === 'inspection' ||
    v === 'none'
  );
}

/** 读取「按主机」保存的面板选择（host.id → 面板类型）；缺失 / 损坏时返回空表 */
function readSavedPanels(): Record<string, PanelKind> {
  try {
    const raw = localStorage.getItem(PANELS_STORAGE_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : null;
    if (!parsed || typeof parsed !== 'object') return {};
    const out: Record<string, PanelKind> = {};
    for (const [hostId, panel] of Object.entries(parsed as Record<string, unknown>)) {
      if (isPanelKind(panel) && panel !== 'none') out[hostId] = panel;
    }
    return out;
  } catch {
    return {};
  }
}

function App() {
  const { t, lang, setLang } = useI18n();
  const [hosts, setHosts] = useState<Host[]>([]);
  const [aiProviders, setAiProviders] = useState<AiProvider[]>([]);
  const [showAi, setShowAi] = useState(false);
  const [showLogs, setShowLogs] = useState(false);
  const [showAlerts, setShowAlerts] = useState(false);
  const [showMcp, setShowMcp] = useState(false);
  const [showTerminalGuard, setShowTerminalGuard] = useState(false);
  const [approvalQueue, setApprovalQueue] = useState<ApprovalQueueItem[]>([]);
  const [panelByHost, setPanelByHost] =
    useState<Record<string, PanelKind>>(readSavedPanels);
  // 当前活动主机（活动标签所属主机）：applyPanel 据此定位写入哪台主机的选择；
  // 当前主机的面板开关状态在下面按 activeTab 派生（chatOpen / sftpOpen / …）
  const activeHostIdRef = useRef<string | null>(null);
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

  // 打开指定面板：按主机记忆选择并持久化；「全部收起」时移除该主机的记录
  // （没有记录 = 不打开任何面板），各主机之间互不影响
  const applyPanel = useCallback((panel: PanelKind) => {
    const hostId = activeHostIdRef.current;
    if (!hostId) return;
    setPanelByHost((prev) => {
      if ((prev[hostId] ?? 'none') === panel) return prev;
      const next = { ...prev };
      if (panel === 'none') {
        delete next[hostId];
      } else {
        next[hostId] = panel;
      }
      try {
        localStorage.setItem(PANELS_STORAGE_KEY, JSON.stringify(next));
      } catch {
        /* ignore storage errors */
      }
      return next;
    });
  }, []);

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
            t('app.updateToast', { version: next.latest_version }),
          );
        }
      })
      .catch(() => {});
    recordLaunch()
      .then((s) => {
        if (s.show_star_prompt) setStarPromptLaunches(s.launches);
      })
      .catch(() => {});
  }, [showToast, t]);

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

  // 主机连接信息变更或删除：后端已经关闭该主机的会话、连接池与在跑任务，
  // 前端同步关闭相关标签，避免继续用旧的 Host 对象/聊天记录操作新主机。
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onHostConnectionChanged(({ host_id }) => {
      const removed = tabsRef.current.filter((tab) => tab.host.id === host_id);
      if (removed.length === 0) return;
      const remaining = tabsRef.current.filter((tab) => tab.host.id !== host_id);
      setTabs(remaining);
      setDirtyTabs((prev) => {
        const next = { ...prev };
        for (const tab of removed) delete next[tab.key];
        return next;
      });
      if (
        activeKeyRef.current !== null &&
        removed.some((tab) => tab.key === activeKeyRef.current)
      ) {
        setActiveKey(remaining[0]?.key ?? null);
      }
      setPanelByHost((prev) => {
        if (!(host_id in prev)) return prev;
        const next = { ...prev };
        delete next[host_id];
        return next;
      });
    })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

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

  // 面板选择跟随主机：每台主机独立记住自己打开的功能面板，
  // 没有记录的主机默认不打开任何面板，切换主机互不干扰
  const activeHostId = activeTab?.host.id ?? null;
  activeHostIdRef.current = activeHostId;
  const activePanel: PanelKind =
    (activeHostId !== null ? panelByHost[activeHostId] : undefined) ?? 'none';
  const chatOpen = activePanel === 'chat';
  const sftpOpen = activePanel === 'sftp';
  const monitorOpen = activePanel === 'monitor';
  const inspectionOpen = activePanel === 'inspection';

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
  // 各终端标签注册的「当前控制台行」读取器：文件面板打开时据此校正 cwd
  const consoleReadersRef = useRef(new Map<number, () => string | null>());
  const registerConsoleReader = useCallback(
    (key: number, reader: (() => string | null) | null) => {
      if (reader) consoleReadersRef.current.set(key, reader);
      else consoleReadersRef.current.delete(key);
    },
    [],
  );
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

  const confirmHostConnectionUpdate = useCallback(
    (current: Host, next: Host) => {
      const changed =
        current.address !== next.address ||
        current.port !== next.port ||
        current.username !== next.username ||
        current.auth_type !== next.auth_type ||
        current.key_path !== next.key_path;
      if (!changed) return true;
      const hasDirtyTabs = tabsRef.current.some(
        (tab) =>
          tab.host.id === current.id && !!dirtyTabsRef.current[tab.key],
      );
      return !hasDirtyTabs || window.confirm(t('hostForm.unsavedTabsOnChange'));
    },
    [t],
  );

  const handleDelete = async (host: Host) => {
    setDeleteHostTarget(null);
    try {
      await deleteHost(host.id);
      await refresh();
      showToast('success', t('app.deletedHost', { name: host.name }));
    } catch (e) {
      showToast('error', fmtError(e));
    }
  };

  /** 导入结果的补充说明：跳过重名 / 忽略的规则块 */
  const importDetail = (r: ImportResult) => {
    const parts: string[] = [];
    if (r.skipped > 0) parts.push(t('app.importSkipDup', { n: r.skipped }));
    if (r.ignored > 0) parts.push(t('app.importIgnored', { n: r.ignored }));
    return parts.length > 0
      ? t('app.importDetail', { parts: parts.join(t('app.importDetailJoin')) })
      : '';
  };

  // 导入前先预览统计：没有可导入的主机时直接提示，否则弹确认窗口
  const handleImport = async () => {
    try {
      const preview = await importSshConfig(true);
      if (preview.imported === 0) {
        showToast(
          'info',
          t('app.importNothing', { detail: importDetail(preview) }),
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
        t('app.importedCount', {
          n: result.imported,
          detail: importDetail(result),
        }),
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
        showToast('error', t('app.cannotOpenBrowser'));
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
          ? t('app.noRelease')
          : next.update_available
            ? t('app.updateFound', { version: next.latest_version })
            : t('app.latest'),
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
      showToast('error', t('app.cannotOpenRepo'));
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
              title={theme === 'dark' ? t('app.themeLight') : t('app.themeDark')}
              aria-label={theme === 'dark' ? t('app.themeLight') : t('app.themeDark')}
            >
              {theme === 'dark' ? <SunIcon size={16} /> : <MoonIcon size={16} />}
            </button>
            <button
              className="rail-btn lang-toggle"
              onClick={() => setLang(lang === 'zh' ? 'en' : 'zh')}
              title={lang === 'zh' ? t('app.switchToEn') : t('app.switchToZh')}
              aria-label={t('app.langAria')}
            >
              {lang === 'zh' ? t('app.langCurrentZh') : t('app.langCurrentEn')}
            </button>
            <button
              className="rail-btn"
              onClick={() => setCollapsed(false)}
              title={t('app.expandSidebar')}
              aria-label={t('app.expandSidebar')}
            >
              <PanelLeftOpenIcon size={16} />
            </button>
          </div>

          <div className="rail-middle">
            <div className="rail-icon-wrap">
              <button className="rail-btn" aria-label={t('app.hosts')}>
                <ServerIcon size={18} />
              </button>
              <div className="rail-popover">
                <div className="rail-popover-title">
                  {t('app.hosts')}
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
                    placeholder={t('app.searchHostsShort')}
                    value={hostSearch}
                    spellCheck={false}
                    onChange={(e) => setHostSearch(e.target.value)}
                  />
                )}
                {hosts.length === 0 ? (
                  <div className="rail-popover-empty">{t('app.noHosts')}</div>
                ) : filteredHosts.length === 0 ? (
                  <div className="rail-popover-empty">{t('app.noMatchHosts')}</div>
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
                                <span className="status-dot" title={t('app.connected')}>
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
                              title={t('app.editHost')}
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
                              title={t('app.deleteHost')}
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
              <span className="rail-tip">{t('app.newHost')}</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={handleImport}>
                <ImportIcon size={16} />
              </button>
              <span className="rail-tip">{t('app.importSsh')}</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowMcp(true)}>
                <WrenchIcon size={16} />
              </button>
              <span className="rail-tip">{t('app.mcpService')}</span>
            </div>
            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowAlerts(true)}>
                <BellIcon size={16} />
              </button>
              <span className="rail-tip">{t('app.notifications')}</span>
            </div>
            <div className="rail-icon-wrap">
              <button
                className="rail-btn"
                onClick={() => setShowTerminalGuard(true)}
              >
                <ShieldIcon size={16} />
              </button>
              <span className="rail-tip">{t('app.terminalGuard')}</span>
            </div>

            <div className="rail-icon-wrap">
              <button className="rail-btn" onClick={() => setShowLogs(true)}>
                <ListIcon size={16} />
              </button>
              <span className="rail-tip">{t('app.auditLog')}</span>
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
                  ? t('app.checkingUpdate')
                  : updateInfo?.update_available
                    ? t('app.downloadUpdate', {
                        version: updateInfo.latest_version,
                      })
                    : t('app.checkUpdate')}
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
            <span className="brand-sub">{t('app.brandSub')}</span>
          </div>
          <button
            className="theme-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setTheme((t) => (t === 'dark' ? 'light' : 'dark'));
            }}
            title={theme === 'dark' ? t('app.themeLight') : t('app.themeDark')}
            aria-label={theme === 'dark' ? t('app.themeLight') : t('app.themeDark')}
          >
            {theme === 'dark' ? <SunIcon size={16} /> : <MoonIcon size={16} />}
          </button>
          <button
            className="theme-toggle lang-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setLang(lang === 'zh' ? 'en' : 'zh');
            }}
            title={lang === 'zh' ? t('app.switchToEn') : t('app.switchToZh')}
            aria-label={t('app.langAria')}
          >
            {lang === 'zh' ? t('app.langCurrentZh') : t('app.langCurrentEn')}
          </button>
          <button
            className="sidebar-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setCollapsed(true);
            }}
            title={t('app.collapseSidebar')}
            aria-label={t('app.collapseSidebar')}
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
            <PlusIcon size={16} /> {t('app.newHost')}
          </button>
          <button className="btn secondary block" onClick={handleImport}>
            <ImportIcon size={16} /> {t('app.importSsh')}
          </button>
        </div>

        <div className="section-title">
          <span>{t('app.hosts')}</span>
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
              placeholder={t('app.searchHosts')}
              value={hostSearch}
              spellCheck={false}
              onChange={(e) => setHostSearch(e.target.value)}
            />
          )}
          {hosts.length === 0 && (
            <div className="host-empty">
              <ServerIcon size={28} />
              <p>{t('app.noHosts')}</p>
              <span>{t('app.noHostsHint')}</span>
            </div>
          )}

          {hosts.length > 0 && filteredHosts.length === 0 && (
            <div className="host-empty">
              <p>{t('app.noMatchHosts')}</p>
              <span>{t('app.tryAnotherKeyword')}</span>
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
                      <span className="status-dot" title={t('app.connected')}>
                        <span />
                      </span>
                    )}
                  </div>
                  <span className="host-addr">
                    {host.username}@{host.address}:{host.port}
                  </span>
                  <div className="host-tags">
                    <span className={`tag tag-${host.auth_type}`}>
                      {host.auth_type === 'key'
                        ? t('hostForm.authKey')
                        : t('hostForm.authPassword')}
                    </span>
                  </div>
                </div>
                {loadingHostId === host.id ? (
                  <div className="spinner" />
                ) : (
                  <>
                    <button
                      className="icon-btn"
                      title={t('app.editHost')}
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
                      title={t('app.deleteHost')}
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
              footerCollapsed ? t('app.expandMenu') : t('app.collapseMenu')
            }
            aria-expanded={!footerCollapsed}
            aria-controls="sidebar-footer-menu"
          >
            <span>{footerCollapsed ? t('app.expand') : t('app.collapse')}</span>
            <span className="footer-toggle-right">
              {footerCollapsed && updateInfo?.update_available && (
                <span className="footer-toggle-badge">{t('app.hasUpdate')}</span>
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
                <WrenchIcon size={15} /> {t('app.mcpService')}
              </button>
              <button className="log-entry" onClick={() => setShowAlerts(true)}>
                <BellIcon size={15} /> {t('app.notifications')}
              </button>
              <button
                className="log-entry"
                onClick={() => setShowTerminalGuard(true)}
              >
                <ShieldIcon size={15} /> {t('app.terminalGuard')}
              </button>

              <button className="log-entry" onClick={() => setShowLogs(true)}>
                <ListIcon size={15} /> {t('app.auditLog')}
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
                      ? t('app.downloadVersion', {
                          version: updateInfo.latest_version,
                        })
                      : t('app.checkGithubRelease')
                  }
                >
                  <RefreshIcon size={15} />
                  <span>
                    {checkingUpdate
                      ? t('app.checkingUpdate')
                      : updateInfo?.update_available
                        ? t('app.downloadUpdate', {
                            version: updateInfo.latest_version,
                          })
                        : t('app.checkUpdate')}
                  </span>
                </button>
                <span className="version-current">
                  {t('app.currentVersion', { version: appVersion ?? '—' })}
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
                  : t('app.noProvider')}
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
                        <span className="tab-dirty" title={t('app.unsavedChanges')}>
                          ●
                        </span>
                      )}
                      {isTerminal && tab.status === 'exited' && (
                        <span className="tab-alert" title={t('app.disconnected')}>
                          !
                        </span>
                      )}
                      <button
                        className="tab-close"
                        title={t('app.closeTab')}
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
                  title={t('app.newConnection')}
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
                      onRegisterConsoleReader={registerConsoleReader}
                      onOpened={(key, id) => {
                        setTabs((prev) =>
                          prev.map((t) =>
                            t.kind === 'terminal' && t.key === key
                              ? { ...t, sessionId: id, status: 'connected' }
                              : t,
                          ),
                        );
                        setLoadingHostId(null);
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
                        showToast('error', t('app.connectionFailed', { msg: message }));
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
                          {t('app.loadingEditor')}
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
                  panelOpen={panelByHost[host.id] === 'sftp'}
                  terminalSessionId={
                    activeTab?.kind === 'terminal' && activeTab.host.id === host.id
                      ? activeTab.sessionId
                      : panelTab?.host.id === host.id
                        ? panelTab.sessionId
                        : null
                  }
                  readConsoleLine={
                    activeTab?.kind === 'terminal' && activeTab.host.id === host.id
                      ? () => consoleReadersRef.current.get(activeTab.key)?.() ?? null
                      : panelTab?.host.id === host.id
                        ? () => consoleReadersRef.current.get(panelTab.key)?.() ?? null
                        : null
                  }
                  onOpenFile={(path) => handleOpenFile(host, path)}
                  onNotify={showToast}
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
            <h2>{t('app.welcomeTitle')}</h2>
            <p>
              {t('app.welcomeLine1')}
              <br />
              {t('app.welcomeLine2')}
            </p>
            <div className="welcome-hints">
              <span>{t('app.welcomeHint1')}</span>
              <span>{t('app.welcomeHint2')}</span>
              <span>{t('app.welcomeHint3')}</span>
            </div>
          </div>
        )}
      </main>

      {showForm && (
        <HostForm
          initial={editingHost}
          onBeforeUpdate={confirmHostConnectionUpdate}
          onSaved={() => {
            setShowForm(false);
            setEditingHost(null);
            refresh().catch(() => {});
            showToast('success', t('app.hostSaved'));
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
          title={t('app.importTitle')}
          body={[
            t('app.importBodyFrom', { n: importPreview.imported }),
            importPreview.skipped > 0
              ? t('app.importBodySkip', { n: importPreview.skipped })
              : '',
            t('app.importBodyNote'),
          ]
            .filter(Boolean)
            .join('\n')}
          confirmText={t('app.importConfirm', { n: importPreview.imported })}
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
          title={t('app.deleteHostTitle')}
          body={
            t('app.deleteHostBody', { name: deleteHostTarget.name }) +
            (tabs.some(
              (tab) =>
                tab.host.id === deleteHostTarget.id &&
                !!dirtyTabs[tab.key],
            )
              ? `\n\n${t('app.deleteHostDirtyWarning')}`
              : '')
          }
          confirmText={t('common.delete')}
          danger
          onConfirm={() => handleDelete(deleteHostTarget)}
          onCancel={() => setDeleteHostTarget(null)}
        />
      )}

      {pendingCloseKey !== null && (
        <ConfirmModal
          title={t('app.closeTabTitle')}
          body={t('app.closeTabBody')}
          confirmText={t('app.discardClose')}
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
