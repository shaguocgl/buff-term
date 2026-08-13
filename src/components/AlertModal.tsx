import { useCallback, useEffect, useState } from 'react';
import { getAlertSettings, saveAlertSettings, testAlertSettings } from '../api';
import type { AlertSettings } from '../types';
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
    load().catch((e) => setError(String(e)));
  }, [load]);

  const handleSave = async () => {
    setError(null);
    setTestResult(null);
    setSaving(true);
    try {
      await saveAlertSettings(settings);
      setTestResult('✓ SMTP 设置已保存');
    } catch (e) {
      setError(String(e));
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
      setTestResult(`! ${e}`);
    } finally {
      setTesting(false);
    }
  };

  const set = <K extends keyof AlertSettings>(key: K, value: AlertSettings[K]) =>
    setSettings((prev) => ({ ...prev, [key]: value }));

  return (
    <Modal
      title="通知配置"
      subtitle="当前支持邮件（SMTP）渠道，配置后可发送通知邮件"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <div className="alert-smtp">
          <div className="alert-settings-title">
            <BellIcon size={15} /> 邮件（SMTP）设置
          </div>

          <div className="alert-field">
            <span className="alert-field-label">SMTP 服务器</span>
            <input
              value={settings.smtp_host ?? ''}
              onChange={(e) => set('smtp_host', e.target.value)}
              placeholder="smtp.example.com"
            />
          </div>

          <div className="alert-grid">
            <div className="alert-field field-110">
              <span className="alert-field-label">端口</span>
              <input
                value={settings.smtp_port ?? ''}
                onChange={(e) =>
                  set('smtp_port', Number.parseInt(e.target.value, 10) || 587)
                }
                className="input-port"
              />
            </div>
            <div className="alert-field field-150">
              <span className="alert-field-label">加密</span>
              <Select
                value={settings.smtp_tls ?? 'starttls'}
                options={[
                  { value: 'starttls', label: 'STARTTLS（587）' },
                  { value: 'ssl', label: 'SSL（465）' },
                  { value: 'none', label: '无' },
                ]}
                onChange={(v) => set('smtp_tls', v)}
                ariaLabel="加密方式"
              />
            </div>
          </div>

          <div className="alert-field">
            <span className="alert-field-label">用户名</span>
            <input
              value={settings.smtp_username ?? ''}
              onChange={(e) => set('smtp_username', e.target.value)}
              placeholder="user@example.com"
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">密码 / 授权码</span>
            <input
              type="password"
              value={settings.smtp_password ?? ''}
              onChange={(e) => set('smtp_password', e.target.value)}
              placeholder="SMTP 密码或邮箱授权码"
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">发件人</span>
            <input
              value={settings.smtp_from ?? ''}
              onChange={(e) => set('smtp_from', e.target.value)}
              placeholder="KeyWisp <alert@example.com>"
            />
          </div>

          <div className="alert-field">
            <span className="alert-field-label">收件人（多个用逗号分隔）</span>
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
              {testing ? '测试中…' : '测试发送'}
            </button>
            <button
              type="button"
              className="btn primary"
              onClick={handleSave}
              disabled={saving}
            >
              {saving ? '保存中…' : '保存 SMTP 设置'}
            </button>
          </div>
        </div>
      </div>
    </Modal>
  );
}
