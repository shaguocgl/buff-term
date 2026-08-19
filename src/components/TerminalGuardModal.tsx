import { useEffect, useState } from 'react';
import {
  addTerminalRule,
  deleteTerminalRule,
  getTerminalGuardSettings,
  listTerminalRules,
  resetTerminalRules,
  saveTerminalGuardSettings,
} from '../api';
import type { TerminalGuardSettings, TerminalRule } from '../types';
import Modal from './Modal';
import { RefreshIcon, ShieldIcon } from './Icons';

interface Props {
  onClose: () => void;
}

export default function TerminalGuardModal({ onClose }: Props) {
  const [settings, setSettings] = useState<TerminalGuardSettings | null>(null);
  const [rules, setRules] = useState<TerminalRule[]>([]);
  const [ruleInput, setRuleInput] = useState('');
  const [timeoutInput, setTimeoutInput] = useState('60');
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getTerminalGuardSettings()
      .then((s) => {
        setSettings(s);
        setTimeoutInput(String(s.timeout_secs));
      })
      .catch((e) => setError(String(e)));
    listTerminalRules()
      .then(setRules)
      .catch((e) => setError(String(e)));
  }, []);

  const updateSettings = async (patch: Partial<TerminalGuardSettings>) => {
    if (!settings) return;
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      const saved = await saveTerminalGuardSettings(next);
      setSettings(saved);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleTimeout = async () => {
    const secs = Number(timeoutInput);
    if (!Number.isFinite(secs) || secs < 10) {
      setError('超时秒数需为 ≥10 的数字');
      return;
    }
    setError(null);
    await updateSettings({ timeout_secs: Math.round(secs) });
  };

  const handleAddRule = async () => {
    const pattern = ruleInput.trim();
    if (!pattern) return;
    setError(null);
    try {
      const rule = await addTerminalRule(pattern);
      setRules((prev) => [rule, ...prev]);
      setRuleInput('');
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDeleteRule = async (id: string) => {
    setError(null);
    try {
      await deleteTerminalRule(id);
      setRules((prev) => prev.filter((r) => r.id !== id));
    } catch (e) {
      setError(String(e));
    }
  };

  const handleReset = async () => {
    if (
      !window.confirm(
        '将删除所有预置规则并恢复为默认清单，自定义规则保留。确定吗？',
      )
    ) {
      return;
    }
    setError(null);
    try {
      const next = await resetTerminalRules();
      setRules(next);
    } catch (e) {
      setError(String(e));
    }
  };

  const builtins = rules.filter((r) => r.builtin);
  const customs = rules.filter((r) => !r.builtin);

  return (
    <Modal
      title="终端防护"
      subtitle="交互终端输入高危命令时，回车前拦截并弹窗确认"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="mcp-service-modal">
        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>
              <ShieldIcon size={14} /> 危险命令拦截
            </strong>
            <span>命中规则的命令需确认后才真正执行</span>
          </div>
          <div className="form-row-inline guard-toggle-row">
            <span className="form-label">
              {settings?.enabled ? '已启用' : '未启用'}
            </span>
            <button
              className={`switch${settings?.enabled ? ' on' : ''}`}
              onClick={() => updateSettings({ enabled: !settings?.enabled })}
              aria-label="启用终端危险命令拦截"
            >
              <span />
            </button>
          </div>
          <div className="form-row-inline guard-timeout-row">
            <span className="form-label">审批超时（秒）</span>
            <div className="guard-timeout-input">
              <input
                type="number"
                min={10}
                value={timeoutInput}
                onChange={(e) => setTimeoutInput(e.target.value)}
                onBlur={handleTimeout}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                    handleTimeout();
                  }
                }}
              />
            </div>
          </div>
          <p className="mcp-hint">
            拦截只防误操作，不构成安全边界；vim/htop 等全屏应用内的命令、方向键历史回放
            无法可靠识别（默认放行）。
          </p>
        </div>

        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>自定义危险命令</strong>
            <span>子串匹配，无需通配符，如 rm -rf</span>
          </div>
          <div className="rule-add">
            <input
              value={ruleInput}
              onChange={(e) => setRuleInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  handleAddRule();
                }
              }}
              placeholder="如 rm -rf，子串匹配，大小写不敏感"
            />
            <button
              className="btn secondary small"
              onClick={handleAddRule}
              disabled={!ruleInput.trim()}
            >
              添加
            </button>
          </div>
          {customs.length > 0 ? (
            <div className="mcp-rule-list">
              {customs.map((rule) => (
                <span className="rule-chip" key={rule.id}>
                  <code>{rule.pattern}</code>
                  <button
                    className="rule-del"
                    title="删除"
                    onClick={() => handleDeleteRule(rule.id)}
                  >
                    ×
                  </button>
                </span>
              ))}
            </div>
          ) : (
            <p className="mcp-hint">还没有自定义规则。</p>
          )}
        </div>

        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>预置危险命令</strong>
            <span>
              {builtins.length} 条 · 可单独删除
              <button
                className="btn ghost small guard-reset-btn"
                onClick={handleReset}
                title="恢复预置规则"
              >
                <RefreshIcon size={13} /> 恢复预置
              </button>
            </span>
          </div>
          {builtins.length > 0 ? (
            <div className="mcp-rule-list">
              {builtins.map((rule) => (
                <span className="rule-chip" key={rule.id}>
                  <code>{rule.pattern}</code>
                  <button
                    className="rule-del"
                    title="删除"
                    onClick={() => handleDeleteRule(rule.id)}
                  >
                    ×
                  </button>
                </span>
              ))}
            </div>
          ) : (
            <p className="mcp-hint">预置规则已被删除，可点「恢复预置」还原。</p>
          )}
        </div>

        {error && <span className="mcp-error">{error}</span>}
      </div>
    </Modal>
  );
}
