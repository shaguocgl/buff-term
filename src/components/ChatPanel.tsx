import {
  memo,
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import {
  agentApprove,
  agentCancel,
  agentChat,
  agentContinue,
  agentReset,
  getContextUsage,
  getHistory,
  getTaskPlan,
  onAiDone,
  onAiError,
  onAiContext,
  onAiPlan,
  onAiRoundsExhausted,
  onAiRoundsResolved,
  onAiStream,
  onAiTool,
  setActiveAiModel,
} from '../api';
import { copyToClipboard } from '../utils/clipboard';
import { fmtError } from '../utils/errors';
import { useT, type DictKey, type TFunc } from '../i18n';
import { baseName } from '../utils/remote';
import type { AiModel, ContextUsage, HistoryEntry, TaskPlan } from '../types';
import Select from './Select';
import RemoteFilePicker from './RemoteFilePicker';
import {
  FileIcon,
  ImageIcon,
  PlusIcon,
  RefreshIcon,
  SendIcon,
  ShieldIcon,
  SparklesIcon,
  StopIcon,
  TerminalIcon,
  XIcon,
} from './Icons';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

type ToolState = 'request' | 'denied' | 'running' | 'result' | 'error';

interface ToolView {
  id: string;
  name: string;
  args: Record<string, unknown>;
  state: ToolState;
  output?: string;
  /** 审批原因（命中规则 / 内置危险 / 模型标记 / 全部审核） */
  reason?: string;
  /** request 事件到达时刻，用于倒计时 */
  requestedAt?: number;
  timeoutSecs?: number;
}

/** 工具轮次达到上限后的继续确认状态 */
interface RoundsPrompt {
  roundsDone: number;
  batch: number;
  /** waiting=等待选择，continued=已继续，stopped=已停止 */
  state: 'waiting' | 'continued' | 'stopped';
  /** 是否因等待超时而自动停止 */
  timedOut?: boolean;
}

interface ChatMsg {
  id: number;
  role: 'user' | 'assistant';
  content: string;
  /** 用户消息附带的图片（data URL），气泡里渲染缩略图 */
  images?: string[];
  tools: ToolView[];
  error?: string;
}

interface Props {
  sessionId: number;
  hostId: string;
  hostName: string;
  providerLabel: string;
  providerConfigured: boolean;
  models: AiModel[];
  providerId: string | null;
  /** 面板隐藏时保持挂载（不中断运行中的 AI 会话），仅隐藏显示 */
  hidden?: boolean;
  /** 由终端「问 AI」传入的选中文本，seq 用于区分重复内容 */
  insertText?: { text: string; seq: number } | null;
  /** 消费完 insertText 后回调，避免切换标签后重复插入 */
  onInsertConsumed?: () => void;
  onOpenConfig: () => void;
  onModelSwitched: () => void;
  onClose: () => void;
}

type PermissionMode = 'all' | 'smart' | 'none';

const PERMISSION_OPTIONS: { value: PermissionMode; labelKey: DictKey }[] = [
  { value: 'all', labelKey: 'chat.permissionAll' },
  { value: 'smart', labelKey: 'chat.permissionSmart' },
  { value: 'none', labelKey: 'chat.permissionNone' },
];

/** 安全级别提示文案键（按当前模式） */
const PERMISSION_TIP_KEY: Record<PermissionMode, DictKey> = {
  all: 'chat.tipAll',
  smart: 'chat.tipSmart',
  none: 'chat.tipNone',
};

function safeParseArgs(raw: string): Record<string, unknown> {
  try {
    return JSON.parse(raw);
  } catch {
    return {};
  }
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

// 上下文圆环：半径 15（viewBox 36），用于把百分比换算成 dash 长度
const RING_R = 15;
const RING_C = 2 * Math.PI * RING_R;

// 引用卡片预览取第一行非空文本
function refPreview(text: string): string {
  const line = text.split('\n').find((l) => l.trim());
  return (line ?? text).trim();
}

// ---- 图片附件 ----
/** 单条消息最多附带的图片数（与后端 MAX_IMAGES_PER_MESSAGE 一致） */
const MAX_IMAGES = 6;
/** 长边超过该值时经 canvas 压缩；与 Claude/OpenAI 高清档推荐分辨率一致 */
const IMAGE_MAX_EDGE = 1568;
/** 原图小于该体积且尺寸达标时跳过压缩，直发原始 data URL（≈500KB） */
const IMAGE_SKIP_COMPRESS_BYTES = 512 * 1024;

function readAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result));
    r.onerror = () => reject(r.error);
    r.readAsDataURL(file);
  });
}

function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error('image decode failed'));
    img.src = src;
  });
}

/**
 * 图片 → 发送用 data URL：尺寸与体积都达标时原样直发；
 * 否则 canvas 缩到长边 1568、JPEG 0.85 重编码（截图场景体积可降一个量级）。
 */
async function imageToDataUrl(file: File): Promise<string> {
  const src = await readAsDataUrl(file);
  const img = await loadImage(src);
  const longest = Math.max(img.width, img.height);
  if (file.size <= IMAGE_SKIP_COMPRESS_BYTES && longest <= IMAGE_MAX_EDGE) {
    return src;
  }
  const scale = Math.min(1, IMAGE_MAX_EDGE / longest);
  const w = Math.max(1, Math.round(img.width * scale));
  const h = Math.max(1, Math.round(img.height * scale));
  const canvas = document.createElement('canvas');
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext('2d');
  if (!ctx) return src;
  ctx.drawImage(img, 0, 0, w, h);
  return canvas.toDataURL('image/jpeg', 0.85);
}

