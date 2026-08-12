import { useCallback, useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { deleteAlert, listAlerts, saveAlert } from '../api';
import type { AlertRule } from '../types';
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

export default function AlertModal({ onClose }: Props) {
  const [rules, setRules] = useState<AlertRule[]>([]);
  const [metric, setMetric] = useState('cpu');
  const [operator, setOperator] = useState('>');
  const [threshold, setThreshold] = useState('90');
  const [channel, setChannel] = useState('notification');
  const [target, setTarget] = useState('');
  const [cooldown, setCooldown] = useState('10');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    setRules(await listAlerts());
  }, []);

  useEffect(() => {
    load().catch((e) => setError(String(e)));
  }, [load]);

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
    if (channel === 'webhook' && !target.trim()) {
      setError('Webhook 渠道需要填写 URL');
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
        ...(channel === 'webhook' ? { target: target.trim() } : {}),
      });
      setThreshold('90');
      setTarget('');
      await load();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (rule: AlertRule) => {
    if (!window.confirm(`删除这条告警规则？`)) return;
    try {
      await deleteAlert(rule.id);
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  return (
    <Modal
      title="告警通知"
      subtitle="后台每 30 秒检查已连接主机的资源占用，命中规则即通知"
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal">
        <form className="alert-form" onSubmit={handleSubmit}>
          <div className="alert-form-row">
            <label>
              指标
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
            </label>
            <label>
              条件
              <Select
                value={operator}
                options={[
                  { value: '>', label: '>' },
                  { value: '<', label: '<' },
                ]}
                onChange={setOperator}
                ariaLabel="条件"
              />
            </label>
            <label>
              阈值
              <input
                value={threshold}
                onChange={(e) => setThreshold(e.target.value)}
                inputMode="decimal"
                className="input-port"
              />
            </label>
            <label>
              冷却（分钟）
              <input
                value={cooldown}
                onChange={(e) => setCooldown(e.target.value)}
                inputMode="numeric"
                className="input-port"
              />
            </label>
          </div>
          <div className="alert-form-row">
            <label>
              通知渠道
              <Select
                value={channel}
                options={[
                  { value: 'notification', label: '桌面通知' },
                  { value: 'webhook', label: 'Webhook' },
                ]}
                onChange={setChannel}
                ariaLabel="通知渠道"
              />
            </label>
            {channel === 'webhook' && (
              <label className="alert-target">
                Webhook URL
                <input
                  value={target}
                  onChange={(e) => setTarget(e.target.value)}
                  placeholder="https://example.com/hook"
                />
              </label>
            )}
            <button type="submit" className="btn primary" disabled={saving}>
              <PlusIcon size={14} /> {saving ? '保存中…' : '添加规则'}
            </button>
          </div>
          {error && <p className="error">{error}</p>}
        </form>

        <div className="alert-list">
          {rules.length === 0 && (
            <div className="ai-empty">
              <BellIcon size={26} />
              <p>暂无告警规则</p>
              <span>添加规则后，资源占用超阈值会通知你</span>
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
                {rule.channel === 'webhook' ? 'Webhook' : '桌面通知'}
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
