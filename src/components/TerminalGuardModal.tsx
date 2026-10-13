import { useEffect, useState } from 'react';
import {
  addTerminalRule,
  deleteTerminalRule,
  getTerminalGuardSettings,
  listTerminalRules,
  resetTerminalRules,
  saveTerminalGuardSettings,
} from '../api';
import { useT } from '../i18n';
import type { TerminalGuardSettings, TerminalRule } from '../types';
import { fmtError } from '../utils/errors';
import ConfirmModal from './ConfirmModal';
import Modal from './Modal';
import { RefreshIcon, ShieldIcon } from './Icons';

interface Props {
  onClose: () => void;
}

export default function TerminalGuardModal({ onClose }: Props) {
  const t = useT();
  const [settings, setSettings] = useState<TerminalGuardSettings | null>(null);
  const [rules, setRules] = useState<TerminalRule[]>([]);
  const [ruleInput, setRuleInput] = useState('');
  const [timeoutInput, setTimeoutInput] = useState('60');
  const [error, setError] = useState<string | null>(null);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  useEffect(() => {
    getTerminalGuardSettings()
      .then((s) => {
        setSettings(s);
        setTimeoutInput(String(s.timeout_secs));
      })
      .catch((e) => setError(fmtError(e)));
    listTerminalRules()
      .then(setRules)
      .catch((e) => setError(fmtError(e)));
  }, []);

  const updateSettings = async (patch: Partial<TerminalGuardSettings>) => {
    if (!settings) return;
    const prev = settings;
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      const saved = await saveTerminalGuardSettings(next);
      setSettings(saved);
    } catch (e) {
      // 保存失败回滚乐观更新，避免 UI 显示与后端实际状态不一致
      setSettings(prev);
      setError(fmtError(e));
    }
  };

  const handleTimeout = async () => {
    const secs = Number(timeoutInput);
    if (!Number.isFinite(secs) || secs < 10) {
      setError(t('guard.timeoutInvalid'));
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
      setError(fmtError(e));
    }
  };

  const handleDeleteRule = async (id: string) => {
    setError(null);
    try {
      await deleteTerminalRule(id);
      setRules((prev) => prev.filter((r) => r.id !== id));
    } catch (e) {
      setError(fmtError(e));
    }
  };

  const handleReset = () => {
    setShowResetConfirm(true);
  };

  const doReset = async () => {
    setShowResetConfirm(false);
    setError(null);
    try {
      const next = await resetTerminalRules();
      setRules(next);
    } catch (e) {
      setError(fmtError(e));
    }
  };

  const builtins = rules.filter((r) => r.builtin);
  const customs = rules.filter((r) => !r.builtin);

  return (
    <Modal
      title={t('guard.title')}
      subtitle={t('guard.subtitle')}
      className="modal-wide"
      onClose={onClose}
    >
      <div className="mcp-service-modal">
        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>
              <ShieldIcon size={14} /> {t('guard.sectionTitle')}
            </strong>
            <span>{t('guard.sectionHint')}</span>
          </div>
          <div className="form-row-inline guard-toggle-row">
            <span className="form-label">
              {settings?.enabled ? t('guard.enabled') : t('guard.disabled')}
            </span>
            <button
              className={`switch${settings?.enabled ? ' on' : ''}`}
              onClick={() => updateSettings({ enabled: !settings?.enabled })}
              aria-label={t('guard.toggleAria')}
            >
              <span />
            </button>
          </div>
          <div className="form-row-inline guard-timeout-row">
            <span className="form-label">{t('guard.timeoutLabel')}</span>
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
          <p className="mcp-hint">{t('guard.timeoutHint')}</p>
        </div>

        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>{t('guard.customTitle')}</strong>
            <span>{t('guard.customHint')}</span>
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
              placeholder={t('guard.addPlaceholder')}
            />
            <button
              className="btn secondary small"
              onClick={handleAddRule}
              disabled={!ruleInput.trim()}
            >
              {t('common.add')}
            </button>
          </div>
          {customs.length > 0 ? (
            <div className="mcp-rule-list">
              {customs.map((rule) => (
                <span className="rule-chip" key={rule.id}>
                  <code>{rule.pattern}</code>
                  <button
                    className="rule-del"
                    title={t('common.delete')}
                    onClick={() => handleDeleteRule(rule.id)}
                  >
                    ×
                  </button>
                </span>
              ))}
            </div>
          ) : (
            <p className="mcp-hint">{t('guard.noCustom')}</p>
          )}
        </div>

        <div className="mcp-section">
          <div className="mcp-section-title">
            <strong>{t('guard.builtinTitle')}</strong>
            <span>
              {t('guard.builtinCount', { n: builtins.length })}
              <button
                className="btn ghost small guard-reset-btn"
                onClick={handleReset}
                title={t('guard.restoreTitle')}
              >
                <RefreshIcon size={13} /> {t('guard.restore')}
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
                    title={t('common.delete')}
                    onClick={() => handleDeleteRule(rule.id)}
                  >
                    ×
                  </button>
                </span>
              ))}
            </div>
          ) : (
            <p className="mcp-hint">{t('guard.builtinEmpty')}</p>
          )}
        </div>

        {error && <span className="mcp-error">{error}</span>}
      </div>

      {showResetConfirm && (
        <ConfirmModal
          title={t('guard.restoreTitle')}
          body={t('guard.resetBody')}
          confirmText={t('guard.restore')}
          danger
          onConfirm={doReset}
          onCancel={() => setShowResetConfirm(false)}
        />
      )}
    </Modal>
  );
}