function refLineCount(text: string): number {
  return text.split('\n').length;
}

// 非命令类工具把参数翻成人话，避免直接展示 JSON
function describeToolArgs(
  name: string,
  args: Record<string, unknown>,
  t: TFunc,
): string | null {
  const path = typeof args.path === 'string' ? args.path : null;
  switch (name) {
    case 'read_file':
      return path
        ? t('chat.toolReadPath', { path })
        : t('chat.toolReadFile');
    case 'list_dir':
      return path
        ? t('chat.toolListDirPath', { path })
        : t('chat.toolListDir');
    case 'resource_usage':
      return t('chat.toolResource');
    case 'query_history': {
      const metric = typeof args.metric === 'string' ? args.metric : '';
      return metric
        ? t('chat.toolTrendMetric', { metric })
        : t('chat.toolTrendGeneric');
    }
    default:
      return null;
  }
}

// 审批倒计时：request 事件附带 timeout_secs，超时后端按拒绝处理
function ApprovalCountdown({
  requestedAt,
  timeoutSecs,
}: {
  requestedAt: number;
  timeoutSecs: number;
}) {
  const t = useT();
  const [remaining, setRemaining] = useState(() =>
    Math.max(0, timeoutSecs - Math.floor((Date.now() - requestedAt) / 1000)),
  );
  useEffect(() => {
    const timer = window.setInterval(() => {
      setRemaining(
        Math.max(0, timeoutSecs - Math.floor((Date.now() - requestedAt) / 1000)),
      );
    }, 500);
    return () => window.clearInterval(timer);
  }, [requestedAt, timeoutSecs]);
  const pct = timeoutSecs > 0 ? Math.max(0, (remaining / timeoutSecs) * 100) : 0;
  return (
    <div className="approval-countdown">
      <div className="approval-countdown-bar">
        <span style={{ width: `${pct}%` }} />
      </div>
      <span className="approval-countdown-text">
        {remaining > 0
          ? t('chat.countdown', { n: remaining })
          : t('chat.countdownTimeout')}
      </span>
    </div>
  );
}

// 工具输出默认折叠（180px 可滚动），长输出可展开，右上角常驻复制
function ToolOutput({ output }: { output: string }) {
  const t = useT();
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const long = output.length > 400 || output.split('\n').length > 10;
  return (
    <div className={`tool-output-wrap${expanded ? ' expanded' : ''}`}>
      <pre className="tool-output">{output}</pre>
      <div className="tool-output-actions">
        {long && (
          <button onClick={() => setExpanded((v) => !v)}>
            {expanded ? t('chat.collapse') : t('chat.expand')}
          </button>
        )}
        <button
          onClick={async () => {
            if (await copyToClipboard(output)) {
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1500);
            }
          }}
        >
          {copied ? t('chat.copied') : t('chat.copy')}
        </button>
      </div>
    </div>
  );
}

// 从 markdown 代码块节点递归提取纯文本，供复制按钮使用
function extractText(node: ReactNode): string {
  if (node == null || typeof node === 'boolean') return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) {
    return node.map((n) => extractText(n as ReactNode)).join('');
  }
  const props = (node as { props?: { children?: ReactNode } }).props;
  return props ? extractText(props.children) : '';
}

// markdown 代码块容器：右上角复制按钮
function MarkdownPre({ children }: { children?: ReactNode }) {
  const t = useT();
  const [copied, setCopied] = useState(false);
  return (
    <div className="md-code">
      <button
        className="md-code-copy"
        title={t('chat.copyCode')}
        onClick={async () => {
          if (await copyToClipboard(extractText(children))) {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1500);
          }
        }}
      >
        {copied ? t('chat.copied') : t('chat.copy')}
      </button>
      <pre>{children}</pre>
    </div>
  );
}

// 消息 ID 使用模块级单调递增序列，避免组件重新挂载后从 0 重新计数，
// 与恢复出来的历史消息 ID 发生冲突。
let chatMessageSeq = 0;
const nextChatMessageId = () => ++chatMessageSeq;

/// 把后端 OpenAI 格式的扁平历史还原成「user 一条 + assistant 一条（含工具卡片）」的展示结构。
function historyToMessages(history: HistoryEntry[], nextId: () => number): ChatMsg[] {
  const result: ChatMsg[] = [];
  let currentAssistant: ChatMsg | null = null;
  for (const entry of history) {
    if (entry.role === 'system') continue;
    if (entry.role === 'user') {
      currentAssistant = null;
      result.push({
        id: nextId(),
        role: 'user',
        content: entry.content ?? '',
        images: entry.images,
        tools: [],
      });
    } else if (entry.role === 'assistant') {
      if (!currentAssistant) {
        currentAssistant = { id: nextId(), role: 'assistant', content: '', tools: [] };
        result.push(currentAssistant);
      }
      if (entry.content) currentAssistant.content += entry.content;
      if (entry.tool_calls) {
        for (const tc of entry.tool_calls) {
          currentAssistant.tools.push({
            id: tc.id,
            name: tc.function.name,
            args: safeParseArgs(tc.function.arguments),
            state: 'result',
          });
        }
      }
    } else if (entry.role === 'tool') {
      if (currentAssistant && entry.tool_call_id) {
        const tool = currentAssistant.tools.find((t) => t.id === entry.tool_call_id);
        if (tool) tool.output = entry.content;
      }
    }
  }
  return result;
}

