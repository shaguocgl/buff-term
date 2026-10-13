import { useCallback, useEffect, useState } from 'react';
import { getAlertSettings, saveAlertSettings, testAlertSettings } from '../api';
import { useT } from '../i18n';
import type { AlertSettings } from '../types';
import { fmtError } from '../utils/errors';
import Modal from './Modal';
import Select from './Select';
import { BellIcon } from './Icons';

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
};

export default function AlertModal({ onClose }: Props) {
  const t = useT();
  const [settings, setSettings] = useState<AlertSettings>(EMPTY_SETTINGS);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);

  const load = useCallback(async () => {
    const st = await getAlertSettings();
    setSettings({ ...EMPTY_SETTINGS, ...st });
  }, []);

  useEffect(() => {
    load().catch((e) => setError(fmtError(e)));
  }, [load]);

  const handleSave = async () => {
    setError(null);
    setTestResult(null);
    setSaving(true);
    try {
      await saveAlertSettings(settings);
      setTestResult(t('alert.saved'));
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
      setTestResult(`${res.ok ? '✓' : '!'} ${res.message}`);
    } catch (e) {
      setTestResult(`! ${fmtError(e)}`);
    } finally {
      setTesting(false);
    }
  };

  const set = <K extends keyof AlertSettings>(key: K, value: AlertSettings[K]) =>
    setSettings((prev) => ({ ...prev, [key]: value }));

  return (
    <Modal
      title={t('alert.title')}
      subtitle={t('alert.subtitle')}
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <div className="alert-smtp">
          <div className="alert-settings-title">
            <BellIcon size={15} /> {t('alert.smtpTitle')}
          </div>

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
              placeholder={t('alert.passwordPlaceholder')}
            />
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

          {error && <p className="mcp-error">{error}</p>}
          {testResult && (
            <p
              className={`test-result ${testResult.startsWith('✓') ? 'ok' : 'err'}`}
            >
              {testResult}
            </p>
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
      </div>
    </Modal>
  );
}
