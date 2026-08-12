import { useCallback, useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import {
  deleteAlert,
  getAlertSettings,
  listAlerts,
  saveAlert,
  saveAlertSettings,
  testAlertChannel,
  testAlertSettings,
} from '../api';
import type { AlertRule, AlertSettings } from '../types';
import Modal from './Modal';
import { BellIcon, PlusIcon, TrashIcon } from './Icons';
import Select from './Select';

interface Props {
  onClose: () => void;
}

const METRIC_LABEL: Record<string, string> = {
  cpu: 'CPU',
  mem: '内存',
  disk: '磁盘（最大使用率）',
  load: '负载（1 分钟）',
};

const CHANNEL_LABEL: Record<string, string> = {
  email: '邮件',
  dingtalk: '钉钉',
  feishu: '飞书',
};

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
  const [rules, setRules] = useState<AlertRule[]>([]);
  const [settings, setSettings] = useState<AlertSettings>(EMPTY_SETTINGS);
  const [metric, setMetric] = useState('cpu');
  const [operator, setOperator] = useState('>');
  const [threshold, setThreshold] = useState('90');
  const [channel, setChannel] = useState('email');
  const [target, setTarget] = useState('');
  const [secret, setSecret] = useState('');
  const [cooldown, setCooldown] = useState('10');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [savingSettings, setSavingSettings] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);

  const load = useCallback(async () => {
    const [rs, st] = await Promise.all([listAlerts(), getAlertSettings()]);
    setRules(rs);
    setSettings({ ...EMPTY_SETTINGS, ...st });
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

  const handleSaveSettings = async () => {
    setError(null);
    setSavingSettings(true);
    try {
      await saveAlertSettings(settings);
      setTestResult('SMTP 设置已保存');
    } catch (e) {
      setError(String(e));
    } finally {
      setSavingSettings(false);
    }
  };

  const handleTestSettings = async () => {
    setError(null);
    setTesting(true);
    setTestResult(null);
    try {
      const res = await testAlertSettings(settings);
      setTestResult(`${res.ok ? '✓' : '!'} ${res.message}`);
    } catch (e) {
      setTestResult(`! ${e}`);
    } finally {
      setTesting(false);
    }
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    const value = Number.parseFloat(threshold);
    if (Number.isNaN(value)) {
      setError('阈值必须是数字');
      return;
    }
    const cooldownNum = Number.parseInt(cooldown, 10);
    if (Number.isNaN(cooldownNum) || cooldownNum < 1) {
      setError('冷却时间必须大于 0 分钟');
      return;
    }
    if ((channel === 'dingtalk' || channel === 'feishu') && !target.trim()) {
      setError('该渠道需要填写 Webhook 地址');
      return;
    }
    setSaving(true);
    try {
      await saveAlert({
        metric,
        operator,
        threshold: value,
        channel,
        cooldown_min: cooldownNum,
        enabled: true,
        ...(target.trim() ? { target: target.trim() } : {}),
        ...(secret.trim() ? { secret: secret.trim() } : {}),
      });
      setThreshold('90');
      setTarget('');
      setSecret('');
      await load();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleTestChannel = async () => {
    setError(null);
    setTesting(true);
    setTestResult(null);
    try {
      const res = await testAlertChannel(
        channel,
        target.trim() || undefined,
        secret.trim() || undefined,
      );
      setTestResult(`${res.ok ? '✓' : '!'} ${res.message}`);
    } catch (e) {
      setTestResult(`! ${e}`);
    } finally {
      setTesting(false);
    }
  };

  const handleDelete = async (rule: AlertRule) => {
    if (!window.confirm('删除这条告警规则？')) return;
    try {
      await deleteAlert(rule.id);
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  const needTarget = channel === 'dingtalk' || channel === 'feishu';
  const needSecret = channel === 'dingtalk' || channel === 'feishu';

  return (
    <Modal
      title="告警通知"
      subtitle="后台每 30 秒检查已连接主机的资源占用，命中规则即通知"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <form className="alert-form" onSubmit={handleSubmit}>
          <div className="alert-grid">
            <div className="alert-field">
              <span className="alert-field-label">指标</span>
              <Select
                value={metric}
                options={[
                  { value: 'cpu', label: 'CPU' },
                  { value: 'mem', label: '内存' },
                  { value: 'disk', label: '磁盘' },
                  { value: 'load', label: '负载' },
                ]}
                onChange={setMetric}
                ariaLabel="指标"
              />
            </div>
            <div className="alert-field field-90">
              <span className="alert-field-label">条件</span>
              <Select
                value={operator}
                options={[
                  { value: '>', label: '>' },
                  { value: '<', label: '<' },
                ]}
                onChange={setOperator}
                ariaLabel="条件"
              />
            </div>
            <div className="alert-field field-90">
              <span className="alert-field-label">阈值</span>
              <input
                value={threshold}
                onChange={(e) => setThreshold(e.target.value)}
                inputMode="decimal"
                className="input-port"
              />
            </div>
            <div className="alert-field field-110">
              <span className="alert-field-label">冷却（分钟）</span>
              <input
                value={cooldown}
                onChange={(e) => setCooldown(e.target.value)}
                inputMode="numeric"
                className="input-port"
              />
            </div>
          </div>

          <div className="alert-grid">
            <div className="alert-field">
              <span className="alert-field-label">通知渠道</span>
              <Select
                value={channel}
                options={[
                  { value: 'email', label: '邮件' },
                  { value: 'dingtalk', label: '钉钉' },
                  { value: 'feishu', label: '飞书' },
                ]}
                onChange={setChannel}
                ariaLabel="通知渠道"
              />
            </div>
            <button type="submit" className="btn primary" disabled={saving}>
              <PlusIcon size={14} /> {saving ? '保存中…' : '添加规则'}
            </button>
          </div>

          {channel === 'email' && (
            <div className="alert-smtp">
              <div className="alert-settings-title">邮件（SMTP）设置</div>
              <div className="alert-field">
                <span className="alert-field-label">SMTP 服务器</span>
                <input
                  value={settings.smtp_host ?? ''}
                  onChange={(e) => setSettings({ ...settings, smtp_host: e.target.value })}
                  placeholder="smtp.example.com"
                />
              </div>
              <div className="alert-grid">
                <div className="alert-field field-110">
                  <span className="alert-field-label">端口</span>
                  <input
                    value={settings.smtp_port ?? ''}
                    onChange={(e) =>
                      setSettings({
                        ...settings,
                        smtp_port: Number.parseInt(e.target.value, 10) || 587,
                      })
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
                    onChange={(v) => setSettings({ ...settings, smtp_tls: v })}
                    ariaLabel="加密方式"
                  />
                </div>
              </div>
              <div className="alert-field">
                <span className="alert-field-label">用户名</span>
                <input
                  value={settings.smtp_username ?? ''}
                  onChange={(e) => setSettings({ ...settings, smtp_username: e.target.value })}
                  placeholder="user@example.com"
                />
              </div>
              <div className="alert-field">
                <span className="alert-field-label">密码 / 授权码</span>
                <input
                  type="password"
                  value={settings.smtp_password ?? ''}
                  onChange={(e) => setSettings({ ...settings, smtp_password: e.target.value })}
                  placeholder="SMTP 密码或邮箱授权码"
                />
              </div>
              <div className="alert-field">
                <span className="alert-field-label">发件人</span>
                <input
                  value={settings.smtp_from ?? ''}
                  onChange={(e) => setSettings({ ...settings, smtp_from: e.target.value })}
                  placeholder="KeyWisp <alert@example.com>"
                />
              </div>
              <div className="alert-field">
                <span className="alert-field-label">收件人（多个用逗号分隔）</span>
                <input
                  value={settings.smtp_to ?? ''}
                  onChange={(e) => setSettings({ ...settings, smtp_to: e.target.value })}
                  placeholder="me@example.com, ops@example.com"
                />
              </div>
              <div className="form-actions">
                <button
                  type="button"
                  className="btn secondary"
                  onClick={handleTestSettings}
                  disabled={testing || savingSettings}
                >
                  {testing ? '测试中…' : '测试邮件'}
                </button>
                <button
                  type="button"
                  className="btn primary"
                  onClick={handleSaveSettings}
                  disabled={savingSettings}
                >
                  {savingSettings ? '保存中…' : '保存 SMTP 设置'}
                </button>
              </div>
            </div>
          )}

          {needTarget && (
            <div className="alert-field">
              <span className="alert-field-label">
                {channel === 'dingtalk' ? '钉钉机器人 Webhook 地址' : '飞书机器人 Webhook 地址'}
              </span>
              <input
                value={target}
                onChange={(e) => setTarget(e.target.value)}
                placeholder={
                  channel === 'dingtalk'
                    ? 'https://oapi.dingtalk.com/robot/send?access_token=…'
                    : 'https://open.feishu.cn/open-apis/bot/v2/hook/…'
                }
              />
            </div>
          )}
          {needSecret && (
            <div className="alert-field">
              <span className="alert-field-label">加签密钥（可选）</span>
              <input
                value={secret}
                onChange={(e) => setSecret(e.target.value)}
                placeholder="机器人安全设置里的签名密钥"
              />
            </div>
          )}
          {needTarget && (
            <button
              type="button"
              className="btn secondary small"
              onClick={handleTestChannel}
              disabled={testing}
            >
              {testing ? '测试中…' : '测试当前渠道'}
            </button>
          )}
          {error && <p className="error">{error}</p>}
          {testResult && (
            <div className={`test-result ${testResult.startsWith('✓') ? 'ok' : 'err'}`}>
              {testResult}
            </div>
          )}
        </form>

        <div className="alert-list">
          {rules.length === 0 && (
            <div className="ai-empty">
              <BellIcon size={26} />
              <p>暂无告警规则</p>
              <span>添加规则后，资源占用超阈值会通过所选渠道通知你</span>
            </div>
          )}
          {rules.map((rule) => (
            <div key={rule.id} className="alert-item">
              <span className="alert-metric">{METRIC_LABEL[rule.metric] ?? rule.metric}</span>
              <span className="alert-op">
                {rule.operator} {rule.threshold}
                {rule.metric === 'cpu' || rule.metric === 'mem' || rule.metric === 'disk'
                  ? '%'
                  : ''}
              </span>
              <span className={`badge alert-channel alert-${rule.channel}`}>
                {CHANNEL_LABEL[rule.channel] ?? rule.channel}
              </span>
              <span className="alert-cooldown">冷却 {rule.cooldown_min} 分钟</span>
              <button
                className="icon-btn danger"
                title="删除规则"
                onClick={() => handleDelete(rule)}
              >
                <TrashIcon size={14} />
              </button>
            </div>
          ))}
        </div>
      </div>
    </Modal>
  );
}