function ChatPanel({
  sessionId,
  hostId,
  hostName,
  providerLabel,
  providerConfigured,
  models,
  providerId,
  hidden = false,
  insertText = null,
  onInsertConsumed,
  onOpenConfig,
  onModelSwitched,
  onClose,
}: Props) {
  const t = useT();
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [roundsPrompt, setRoundsPrompt] = useState<RoundsPrompt | null>(null);
  const [permissionMode, setPermissionMode] = useState<'all' | 'smart' | 'none'>(
    () =>
      (localStorage.getItem('buffterm.permissionMode') as 'all' | 'smart' | 'none') ||
      'smart',
  );
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const [nearBottom, setNearBottom] = useState(true);
  const activeAssistantId = useRef<number | null>(null);
  const composingRef = useRef(false);
  // WebKit 会用「先 compositionend、后 Enter keydown」的顺序确认候选词，
  // 需要额外标记来识别这次回车（见 textarea 的 onKeyDown）
  const justComposedRef = useRef(false);
  const hasSentRef = useRef(false);
  const lastInsertSeqRef = useRef(0);
  const [plan, setPlan] = useState<TaskPlan | null>(null);
  const [contextUsage, setContextUsage] = useState<ContextUsage | null>(null);
  const [planExpanded, setPlanExpanded] = useState(true);
  // 待发送的引用卡片：text = 终端选中内容，file = 服务器文件路径
  const [references, setReferences] = useState<
    { id: number; kind: 'text' | 'file'; text: string }[]
  >([]);
  const refSeqRef = useRef(0);
  const [pickerOpen, setPickerOpen] = useState(false);
  // 待发送的图片附件（data URL，已经过压缩管线），仅视觉模型显示入口
  const [images, setImages] = useState<string[]>([]);
  const imageInputRef = useRef<HTMLInputElement | null>(null);
  const [imgNotice, setImgNotice] = useState<string | null>(null);

  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    setNearBottom(el.scrollTop + el.clientHeight >= el.scrollHeight - 40);
  }, []);

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    setNearBottom(true);
  }, []);

  const changePermissionMode = (mode: 'all' | 'smart' | 'none') => {
    setPermissionMode(mode);
    localStorage.setItem('buffterm.permissionMode', mode);
  };

  const currentModelId =
    models.find((m) => m.is_active)?.id ?? models[0]?.id ?? null;
  const currentModel =
    models.find((m) => m.id === currentModelId) ?? models[0] ?? null;
  const currentWindow = currentModel?.context_window ?? 0;
  const displayWindow = contextUsage?.window_tokens ?? currentWindow;
  const displayUsed = contextUsage?.used_tokens ?? 0;
  const windowPct =
    displayWindow > 0 ? (displayUsed / displayWindow) * 100 : 0;
  const ringDash = RING_C * Math.min(1, windowPct / 100);
  const ringTone = contextUsage?.warning
    ? ' warn'
    : windowPct > 70
    ? ' danger'
    : windowPct >= 50
    ? ' warn'
    : '';

  // 图片入口按当前模型的 supports_vision 开关显示；粘贴时未开启则给出提示
  const supportsVision = currentModel?.supports_vision === true;
  const imgNoticeTimer = useRef<number | undefined>(undefined);
  const flashImgNotice = useCallback((text: string) => {
    setImgNotice(text);
    window.clearTimeout(imgNoticeTimer.current);
    imgNoticeTimer.current = window.setTimeout(() => setImgNotice(null), 3000);
  }, []);

  const addImageFiles = useCallback(
    async (files: File[]) => {
      const imgs = files.filter((f) => f.type.startsWith('image/'));
      if (imgs.length === 0) return;
      for (const f of imgs) {
        try {
          const url = await imageToDataUrl(f);
          setImages((prev) =>
            prev.length >= MAX_IMAGES ? prev : [...prev, url],
          );
        } catch {
          flashImgNotice(t('chat.imageReadFailed'));
        }
      }
    },
    [flashImgNotice, t],
  );

  const handleModelChange = async (modelId: string) => {
    if (!providerId || !modelId) return;
    // 切换模型后旧模型的用量/预算不再适用，先清空，等下一轮 ai:context 事件。
    setContextUsage(null);
    try {
      await setActiveAiModel(providerId, modelId);
      onModelSwitched();
    } catch (err) {
      updateLastAssistant((m) => ({ ...m, error: fmtError(err) }));
    }
  };

  const updateLastAssistant = useCallback(
    (updater: (m: ChatMsg) => ChatMsg) => {
      const targetId = activeAssistantId.current;
      // 清空对话后 activeAssistantId 为 null，此时忽略迟到的流式事件，避免旧内容残留
      if (targetId === null) return;
      setMessages((prev) => {
        const idx = prev.findIndex((m) => m.id === targetId);
        if (idx === -1) return prev;
        const next = [...prev];
        next[idx] = updater(next[idx]);
        return next;
      });
    },
    [],
  );

  useEffect(() => {
    let cancelled = false;
    getHistory(hostId)
      .then((history) => {
        if (cancelled) return;
        // 历史加载返回前用户已经发出新消息时，不要用旧历史覆盖当前对话。
        if (hasSentRef.current) return;
        setMessages(historyToMessages(history, nextChatMessageId));
        activeAssistantId.current = null;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [hostId]);

  useEffect(() => {
    let cancelled = false;
    getTaskPlan(hostId)
      .then((p) => {
        if (!cancelled) setPlan(p);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [hostId]);

  // 切换模型/打开对话时，按当前完整历史 + 新模型窗口立即估算用量，
  // 而不是等下一轮对话；下一轮真实 ai:context 事件会用实测值覆盖它。
  useEffect(() => {
    if (currentWindow <= 0) {
      setContextUsage(null);
      return;
    }
    let cancelled = false;
    getContextUsage(
      hostId,
      sessionId,
      currentWindow,
      currentModel?.model ?? '',
    )
      .then((usage) => {
        if (!cancelled) setContextUsage(usage);
      })
      .catch((err) => {
        if (cancelled) return;
        setContextUsage({
          session_id: sessionId,
          used_tokens: 0,
          history_tokens: 0,
          budget_tokens: Math.round(currentWindow * 0.7),
          window_tokens: currentWindow,
          compressed_rounds: 0,
          strategy: 'none',
          estimated: true,
          calibrated: false,
          warning: fmtError(err),
        });
      });
    return () => {
      cancelled = true;
    };
  }, [hostId, sessionId, currentWindow, currentModel?.model]);

  // 终端「问 AI」：把选中内容作为一条引用卡片挂到输入框上方，
  // 并把焦点移到输入框，方便用户接着补充问题。
  useEffect(() => {
    if (!insertText || insertText.seq === lastInsertSeqRef.current) return;
    lastInsertSeqRef.current = insertText.seq;
    setReferences((prev) => [
      ...prev,
      { id: ++refSeqRef.current, kind: 'text', text: insertText.text },
    ]);
    onInsertConsumed?.();
    requestAnimationFrame(() => inputRef.current?.focus());
  }, [insertText, onInsertConsumed]);

  const addFileReference = useCallback((path: string) => {
    setReferences((prev) =>
      prev.some((r) => r.kind === 'file' && r.text === path)
        ? prev
        : [...prev, { id: ++refSeqRef.current, kind: 'file', text: path }],
    );
    requestAnimationFrame(() => inputRef.current?.focus());
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unStream: (() => void) | undefined;
    let unTool: (() => void) | undefined;
    let unDone: (() => void) | undefined;
    let unError: (() => void) | undefined;
    let unPlan: (() => void) | undefined;
    let unContext: (() => void) | undefined;
    let unRoundsExhausted: (() => void) | undefined;
    let unRoundsResolved: (() => void) | undefined;

    onAiStream((sid, delta) => {
        if (sid !== sessionId) return;
        updateLastAssistant((m) => ({ ...m, content: m.content + delta }));
      })
      .then((fn) => {
        if (cancelled) fn();
        else unStream = fn;
      });

    onAiTool((p) => {
        if (p.session_id !== sessionId) return;
        updateLastAssistant((m) => {
          const tools = [...m.tools];
          const idx = tools.findIndex((t) => t.id === p.tool_call_id);
          const prev = idx >= 0 ? tools[idx] : undefined;
          const tool: ToolView = {
            id: p.tool_call_id,
            name: p.name,
            args: p.args,
            state: p.state,
            output: p.output ?? undefined,
            reason: p.reason ?? prev?.reason,
            requestedAt: p.state === 'request' ? Date.now() : prev?.requestedAt,
            timeoutSecs:
              p.state === 'request' ? p.timeout_secs ?? undefined : prev?.timeoutSecs,
          };
          if (idx >= 0) tools[idx] = tool;
          else tools.push(tool);
          return { ...m, tools };
        });
      })
      .then((fn) => {
        if (cancelled) fn();
        else unTool = fn;
      });

    onAiDone((sid) => {
        if (sid === sessionId) setBusy(false);
      })
      .then((fn) => {
        if (cancelled) fn();
        else unDone = fn;
      });

    onAiError((sid, message) => {
        if (sid !== sessionId) return;
        updateLastAssistant((m) => ({ ...m, error: message }));
        setBusy(false);
      })
      .then((fn) => {
        if (cancelled) fn();
        else unError = fn;
      });

    onAiPlan((payload) => {
        if (payload.session_id !== sessionId) return;
        setPlan(payload.plan);
      })
      .then((fn) => {
        if (cancelled) fn();
        else unPlan = fn;
      });

    onAiRoundsExhausted((payload) => {
        if (payload.session_id !== sessionId) return;
        setRoundsPrompt({
          roundsDone: payload.rounds_done,
          batch: payload.batch,
          state: 'waiting',
        });
      })
      .then((fn) => {
        if (cancelled) fn();
        else unRoundsExhausted = fn;
      });

    onAiRoundsResolved((payload) => {
        if (payload.session_id !== sessionId) return;
        setRoundsPrompt((prev) =>
          prev
            ? {
                ...prev,
                state: payload.continued ? 'continued' : 'stopped',
                timedOut: payload.timed_out,
              }
            : prev,
        );
      })
      .then((fn) => {
        if (cancelled) fn();
        else unRoundsResolved = fn;
      });

    onAiContext((payload) => {
        if (payload.session_id !== sessionId) return;
        setContextUsage(payload);
      })
      .then((fn) => {
        if (cancelled) fn();
        else unContext = fn;
      });

    return () => {
      cancelled = true;
      unStream?.();
      unTool?.();
      unDone?.();
      unError?.();
      unPlan?.();
      unContext?.();
      unRoundsExhausted?.();
      unRoundsResolved?.();
      // 注意：不在这里调用 agentCancel —— 面板隐藏/切换不应中断运行中的 AI 会话，
      // 仅在用户点击「停止」按钮时取消（后端会话结束前事件照常收，重开面板可看历史）
    };
  }, [sessionId, updateLastAssistant]);

  // 流式输出时仅当用户本来就停在底部才自动跟随，避免上翻阅读被拽回
  useEffect(() => {
    const el = scrollRef.current;
    if (el && nearBottom) el.scrollTop = el.scrollHeight;
  }, [messages, busy, nearBottom]);

  const handleSend = () => {
    const text = input.trim();
    const refs = references;
    const imgs = images;
    if (
      (!text && refs.length === 0 && imgs.length === 0) ||
      busy ||
      !providerConfigured
    )
      return;
    // 引用统一放在问题之后：文件引用给出路径（交由模型用 read_file 读取），
    // 终端引用用代码块包裹
    const refBlock = refs.length
      ? refs
          .map((r) =>
            r.kind === 'file'
              ? t('chat.refFilePrefix', { path: r.text })
              : `\`\`\`\n${r.text}\n\`\`\``,
          )
          .join('\n\n')
      : '';
    const payload = text
      ? refBlock
        ? `${text}\n\n${refBlock}`
        : text
      : refBlock;
    const userMsg: ChatMsg = {
      id: nextChatMessageId(),
      role: 'user',
      content: payload,
      images: imgs.length > 0 ? imgs : undefined,
      tools: [],
    };
    const assistantMsg: ChatMsg = {
      id: nextChatMessageId(),
      role: 'assistant',
      content: '',
      tools: [],
    };
    activeAssistantId.current = assistantMsg.id;
    hasSentRef.current = true;
    setMessages((prev) => [...prev, userMsg, assistantMsg]);
    setInput('');
    setReferences([]);
    setImages([]);
    setRoundsPrompt(null);
    setBusy(true);
    agentChat(sessionId, payload, permissionMode, imgs).catch((err) => {
      updateLastAssistant((m) => ({ ...m, error: fmtError(err) }));
      setBusy(false);
    });
  };

  const handleApprove = (toolCallId: string, allow: boolean) => {
    agentApprove(sessionId, toolCallId, allow).catch((err) => {
      updateLastAssistant((m) => ({ ...m, error: fmtError(err) }));
    });
  };

  // 达到工具轮次上限后的选择：继续下一批 / 停止本次执行
  const handleContinue = (allow: boolean) => {
    setRoundsPrompt((prev) =>
      prev ? { ...prev, state: allow ? 'continued' : 'stopped' } : prev,
    );
    agentContinue(sessionId, allow).catch((err) => {
      updateLastAssistant((m) => ({ ...m, error: fmtError(err) }));
      setBusy(false);
    });
  };

  const renderTool = (tool: ToolView) => (
    <div className={`tool-card tool-${tool.state}`} key={tool.id}>
      <div className="tool-head">
        <span className="tool-name">{tool.name}</span>
        <span className="tool-state">
          {tool.state === 'request' && t('chat.toolStateRequest')}
          {tool.state === 'denied' && t('chat.toolStateDenied')}
          {tool.state === 'running' && t('chat.toolStateRunning')}
          {tool.state === 'result' && t('chat.toolStateResult')}
          {tool.state === 'error' && t('chat.toolStateError')}
        </span>
      </div>
      {tool.reason && <div className="tool-reason">{tool.reason}</div>}
      <div className="tool-args">
        {typeof tool.args.command === 'string' ? (
          <code className="tool-command">{tool.args.command}</code>
        ) : describeToolArgs(tool.name, tool.args, t) ? (
          <code className="tool-command">
            {describeToolArgs(tool.name, tool.args, t)}
          </code>
        ) : (
          <code>{JSON.stringify(tool.args, null, 2)}</code>
        )}
      </div>
      {tool.state === 'request' &&
        tool.requestedAt != null &&
        tool.timeoutSecs != null && (
          <ApprovalCountdown
            requestedAt={tool.requestedAt}
            timeoutSecs={tool.timeoutSecs}
          />
        )}
      {tool.state === 'request' && (
        <div className="tool-actions">
          <button className="btn primary small" onClick={() => handleApprove(tool.id, true)}>
            {t('chat.approve')}
          </button>
          <button className="btn ghost small" onClick={() => handleApprove(tool.id, false)}>
            {t('chat.deny')}
          </button>
        </div>
      )}
      {tool.state === 'running' && (
        <div className="tool-running">
          <span className="spinner" /> {t('chat.running')}
        </div>
      )}
      {tool.output && <ToolOutput output={tool.output} />}
    </div>
  );

  return (
    <aside
      className="chat-panel"
      style={{
        width: 'var(--right-panel-width, 384px)',
        display: hidden ? 'none' : undefined,
      }}
    >
      <div className="chat-header">
        <div className="chat-header-left">
          <span className="chat-ai-icon">
            <SparklesIcon size={15} />
          </span>
          <div className="chat-header-text">
            <span className="chat-title">AI Agent</span>
            <span className="chat-sub">
              {providerLabel || t('chat.noProvider')} · {hostName}
            </span>
          </div>
        </div>
        <div className="chat-header-actions">
          <button
            className="icon-btn"
            title={t('chat.clearTitle')}
            onClick={() => {
              // 后端 agent_reset 会同时停止运行中的循环并清空历史
              agentReset(hostId).catch(() => {});
              setMessages([]);
              activeAssistantId.current = null;
              hasSentRef.current = false;
              setBusy(false);
              setPlan(null);
              setContextUsage(null);
              setReferences([]);
            }}
          >
            <RefreshIcon size={15} />
          </button>
          <button className="icon-btn" title={t('chat.closeTitle')} onClick={onClose}>
            <XIcon size={15} />
          </button>
        </div>
      </div>

      <div className="chat-messages" ref={scrollRef} onScroll={handleScroll}>
        {plan && (
          <div className="task-plan-card">
            <button
              type="button"
              className="task-plan-head"
              onClick={() => setPlanExpanded((v) => !v)}
              title={planExpanded ? t('chat.planCollapse') : t('chat.planExpand')}
            >
              <span className="task-plan-title">{t('chat.planTitle')}</span>
              <span className="task-plan-progress">
                {plan.completed.length}/{plan.completed.length + plan.pending.length}
              </span>
              <span className="task-plan-toggle">
                {planExpanded ? t('chat.collapse') : t('chat.expand')}
              </span>
            </button>
            {plan.goal && <div className="task-plan-goal">{plan.goal}</div>}
            {plan.current_step && (
              <div className="task-plan-step">
                {t('chat.planCurrent', { step: plan.current_step })}
              </div>
            )}
            {planExpanded && (
              <div className="task-plan-details">
                {plan.constraints.length > 0 && (
                  <div>
                    <strong>{t('chat.planConstraints')}</strong>
                    <ul>
                      {plan.constraints.map((item, i) => (
                        <li key={`c${i}`}>{item}</li>
                      ))}
                    </ul>
                  </div>
                )}
                {plan.completed.length > 0 && (
                  <div>
                    <strong>{t('chat.planCompleted')}</strong>
                    <ul>
                      {plan.completed.map((item, i) => (
                        <li key={`d${i}`}>{item}</li>
                      ))}
                    </ul>
                  </div>
                )}
                {plan.pending.length > 0 && (
                  <div>
                    <strong>{t('chat.planPending')}</strong>
                    <ul>
                      {plan.pending.map((item, i) => (
                        <li key={`p${i}`}>{item}</li>
                      ))}
                    </ul>
                  </div>
                )}
                {plan.failed.length > 0 && (
                  <div className="task-plan-failed">
                    <strong>{t('chat.planFailed')}</strong>
                    <ul>
                      {plan.failed.map((item, i) => (
                        <li key={`f${i}`}>{item}</li>
                      ))}
                    </ul>
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        {messages.length === 0 && (
          <div className="chat-empty">
            <SparklesIcon size={26} />
            <p>{t('chat.emptyTitle')}</p>
            <span>{t('chat.emptyHint')}</span>
          </div>
        )}

        {messages.map((msg) => {
          const emptyAssistant =
            msg.role === 'assistant' &&
            !msg.content &&
            msg.tools.length === 0 &&
            !msg.error;
          if (emptyAssistant) return null;
          return (
            <div key={msg.id} className={`msg msg-${msg.role}`}>
              <div className="msg-bubble">
                {msg.tools.map(renderTool)}
                {msg.images && msg.images.length > 0 && (
                  <div className="msg-images">
                    {msg.images.map((src, i) => (
                      <img key={i} src={src} alt={t('chat.imageAlt', { n: i + 1 })} />
                    ))}
                  </div>
                )}
                {msg.content && (
                  <div className="md-content">
                    <ReactMarkdown
                      remarkPlugins={[remarkGfm]}
                      components={{ pre: MarkdownPre }}
                    >
                      {msg.content}
                    </ReactMarkdown>
                  </div>
                )}
                {msg.error && <div className="msg-error">{msg.error}</div>}
              </div>
            </div>
          );
        })}

        {roundsPrompt && roundsPrompt.state !== 'continued' && (
          <div className="msg msg-assistant">
            <div className="msg-bubble rounds-prompt">
              {roundsPrompt.state === 'waiting' ? (
                <>
                  <div className="rounds-prompt-text">
                    {t('chat.roundsLimit', {
                      done: roundsPrompt.roundsDone,
                      batch: roundsPrompt.batch,
                    })}
                  </div>
                  <div className="rounds-prompt-actions">
                    <button
                      className="btn secondary small"
                      onClick={() => handleContinue(false)}
                    >
                      {t('chat.stop')}
                    </button>
                    <button
                      className="btn primary small"
                      onClick={() => handleContinue(true)}
                    >
                      {t('chat.continue')}
                    </button>
                  </div>
                </>
              ) : (
                <div className="rounds-prompt-text">
                  {roundsPrompt.timedOut
                    ? t('chat.roundsTimeout')
                    : t('chat.roundsStopped')}
                </div>
              )}
            </div>
          </div>
        )}

        {busy && roundsPrompt?.state !== 'waiting' && (
          <div className="msg msg-assistant">
            <div className="msg-bubble msg-thinking">
              <span className="spinner" /> {t('chat.thinking')}
            </div>
          </div>
        )}

        {!nearBottom && (
          <button
            className="chat-scroll-bottom"
            onClick={scrollToBottom}
            title={t('chat.scrollBottom')}
          >
            ↓ {t('chat.scrollBottom')}
          </button>
        )}
      </div>

      <div className="chat-footer">
        {!providerConfigured && (
          <div className="chat-noconfig">
            <span>{t('chat.noConfig')}</span>
            <button className="btn secondary small" onClick={onOpenConfig}>
              {t('chat.goConfig')}
            </button>
          </div>
        )}
        {references.length > 0 && (
          <div className="chat-refs">
            {references.map((ref) => (
              <div className="chat-ref" key={ref.id}>
                <span
                  className={`chat-ref-icon${ref.kind === 'file' ? ' file' : ''}`}
                >
                  {ref.kind === 'file' ? (
                    <FileIcon size={14} />
                  ) : (
                    <TerminalIcon size={14} />
                  )}
                </span>
                <div className="chat-ref-main">
                  <div className="chat-ref-title">
                    {ref.kind === 'file' ? baseName(ref.text) : refPreview(ref.text)}
                  </div>
                  <div className="chat-ref-sub">
                    {ref.kind === 'file'
                      ? t('chat.refFile', { path: ref.text })
                      : t('chat.refTerminal', { n: refLineCount(ref.text) })}
                  </div>
                </div>
                <button
                  type="button"
                  className="chat-ref-remove"
                  title={t('chat.removeRef')}
                  onClick={() =>
                    setReferences((prev) => prev.filter((r) => r.id !== ref.id))
                  }
                >
                  <XIcon size={13} />
                </button>
                <div className="chat-ref-popover">
                  <pre>{ref.text}</pre>
                </div>
              </div>
            ))}
          </div>
        )}
        {images.length > 0 && (
          <div className="chat-imgs">
            {images.map((src, i) => (
              <div className="chat-img-chip" key={i}>
                <img src={src} alt={t('chat.pendingImageAlt', { n: i + 1 })} />
                <button
                  type="button"
                  className="chat-img-remove"
                  title={t('chat.removeImage')}
                  onClick={() =>
                    setImages((prev) => prev.filter((_, x) => x !== i))
                  }
                >
                  <XIcon size={11} />
                </button>
              </div>
            ))}
          </div>
        )}
        <div className="chat-input-row">
          <div className="chat-input-box">
            <textarea
              ref={inputRef}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onPaste={(e) => {
                const items = e.clipboardData?.items;
                if (!items) return;
                const files = [...items]
                  .filter((it) => it.kind === 'file' && it.type.startsWith('image/'))
                  .map((it) => it.getAsFile())
                  .filter((f): f is File => f !== null);
                if (files.length === 0) return;
                e.preventDefault();
                if (!supportsVision) {
                  flashImgNotice(t('chat.visionDisabled'));
                  return;
                }
                void addImageFiles(files);
              }}
              onKeyDown={(e) => {
                if (e.key !== 'Enter' || e.shiftKey) return;
                // 输入法组合中（候选词列表还开着）的回车只用于确认候选词，不能发送。
                // WebKit 在「回车确认候选词」时会先派发 compositionend、再派发 Enter keydown，
                // 此时 isComposing 已是 false，只能靠 justComposedRef 拦下这一下。
                if (
                  e.nativeEvent.isComposing ||
                  e.nativeEvent.keyCode === 229 ||
                  composingRef.current ||
                  justComposedRef.current
                ) {
                  return;
                }
                e.preventDefault();
                handleSend();
              }}
              onCompositionStart={() => {
                composingRef.current = true;
              }}
              onCompositionEnd={() => {
                composingRef.current = false;
                // 标记「刚刚结束一次组合」，在下一个事件循环前拦掉紧随其后的确认回车
                justComposedRef.current = true;
                window.setTimeout(() => {
                  justComposedRef.current = false;
                }, 0);
              }}
              placeholder={
                providerConfigured
                  ? t('chat.inputPlaceholder')
                  : t('chat.inputPlaceholderNoConfig')
              }
              rows={2}
              disabled={!providerConfigured}
            />
            {providerConfigured && !input && (
              <span className="chat-input-hint">{t('chat.inputHint')}</span>
            )}
            {imgNotice && <span className="chat-img-notice">{imgNotice}</span>}
            {busy ? (
              <button
                className="icon-btn stop"
                title={t('chat.stopTitle')}
                onClick={() => {
                  setBusy(false);
                  agentCancel(sessionId).catch(() => {});
                }}
              >
                <StopIcon size={16} />
              </button>
            ) : (
              <button
                className="icon-btn send"
                title={t('chat.sendTitle')}
                onClick={handleSend}
                disabled={
                  !providerConfigured ||
                  (!input.trim() &&
                    references.length === 0 &&
                    images.length === 0)
                }
              >
                <SendIcon size={16} />
              </button>
            )}
          </div>
        </div>
        <div className="chat-bottom-bar">
          <div className="chat-bottom-left">
            <button
              type="button"
              className="icon-btn chat-attach-btn"
              title={t('chat.attachFile')}
              onClick={() => setPickerOpen(true)}
              disabled={!providerConfigured}
            >
              <PlusIcon size={16} />
            </button>
            {supportsVision && (
              <>
                <input
                  ref={imageInputRef}
                  type="file"
                  accept="image/*"
                  multiple
                  hidden
                  onChange={(e) => {
                    const files = [...(e.target.files ?? [])];
                    e.target.value = '';
                    void addImageFiles(files);
                  }}
                />
                <button
                  type="button"
                  className="icon-btn chat-attach-btn"
                  title={t('chat.attachImage')}
                  onClick={() => imageInputRef.current?.click()}
                  disabled={!providerConfigured}
                >
                  <ImageIcon size={16} />
                </button>
              </>
            )}
            <div
              className={`chat-control chat-control-permission permission-${permissionMode}`}
              title={t('chat.permissionTitle', {
                label: t(
                  PERMISSION_OPTIONS.find((o) => o.value === permissionMode)!
                    .labelKey,
                ),
              })}
            >
              <Select
                className="select-up"
                value={permissionMode}
                options={PERMISSION_OPTIONS.map((o) => ({
                  value: o.value,
                  label: t(o.labelKey),
                }))}
                onChange={changePermissionMode}
                ariaLabel={t('chat.permissionAria')}
                icon={<ShieldIcon size={13} />}
              />
            </div>
            {models.length > 0 && providerId && (
              <div className="chat-control chat-control-model" title={t('chat.modelTitle')}>
                <Select
                  className="select-up"
                  value={currentModelId ?? ''}
                  options={models.map((m) => ({ value: m.id, label: m.label }))}
                  onChange={handleModelChange}
                  ariaLabel={t('chat.modelAria')}
                />
              </div>
            )}
          </div>
          <div className="chat-bottom-right">
            {(contextUsage || currentWindow > 0) && (
              <div className={`context-ring${ringTone}`} tabIndex={0}>
                <svg className="context-ring-svg" viewBox="0 0 36 36">
                  <circle className="context-ring-track" cx="18" cy="18" r={RING_R} />
                  <circle
                    className="context-ring-fill"
                    cx="18"
                    cy="18"
                    r={RING_R}
                    strokeDasharray={`${ringDash} ${RING_C}`}
                    transform="rotate(-90 18 18)"
                  />
                </svg>
                <span className="context-ring-pct">{Math.round(windowPct)}%</span>
                <div className="context-ring-popover">
                  {contextUsage ? (
                    <>
                      <div className="context-ring-row">
                        <span>{t('chat.contextUsage')}</span>
                        <strong>
                          {formatTokens(contextUsage.used_tokens)} /{' '}
                          {formatTokens(contextUsage.window_tokens)}
                        </strong>
                      </div>
                      <div className="context-ring-row">
                        <span>{t('chat.modelWindow')}</span>
                        <strong>{formatTokens(displayWindow)}</strong>
                      </div>
                      <div className="context-ring-row">
                        <span>{t('chat.usageSource')}</span>
                        <strong>
                          {contextUsage.estimated
                            ? contextUsage.calibrated
                              ? t('chat.estimatedCalibrated')
                              : t('chat.estimatedLocal')
                            : t('chat.platformMeasured')}
                        </strong>
                      </div>
                      {contextUsage.compressed_rounds > 0 && (
                        <div className="context-ring-row">
                          <span>{t('chat.compressed')}</span>
                          <strong>
                            {t('chat.rounds', { n: contextUsage.compressed_rounds })}
                          </strong>
                        </div>
                      )}
                      {contextUsage.warning && (
                        <div className="context-ring-warning">
                          {contextUsage.warning}
                        </div>
                      )}
                    </>
                  ) : (
                    <>
                      <div className="context-ring-row">
                        <span>{t('chat.modelWindow')}</span>
                        <strong>{formatTokens(currentWindow)}</strong>
                      </div>
                      <div className="context-ring-row">
                        <span>{t('chat.status')}</span>
                        <strong>{t('chat.estimating')}</strong>
                      </div>
                    </>
                  )}
                </div>
              </div>
            )}
          </div>
        </div>
        <p className={`chat-tip tip-${permissionMode}`}>
          {t(PERMISSION_TIP_KEY[permissionMode])}
        </p>
      </div>
      {pickerOpen && (
        <RemoteFilePicker
          hostId={hostId}
          selectedPaths={references
            .filter((r) => r.kind === 'file')
            .map((r) => r.text)}
          onSelect={addFileReference}
          onClose={() => setPickerOpen(false)}
        />
      )}
    </aside>
  );
}

// props 由 App 稳定化，memo 避免 App 无关状态变化时重渲染（含 Markdown 重新解析）
export default memo(ChatPanel);
