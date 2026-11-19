import { useCallback, useEffect, useRef, useState } from 'react';
import {
  clearAlertPassword,
  clearWebhookSecret,
  getAlertSettings,
  saveAlertSettings,
  testAlertSettings,
} from '../api';
import { useT } from '../i18n';
import type { AlertSettings } from '../types';
import { fmtError } from '../utils/errors';
import { copyToClipboard } from '../utils/clipboard';
import Modal from './Modal';
import Select from './Select';
import { BellIcon, CheckIcon, SendIcon } from './Icons';

interface Props {
  onClose: () => void;
}

const EMPTY_SETTINGS: AlertSettings = {
  smtp_host: '',
  smtp_port: 587,
  smtp_username: '',
  smtp_password: '',
  smtp_from: '',
  smtp_to: '',
  smtp_tls: 'starttls',
  webhook_kind: 'dingtalk',
  webhook_url: '',
  webhook_secret: '',
};

const WEBHOOK_KINDS: {
  value: NonNullable<AlertSettings['webhook_kind']>;
  labelKey:
    | 'alert.kindDingtalk'
    | 'alert.kindWecom'
    | 'alert.kindFeishu'
    | 'alert.kindSlack'
    | 'alert.kindDiscord'
    | 'alert.kindGoogleChat'
    | 'alert.kindTeams'
    | 'alert.kindGeneric';
}[] = [
  { value: 'dingtalk', labelKey: 'alert.kindDingtalk' },
  { value: 'wecom', labelKey: 'alert.kindWecom' },
  { value: 'feishu', labelKey: 'alert.kindFeishu' },
  { value: 'slack', labelKey: 'alert.kindSlack' },
  { value: 'discord', labelKey: 'alert.kindDiscord' },
  { value: 'googlechat', labelKey: 'alert.kindGoogleChat' },
  { value: 'teams', labelKey: 'alert.kindTeams' },
  { value: 'generic', labelKey: 'alert.kindGeneric' },
];

const URL_PLACEHOLDERS: Record<
  NonNullable<AlertSettings['webhook_kind']>,
  | 'alert.webhookUrlPlaceholder'
  | 'alert.webhookUrlPlaceholderWecom'
  | 'alert.webhookUrlPlaceholderFeishu'
  | 'alert.webhookUrlPlaceholderSlack'
  | 'alert.webhookUrlPlaceholderDiscord'
  | 'alert.webhookUrlPlaceholderGoogleChat'
  | 'alert.webhookUrlPlaceholderTeams'
  | 'alert.webhookUrlPlaceholderGeneric'
> = {
  dingtalk: 'alert.webhookUrlPlaceholder',
  wecom: 'alert.webhookUrlPlaceholderWecom',
  feishu: 'alert.webhookUrlPlaceholderFeishu',
  slack: 'alert.webhookUrlPlaceholderSlack',
  discord: 'alert.webhookUrlPlaceholderDiscord',
  googlechat: 'alert.webhookUrlPlaceholderGoogleChat',
  teams: 'alert.webhookUrlPlaceholderTeams',
  generic: 'alert.webhookUrlPlaceholderGeneric',
};

/** 通用自定义 JSON 的请求体示例。字段值随应用语言变化，这里只展示结构。 */
const GENERIC_SAMPLE = `{
  "source": "buffterm",
  "title": "[buffTerm] web-01 - high",
  "text": "**web-01 (root@10.0.0.5:22)**\\n\\nRisk level: high\\n\\nTrigger: scheduled smart monitoring\\n\\nRoot disk usage 96%, estimated to fill up within 6 hours",
  "risk": "high",
  "host": "web-01 (root@10.0.0.5:22)"
}`;

