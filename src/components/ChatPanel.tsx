import { useCallback, useEffect, useRef, useState } from 'react';
import {
  agentApprove,
  agentCancel,
  agentChat,
  agentReset,
  getHistory,
  onAiDone,
  onAiError,
  onAiStream,
  onAiTool,
  setActiveAiModel,
} from '../api';
import type { AiModel, HistoryEntry } from '../types';
import Select, { type SelectOption } from './Select';
import {
  RefreshIcon,
  SendIcon,
  SparklesIcon,
  StopIcon,
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
}

interface ChatMsg {
  id: number;
  role: 'user' | 'assistant';
  content: string;
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
  panelWidth?: number;
  onOpenConfig: () => void;
  onModelSwitched: () => void;
  onClose: () => void;
}

const PERMISSION_OPTIONS: SelectOption<'all' | 'smart' | 'none'>[] = [
  { value: 'all', label: '全部审核' },
  { value: 'smart', label: '智能审核' },
  { value: 'none', label: '全部放行' },
];

function safeParseArgs(raw: string): Record<string, unknown> {
  try {
    return JSON.parse(raw);
  } catch {
    return {};
  }
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
      result.push({ id: nextId(), role: 'user', content: entry.content ?? '', tools: [] });
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

export default function ChatPanel({
  sessionId,
  hostId,
  hostName,
  providerLabel,
  providerConfigured,
  models,
  providerId,
  panelWidth = 384,
  onOpenConfig,
  onModelSwitched,
  onClose,
}: Props) {
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [permissionMode, setPermissionMode] = useState<'all' | 'smart' | 'none'>(
    () =>
      (localStorage.getItem('buffterm.permissionMode') as 'all' | 'smart' | 'none') ||
      'smart',
  );
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const activeAssistantId = useRef<number | null>(null);
  const composingRef = useRef(false);
  const hasSentRef = useRef(false);

  const changePermissionMode = (mode: 'all' | 'smart' | 'none') => {
    setPermissionMode(mode);
    localStorage.setItem('buffterm.permissionMode', mode);
  };

  const currentModelId =
    models.find((m) => m.is_active)?.id ?? models[0]?.id ?? null;

  const handleModelChange = async (modelId: string) => {
    if (!providerId || !modelId) return;
    try {
      await setActiveAiModel(providerId, modelId);
      onModelSwitched();
    } catch (err) {
      updateLastAssistant((m) => ({ ...m, error: String(err) }));
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
    let unStream: (() => void) | undefined;
    let unTool: (() => void) | undefined;
    let unDone: (() => void) | undefined;
    let unError: (() => void) | undefined;

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
          const tool: ToolView = {
            id: p.tool_call_id,
            name: p.name,
            args: p.args,
            state: p.state,
            output: p.output ?? undefined,
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

    return () => {
      cancelled = true;
      unStream?.();
      unTool?.();
      unDone?.();
      unError?.();
      agentCancel(sessionId).catch(() => {});
    };
  }, [sessionId, updateLastAssistant]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);

  const handleSend = () => {
    const text = input.trim();
    if (!text || busy || !providerConfigured) return;
    const userMsg: ChatMsg = {
      id: nextChatMessageId(),
      role: 'user',
      content: text,
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
    setBusy(true);
    agentChat(sessionId, text, permissionMode).catch((err) => {
      updateLastAssistant((m) => ({ ...m, error: String(err) }));
      setBusy(false);
    });
  };

  const handleApprove = (toolCallId: string, allow: boolean) => {
    agentApprove(sessionId, toolCallId, allow).catch((err) => {
      updateLastAssistant((m) => ({ ...m, error: String(err) }));
    });
  };

  const renderTool = (tool: ToolView) => (
    <div className={`tool-card tool-${tool.state}`} key={tool.id}>
      <div className="tool-head">
        <span className="tool-name">{tool.name}</span>
        <span className="tool-state">
          {tool.state === 'request' && '等待审批'}
          {tool.state === 'denied' && '已拒绝'}
          {tool.state === 'running' && '执行中'}
          {tool.state === 'result' && '已完成'}
          {tool.state === 'error' && '出错'}
        </span>
      </div>
      <div className="tool-args">
        {typeof tool.args.command === 'string' ? (
          <code className="tool-command">{tool.args.command}</code>
        ) : (
          <code>{JSON.stringify(tool.args, null, 2)}</code>
        )}
      </div>
      {tool.state === 'request' && (
        <div className="tool-actions">
          <button className="btn primary small" onClick={() => handleApprove(tool.id, true)}>
            批准执行
          </button>
          <button className="btn ghost small" onClick={() => handleApprove(tool.id, false)}>
            拒绝
          </button>
        </div>
      )}
      {tool.state === 'running' && (
        <div className="tool-running">
          <span className="spinner" /> 执行中…
        </div>
      )}
      {tool.output && (
        <pre className="tool-output">{tool.output}</pre>
      )}
    </div>
  );

  return (
    <aside className="chat-panel" style={{ width: panelWidth }}>
      <div className="chat-header">
        <div className="chat-header-left">
          <span className="chat-ai-icon">
            <SparklesIcon size={15} />
          </span>
          <div className="chat-header-text">
            <span className="chat-title">AI Agent</span>
            <span className="chat-sub">
              {providerLabel || '未配置模型平台'} · {hostName}
            </span>
          </div>
        </div>
        <div className="chat-header-actions">
          <button
            className="icon-btn"
            title="清空对话"
            onClick={() => {
              // 后端 agent_reset 会同时停止运行中的循环并清空历史
              agentReset(sessionId, hostId).catch(() => {});
              setMessages([]);
              activeAssistantId.current = null;
              hasSentRef.current = false;
              setBusy(false);
            }}
          >
            <RefreshIcon size={15} />
          </button>
          <button className="icon-btn" title="关闭对话" onClick={onClose}>
            <XIcon size={15} />
          </button>
        </div>
      </div>

      <div className="chat-messages" ref={scrollRef}>
        {messages.length === 0 && (
          <div className="chat-empty">
            <SparklesIcon size={26} />
            <p>向 AI 描述你想做的事</p>
            <span>
              例如：“看看磁盘占用”“查一下 /var/log 里的报错”“部署这个服务”
            </span>
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
                {msg.content &&
                  (msg.role === 'assistant' ? (
                    <div className="md-content">
                      <ReactMarkdown remarkPlugins={[remarkGfm]}>
                        {msg.content}
                      </ReactMarkdown>
                    </div>
                  ) : (
                    <div className="msg-content">{msg.content}</div>
                  ))}
                {msg.error && <div className="msg-error">{msg.error}</div>}
              </div>
            </div>
          );
        })}

        {busy && (
          <div className="msg msg-assistant">
            <div className="msg-bubble msg-thinking">
              <span className="spinner" /> 思考中…
            </div>
          </div>
        )}
      </div>

      <div className="chat-footer">
        {!providerConfigured && (
          <div className="chat-noconfig">
            <span>还没有配置 AI 平台</span>
            <button className="btn secondary small" onClick={onOpenConfig}>
              去配置
            </button>
          </div>
        )}
        <div className="chat-controls">
          <div className={`chat-control permission-${permissionMode}`}>
            <span className="chat-control-label">
              安全级别
              {permissionMode === 'smart' && (
                <em className="permission-recommend">推荐</em>
              )}
            </span>
            <Select
              className="select-up"
              value={permissionMode}
              options={PERMISSION_OPTIONS}
              onChange={changePermissionMode}
              ariaLabel="安全级别"
            />
          </div>
          {models.length > 0 && providerId && (
            <div className="chat-control chat-control-model">
              <span className="chat-control-label">模型</span>
              <Select
                className="select-up"
                value={currentModelId ?? ''}
                options={models.map((m) => ({ value: m.id, label: m.label }))}
                onChange={handleModelChange}
                ariaLabel="模型"
              />
            </div>
          )}
        </div>
        <div className="chat-input-row">
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (
                e.key === 'Enter' &&
                !e.shiftKey &&
                !e.nativeEvent.isComposing &&
                !composingRef.current
              ) {
                e.preventDefault();
                handleSend();
              }
            }}
            onCompositionStart={() => {
              composingRef.current = true;
            }}
            onCompositionEnd={() => {
              composingRef.current = false;
            }}
            placeholder={providerConfigured ? '给 AI 下达指令…' : '请先配置 AI 平台'}
            rows={2}
            disabled={!providerConfigured}
          />
          {busy ? (
            <button className="icon-btn stop" title="停止" onClick={() => {
              setBusy(false);
              agentCancel(sessionId).catch(() => {});
            }}>
              <StopIcon size={16} />
            </button>
          ) : (
            <button
              className="icon-btn send"
              title="发送"
              onClick={handleSend}
              disabled={!providerConfigured || !input.trim()}
            >
              <SendIcon size={16} />
            </button>
          )}
        </div>
        <p className={`chat-tip tip-${permissionMode}`}>
          {permissionMode === 'all' &&
            '安全级别：全部审核 · 每个命令执行前都需要你批准'}
          {permissionMode === 'smart' &&
            '安全级别：智能审核 · 危险命令需批准，只读命令自动执行'}
          {permissionMode === 'none' &&
            '安全级别：全部放行 · 命令直接执行，请谨慎使用'}
          {' · Enter 发送 / Shift+Enter 换行'}
        </p>
      </div>
    </aside>
  );
}