export default function AlertModal({ onClose }: Props) {
  const t = useT();
  const [tab, setTab] = useState<'smtp' | 'webhook'>('smtp');
  const [settings, setSettings] = useState<AlertSettings>(EMPTY_SETTINGS);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [testOk, setTestOk] = useState(false);
  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<number | null>(null);

  const load = useCallback(async () => {
    const st = await getAlertSettings();
    setSettings({ ...EMPTY_SETTINGS, ...st });
    // 只配了群机器人的用户直接落在对应页签，少点一次
    if (!st.smtp_host?.trim() && st.webhook_url?.trim()) setTab('webhook');
  }, []);

  useEffect(() => {
    load().catch((e) => setError(fmtError(e)));
  }, [load]);

  const handleSave = async () => {
    setError(null);
    setTestResult(null);
    setSaving(true);
    const typedPassword = settings.smtp_password?.trim() ?? '';
    const typedSecret = settings.webhook_secret?.trim() ?? '';
    try {
      await saveAlertSettings(settings);
      // 密码与加签密钥已单独加密保存；清空输入框，避免明文长期停留在组件状态。
      setSettings((prev) => ({
        ...prev,
        smtp_password: '',
        smtp_password_set: prev.smtp_password_set || typedPassword.length > 0,
        webhook_secret: '',
        webhook_secret_set: prev.webhook_secret_set || typedSecret.length > 0,
      }));
      setTestOk(true);
      setTestResult(t('alert.saved'));
    } catch (e) {
      setError(fmtError(e));
    } finally {
      setSaving(false);
    }
  };

  const handleClearPassword = async () => {
    setError(null);
    setTestResult(null);
    setSaving(true);
    try {
      await clearAlertPassword();
      setSettings((prev) => ({
        ...prev,
        smtp_password: '',
        smtp_password_set: false,
      }));
      setTestOk(true);
      setTestResult(t('alert.passwordCleared'));
    } catch (e) {
      setError(fmtError(e));
    } finally {
      setSaving(false);
    }
  };

  const handleClearWebhookSecret = async () => {
    setError(null);
    setTestResult(null);
    setSaving(true);
    try {
      await clearWebhookSecret();
      setSettings((prev) => ({
        ...prev,
        webhook_secret: '',
        webhook_secret_set: false,
      }));
      setTestOk(true);
      setTestResult(t('alert.webhookSecretCleared'));
    } catch (e) {
      setError(fmtError(e));
    } finally {
      setSaving(false);
    }
  };

  const handleTest = async () => {
    setError(null);
    setTestResult(null);
    setTesting(true);
    try {
      const res = await testAlertSettings(settings);
      setTestOk(res.ok);
      setTestResult(`${res.ok ? '✓' : '!'} ${res.message}`);
    } catch (e) {
      setTestOk(false);
      setTestResult(`! ${fmtError(e)}`);
    } finally {
      setTesting(false);
    }
  };

  const set = <K extends keyof AlertSettings>(key: K, value: AlertSettings[K]) =>
    setSettings((prev) => ({ ...prev, [key]: value }));

  const kind = settings.webhook_kind ?? 'dingtalk';

  const copySample = async () => {
    if (!(await copyToClipboard(GENERIC_SAMPLE))) return;
    setCopied(true);
    if (copyTimerRef.current !== null) window.clearTimeout(copyTimerRef.current);
    copyTimerRef.current = window.setTimeout(() => setCopied(false), 2000);
  };

  return (
    <Modal
      title={t('alert.title')}
      subtitle={t('alert.subtitle')}
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal alert-channels">
        <div className="segmented alert-tabs">
          <button
            type="button"
            className={tab === 'smtp' ? 'seg active' : 'seg'}
            onClick={() => setTab('smtp')}
          >
            <BellIcon size={14} /> {t('alert.smtpTitle')}
          </button>
          <button
            type="button"
            className={tab === 'webhook' ? 'seg active' : 'seg'}
            onClick={() => setTab('webhook')}
          >
            <SendIcon size={14} /> {t('alert.webhookTitle')}
          </button>
        </div>

        {tab === 'smtp' && (
        <div className="alert-smtp">
          <div className="alert-field">
            <span className="alert-field-label">{t('alert.smtpHost')}</span>
            <input
              value={settings.smtp_host ?? ''}
              onChange={(e) => set('smtp_host', e.target.value)}
              placeholder="smtp.example.com"
            />
          </div>

          <div className="alert-grid">
            <div className="alert-field field-110">
              <span className="alert-field-label">{t('alert.port')}</span>
              <input
                value={settings.smtp_port ?? ''}
                onChange={(e) =>
                  set('smtp_port', Number.parseInt(e.target.value, 10) || 587)
                }
                className="input-port"
              />
            </div>
            <div className="alert-field field-150">
              <span className="alert-field-label">{t('alert.tls')}</span>
              <Select
                value={settings.smtp_tls ?? 'starttls'}
                options={[
                  { value: 'starttls', label: t('alert.tlsStarttls') },
                  { value: 'ssl', label: t('alert.tlsSsl') },
                  { value: 'none', label: t('alert.tlsNone') },
                ]}
                onChange={(v) => set('smtp_tls', v)}
                ariaLabel={t('alert.tlsAria')}
              />
            </div>
          </div>
          {settings.smtp_tls === 'none' && (
            <p className="alert-warning">{t('alert.tlsNoneWarning')}</p>
          )}

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.username')}</span>
            <input
              value={settings.smtp_username ?? ''}
              onChange={(e) => set('smtp_username', e.target.value)}
              placeholder="user@example.com"
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.password')}</span>
            <input
              type="password"
              value={settings.smtp_password ?? ''}
              onChange={(e) => set('smtp_password', e.target.value)}
              placeholder={
                settings.smtp_password_set
                  ? t('alert.passwordSavedHint')
                  : t('alert.passwordPlaceholder')
              }
            />
            {settings.smtp_password_set && (
              <button
                type="button"
                className="alert-clear-password"
                onClick={() => void handleClearPassword()}
                disabled={saving}
              >
                {t('alert.clearPassword')}
              </button>
            )}
          </div>

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.from')}</span>
            <input
              value={settings.smtp_from ?? ''}
              onChange={(e) => set('smtp_from', e.target.value)}
              placeholder="buffTerm <alert@example.com>"
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.to')}</span>
            <input
              value={settings.smtp_to ?? ''}
              onChange={(e) => set('smtp_to', e.target.value)}
              placeholder="me@example.com, ops@example.com"
            />
          </div>
        </div>
        )}

        {tab === 'webhook' && (
        <div className="alert-smtp">

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.webhookKind')}</span>
            <Select
              value={kind}
              options={WEBHOOK_KINDS.map((k) => ({
                value: k.value,
                label: t(k.labelKey),
              }))}
              onChange={(v) => set('webhook_kind', v)}
              ariaLabel={t('alert.webhookKindAria')}
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">{t('alert.webhookUrl')}</span>
            <input
              value={settings.webhook_url ?? ''}
              onChange={(e) => set('webhook_url', e.target.value)}
              placeholder={t(
                URL_PLACEHOLDERS[kind] ?? 'alert.webhookUrlPlaceholder',
              )}
            />
          </div>

          {kind === 'generic' && (
            <div className="mcp-config-block">
              <div className="mcp-section-title">
                <strong>{t('alert.genericTitle')}</strong>
              </div>
              <p className="mcp-hint">{t('alert.genericRequest')}</p>
              <ul className="generic-fields">
                <li>
                  <code>source</code>
                  <span>{t('alert.genericFieldSource')}</span>
                </li>
                <li>
                  <code>title</code>
                  <span>{t('alert.genericFieldTitle')}</span>
                </li>
                <li>
                  <code>text</code>
                  <span>{t('alert.genericFieldText')}</span>
                </li>
                <li>
                  <code>risk</code>
                  <span>{t('alert.genericFieldRisk')}</span>
                </li>
                <li>
                  <code>host</code>
                  <span>{t('alert.genericFieldHost')}</span>
                </li>
              </ul>
              <pre className="mcp-config-json">{GENERIC_SAMPLE}</pre>
              <div className="mcp-config-actions">
                <button
                  type="button"
                  className="btn secondary small"
                  onClick={() => void copySample()}
                >
                  {copied ? (
                    <>
                      <CheckIcon size={14} /> {t('common.copied')}
                    </>
                  ) : (
                    t('alert.copySample')
                  )}
                </button>
              </div>
            </div>
          )}

          {(kind === 'dingtalk' || kind === 'feishu') && (
            <div className="alert-field">
              <span className="alert-field-label">{t('alert.webhookSecret')}</span>
              <input
                type="password"
                value={settings.webhook_secret ?? ''}
                onChange={(e) => set('webhook_secret', e.target.value)}
                placeholder={
                  settings.webhook_secret_set
                    ? t('alert.webhookSecretSavedHint')
                    : kind === 'feishu'
                      ? t('alert.webhookSecretPlaceholderFeishu')
                      : t('alert.webhookSecretPlaceholder')
                }
              />
              {settings.webhook_secret_set && (
                <button
                  type="button"
                  className="alert-clear-password"
                  onClick={() => void handleClearWebhookSecret()}
                  disabled={saving}
                >
                  {t('alert.clearWebhookSecret')}
                </button>
              )}
            </div>
          )}

          {kind !== 'generic' && (
            <p className="mcp-hint">{t('alert.webhookHint')}</p>
          )}
        </div>
        )}

        {error && <p className="mcp-error">{error}</p>}
        {testResult && (
          <p className={`test-result ${testOk ? 'ok' : 'err'}`}>{testResult}</p>
        )}

        <div className="form-actions">
          <button
            type="button"
            className="btn secondary"
            onClick={handleTest}
            disabled={testing || saving}
          >
            {testing ? t('alert.testing') : t('alert.testSend')}
          </button>
          <button
            type="button"
            className="btn primary"
            onClick={handleSave}
            disabled={saving}
          >
            {saving ? t('common.saving') : t('alert.saveSettings')}
          </button>
        </div>
      </div>
    </Modal>
  );
}
