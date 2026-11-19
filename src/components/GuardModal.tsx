import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  getGuardSettings,
  guardStatus,
  installServerGuard,
  listHosts,
  saveGuardSettings,
  serverGuardStatus,
  syncServerGuard,
  uninstallServerGuard,
} from '../api';
import { useI18n } from '../i18n';
import type { GuardHostStatus, GuardSettings, Host } from '../types';
import { fmtError } from '../utils/errors';
import Modal from './Modal';
import Select from './Select';
import { ActivityIcon, CheckIcon, RadarIcon, ServerIcon } from './Icons';

interface Props {
  onClose: () => void;
}

type Mode = 'off' | 'client' | 'server';

const EMPTY_SETTINGS: GuardSettings = {
  enabled: false,
  hosts: [],
  cpu_enabled: true,
  cpu_percent: 90,
  mem_enabled: true,
  mem_percent: 90,
  disk_enabled: true,
  disk_percent: 90,
  ssh_fail_enabled: true,
  ssh_fail_per_min: 20,
  conns_enabled: true,
  conns_threshold: 300,
  load_enabled: true,
  load_per_core: 4,
  inode_enabled: true,
  inode_percent: 90,
  oom_enabled: true,
  units_enabled: true,
  svc_enabled: true,
  svc_procs: [],
  svc_ports: [],
  duration_secs: 60,
  cooldown_minutes: 15,
};

const RULES: {
  key:
    | 'cpu'
    | 'mem'
    | 'disk'
    | 'ssh_fail'
    | 'conns'
    | 'load'
    | 'inode';
  labelKey:
    | 'duty.cpu'
    | 'duty.mem'
    | 'duty.disk'
    | 'duty.sshFail'
    | 'duty.conns'
    | 'duty.load'
    | 'duty.inode';
  enabledKey:
    | 'cpu_enabled'
    | 'mem_enabled'
    | 'disk_enabled'
    | 'ssh_fail_enabled'
    | 'conns_enabled'
    | 'load_enabled'
    | 'inode_enabled';
  valueKey:
    | 'cpu_percent'
    | 'mem_percent'
    | 'disk_percent'
    | 'ssh_fail_per_min'
    | 'conns_threshold'
    | 'load_per_core'
    | 'inode_percent';
  min: number;
  max: number;
  step?: number;
}[] = [
  { key: 'cpu', labelKey: 'duty.cpu', enabledKey: 'cpu_enabled', valueKey: 'cpu_percent', min: 10, max: 99 },
  { key: 'mem', labelKey: 'duty.mem', enabledKey: 'mem_enabled', valueKey: 'mem_percent', min: 10, max: 99 },
  { key: 'disk', labelKey: 'duty.disk', enabledKey: 'disk_enabled', valueKey: 'disk_percent', min: 10, max: 99 },
  { key: 'ssh_fail', labelKey: 'duty.sshFail', enabledKey: 'ssh_fail_enabled', valueKey: 'ssh_fail_per_min', min: 1, max: 1000 },
  { key: 'conns', labelKey: 'duty.conns', enabledKey: 'conns_enabled', valueKey: 'conns_threshold', min: 10, max: 100000 },
  { key: 'load', labelKey: 'duty.load', enabledKey: 'load_enabled', valueKey: 'load_per_core', min: 0.5, max: 64, step: 0.5 },
  { key: 'inode', labelKey: 'duty.inode', enabledKey: 'inode_enabled', valueKey: 'inode_percent', min: 10, max: 99 },
];

/** 无阈值、只有开关的规则 */
const TOGGLES: {
  key: 'oom' | 'units' | 'svc';
  labelKey: 'duty.oom' | 'duty.units' | 'duty.svcTitle';
  enabledKey: 'oom_enabled' | 'units_enabled' | 'svc_enabled';
}[] = [
  { key: 'oom', labelKey: 'duty.oom', enabledKey: 'oom_enabled' },
  { key: 'units', labelKey: 'duty.units', enabledKey: 'units_enabled' },
  { key: 'svc', labelKey: 'duty.svcTitle', enabledKey: 'svc_enabled' },
];

function splitItems(text: string): string[] {
  return text
    .split(/[,，\s]+/)
    .map((v) => v.trim())
    .filter(Boolean);
}

function splitPorts(text: string): number[] {
  return splitItems(text)
    .map((v) => Number.parseInt(v, 10))
    .filter((v) => Number.isFinite(v) && v > 0 && v <= 65535);
}

function modesFromSettings(settings: GuardSettings): Record<string, Mode> {
  const out: Record<string, Mode> = {};
  for (const h of settings.hosts) out[h.host_id] = h.mode;
  return out;
}

export default function GuardModal({ onClose }: Props) {
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
  const [hosts, setHosts] = useState<Host[]>([]);
  const [settings, setSettings] = useState<GuardSettings>(EMPTY_SETTINGS);
  const [saved, setSaved] = useState<GuardSettings>(EMPTY_SETTINGS);
  const [modes, setModes] = useState<Record<string, Mode>>({});
  const [statuses, setStatuses] = useState<Record<string, GuardHostStatus>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [output, setOutput] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [tab, setTab] = useState<'hosts' | 'rules'>('hosts');
  const [procsText, setProcsText] = useState('');
  const [portsText, setPortsText] = useState('');

  const load = useCallback(async () => {
    const [hostList, guard] = await Promise.all([listHosts(), getGuardSettings()]);
    setHosts(hostList);
    setSettings(guard);
    setSaved(guard);
    setModes(modesFromSettings(guard));
    setProcsText(guard.svc_procs.join(', '));
    setPortsText(guard.svc_ports.join(', '));
    try {
      const list = await guardStatus();
      setStatuses(Object.fromEntries(list.map((s) => [s.host_id, s])));
    } catch {
      // 状态失败不影响配置编辑
    }
  }, []);

  useEffect(() => {
    load().catch((e) => setError(fmtError(e)));
  }, [load]);

  const set = <K extends keyof GuardSettings>(key: K, value: GuardSettings[K]) =>
    setSettings((prev) => ({ ...prev, [key]: value }));

  const guardedHosts = useMemo(
    () =>
      Object.entries(modes)
        .filter(([, mode]) => mode !== 'off')
        .map(([host_id, mode]) => ({ host_id, mode: mode as 'client' | 'server' })),
    [modes],
  );

  const procTokens = splitItems(procsText);
  const portTokens = splitItems(portsText);
  const validProcs = [
    ...new Set(procTokens.filter((n) => /^[A-Za-z0-9._@-]{1,64}$/.test(n))),
  ];
  const badProcs = procTokens.filter((n) => !/^[A-Za-z0-9._@-]{1,64}$/.test(n));
  const validPorts = splitPorts(portsText);
  const badPorts = portTokens.filter(
    (v) => !/^\d+$/.test(v) || Number(v) < 1 || Number(v) > 65535,
  );

  const dirty =
    JSON.stringify(guardedHosts) !== JSON.stringify(saved.hosts) ||
    RULES.some(
      (r) =>
        settings[r.enabledKey] !== saved[r.enabledKey] ||
        settings[r.valueKey] !== saved[r.valueKey],
    ) ||
    settings.duration_secs !== saved.duration_secs ||
    settings.cooldown_minutes !== saved.cooldown_minutes ||
    settings.oom_enabled !== saved.oom_enabled ||
    settings.units_enabled !== saved.units_enabled ||
    settings.svc_enabled !== saved.svc_enabled ||
    validProcs.join(',') !== saved.svc_procs.join(',') ||
    validPorts.join(',') !== saved.svc_ports.join(',') ||
    settings.enabled !== saved.enabled;

  const canEnable = guardedHosts.length > 0;
  const enableText = !canEnable
    ? t('duty.enableNoHosts')
    : settings.enabled !== saved.enabled
      ? t(settings.enabled ? 'duty.enablePendingOn' : 'duty.enablePendingOff')
      : t(settings.enabled ? 'duty.enableOn' : 'duty.enableOff');

  const refreshStatuses = useCallback(async () => {
    try {
      const list = await guardStatus();
      setStatuses(Object.fromEntries(list.map((s) => [s.host_id, s])));
    } catch (e) {
      setError(fmtError(e));
    }
  }, []);

  const handleSave = async () => {
    setError(null);
    setNotice(null);
    setSaving(true);
    try {
      const next = await saveGuardSettings({
        ...settings,
        hosts: guardedHosts,
        svc_procs: validProcs,
        svc_ports: validPorts,
      });
      setSettings(next);
      setSaved(next);
      setModes(modesFromSettings(next));
      setProcsText(next.svc_procs.join(', '));
      setPortsText(next.svc_ports.join(', '));
      setNotice(t('duty.saved'));
      await refreshStatuses();
    } catch (e) {
      setError(fmtError(e));
    } finally {
      setSaving(false);
    }
  };

  const runServerAction = async (hostId: string, action: () => Promise<string>) => {
    setError(null);
    setBusy(hostId);
    try {
      const text = await action();
      setOutput((prev) => ({ ...prev, [hostId]: text }));
    } catch (e) {
      setOutput((prev) => ({ ...prev, [hostId]: fmtError(e) }));
    } finally {
      setBusy(null);
    }
  };

  const fmtTime = (secs: number) =>
    secs > 0 ? new Date(secs * 1000).toLocaleString(locale) : null;

  const clientStatusText = (hostId: string) => {
    const st = statuses[hostId];
    if (!st) return null;
    if (st.error) return `${t('duty.statusError')}：${st.error}`;
    if (st.running) {
      const at = fmtTime(st.last_sample_at);
      return at ? t('duty.lastSample', { time: at }) : t('duty.statusRunning');
    }
    return t('duty.statusIdle');
  };

  return (
    <Modal
      title={t('duty.title')}
      subtitle={t('duty.subtitle')}
      className="modal-wide"
      onClose={onClose}
    >
      <div className="ai-modal alert-channels">
        <div className="segmented alert-tabs">
          <button
            type="button"
            className={tab === 'hosts' ? 'seg active' : 'seg'}
            onClick={() => setTab('hosts')}
          >
            <RadarIcon size={14} /> {t('duty.hostsTitle')}
          </button>
          <button
            type="button"
            className={tab === 'rules' ? 'seg active' : 'seg'}
            onClick={() => setTab('rules')}
          >
            <ActivityIcon size={14} /> {t('duty.rulesTitle')}
          </button>
        </div>

        {tab === 'hosts' && (
        <div className="alert-smtp">
          {hosts.length === 0 ? (
            <div className="mcp-empty">
              <ServerIcon size={26} />
              <p>{t('duty.noHosts')}</p>
              <span>{t('duty.noHostsHint')}</span>
            </div>
          ) : (
            <div className="guard-host-list">
              {hosts.map((host) => {
                const mode = modes[host.id] ?? 'off';
                const statusText = mode === 'client' ? clientStatusText(host.id) : null;
                return (
                  <div className="guard-host-row" key={host.id}>
                    <div className="guard-host-main">
                      <span className="mcp-card-avatar">{host.name.slice(0, 1).toUpperCase()}</span>
                      <span className="mcp-card-meta">
                        <span className="mcp-card-name">{host.name}</span>
                        <span className="mcp-card-addr">
                          {host.username}@{host.address}:{host.port}
                        </span>
                        {statusText && <span className="guard-status">{statusText}</span>}
                      </span>
                      <div className="guard-mode">
                        <Select
                          value={mode}
                          options={[
                            { value: 'off', label: t('duty.modeOff') },
                            { value: 'client', label: t('duty.modeClient') },
                            { value: 'server', label: t('duty.modeServer') },
                          ]}
                          onChange={(v) =>
                            setModes((prev) => ({ ...prev, [host.id]: v as Mode }))
                          }
                          ariaLabel={t('duty.mode')}
                        />
                      </div>
                    </div>
                    {mode === 'server' && (
                      <div className="guard-actions">
                        <button
                          type="button"
                          className="btn secondary small"
                          disabled={busy === host.id || dirty}
                          title={dirty ? t('duty.needsSave') : t('duty.installHint')}
                          onClick={() =>
                            void runServerAction(host.id, () => installServerGuard(host.id))
                          }
                        >
                          {busy === host.id ? t('duty.working') : t('duty.install')}
                        </button>
                        <button
                          type="button"
                          className="btn ghost small"
                          disabled={busy === host.id}
                          onClick={() =>
                            void runServerAction(host.id, () => serverGuardStatus(host.id))
                          }
                        >
                          {t('duty.check')}
                        </button>
                        <button
                          type="button"
                          className="btn ghost small"
                          disabled={busy === host.id}
                          title={t('duty.syncHint')}
                          onClick={() =>
                            void runServerAction(host.id, () => syncServerGuard(host.id))
                          }
                        >
                          {t('duty.sync')}
                        </button>
                        <button
                          type="button"
                          className="btn ghost small"
                          disabled={busy === host.id}
                          onClick={() =>
                            void runServerAction(host.id, () => uninstallServerGuard(host.id))
                          }
                        >
                          {t('duty.uninstall')}
                        </button>
                        {dirty && <span className="mcp-hint">{t('duty.needsSave')}</span>}
                        {output[host.id] && (
                          <pre className="guard-output">{output[host.id]}</pre>
                        )}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}
          <p className="mcp-hint">{t('duty.readonlyHint')}</p>
          <p className="mcp-hint">{t('duty.serverHint')}</p>
          <p className="mcp-hint warn">{t('duty.secretWarning')}</p>
        </div>
        )}

        {tab === 'rules' && (
        <div className="alert-smtp">
          <div className="guard-rules">
            {RULES.map((rule) => (
              <div className="guard-rule" key={rule.key}>
                <input
                  type="checkbox"
                  checked={settings[rule.enabledKey]}
                  onChange={(e) => set(rule.enabledKey, e.target.checked)}
                  aria-label={t(rule.labelKey)}
                />
                <span className="guard-rule-label">{t(rule.labelKey)}</span>
                <input
                  type="number"
                  min={rule.min}
                  max={rule.max}
                  step={rule.step ?? 1}
                  value={settings[rule.valueKey]}
                  disabled={!settings[rule.enabledKey]}
                  onChange={(e) => {
                    const raw = rule.step
                      ? Number.parseFloat(e.target.value)
                      : Number.parseInt(e.target.value, 10);
                    set(rule.valueKey, Number.isFinite(raw) ? raw : rule.min);
                  }}
                />
              </div>
            ))}
            {TOGGLES.map((item) => (
              <div className="guard-rule" key={item.key}>
                <input
                  type="checkbox"
                  checked={settings[item.enabledKey]}
                  onChange={(e) => set(item.enabledKey, e.target.checked)}
                  aria-label={t(item.labelKey)}
                />
                <span className="guard-rule-label">{t(item.labelKey)}</span>
                <span />
              </div>
            ))}
          </div>

          {settings.svc_enabled && (
            <>
              <div className="alert-grid guard-svc">
                <div className="alert-field">
                  <span className="alert-field-label">{t('duty.svcProcs')}</span>
                  <input
                    value={procsText}
                    onChange={(e) => setProcsText(e.target.value)}
                    placeholder={t('duty.svcProcsPlaceholder')}
                  />
                </div>
                <div className="alert-field field-150">
                  <span className="alert-field-label">{t('duty.svcPorts')}</span>
                  <input
                    value={portsText}
                    onChange={(e) => setPortsText(e.target.value)}
                    placeholder={t('duty.svcPortsPlaceholder')}
                  />
                </div>
              </div>
              <p className="mcp-hint">{t('duty.svcHint')}</p>
              {validProcs.length === 0 && validPorts.length === 0 && (
                <p className="mcp-hint warn">{t('duty.svcEmpty')}</p>
              )}
              {validProcs.length + validPorts.length > 0 && (
                <p className="mcp-hint">
                  {t('duty.svcSummary', {
                    procs: validProcs.length,
                    ports: validPorts.length,
                  })}
                </p>
              )}
              {badProcs.length > 0 && (
                <p className="mcp-hint warn">
                  {t('duty.svcBadProcs', { list: badProcs.join(', ') })}
                </p>
              )}
              {badPorts.length > 0 && (
                <p className="mcp-hint warn">
                  {t('duty.svcBadPorts', { list: badPorts.join(', ') })}
                </p>
              )}
            </>
          )}
          <div className="alert-grid">
            <div className="alert-field field-150">
              <span className="alert-field-label">{t('duty.duration')}</span>
              <input
                type="number"
                min={10}
                max={600}
                value={settings.duration_secs}
                onChange={(e) =>
                  set('duration_secs', Number.parseInt(e.target.value, 10) || 10)
                }
              />
            </div>
            <div className="alert-field field-150">
              <span className="alert-field-label">{t('duty.cooldown')}</span>
              <input
                type="number"
                min={0}
                max={1440}
                value={settings.cooldown_minutes}
                onChange={(e) =>
                  set('cooldown_minutes', Number.parseInt(e.target.value, 10) || 0)
                }
              />
            </div>
          </div>
          <p className="mcp-hint">{t('duty.durationHint')}</p>
          <p className="mcp-hint">{t('duty.cooldownHint')}</p>
        </div>
        )}

        <div className={`watch-enable${settings.enabled ? ' on' : ''}`}>
          <button
            type="button"
            role="switch"
            aria-checked={settings.enabled}
            aria-label={t('duty.enable')}
            className={`switch${settings.enabled ? ' on' : ''}`}
            disabled={!canEnable || saving}
            onClick={() => set('enabled', !settings.enabled)}
          >
            <span />
          </button>
          <div className="watch-enable-text">
            <strong>{t('duty.enable')}</strong>
            <span>{enableText}</span>
          </div>
        </div>

        {error && <p className="mcp-error">{error}</p>}
        {notice && (
          <p className="test-result ok">
            <CheckIcon size={14} /> {notice}
          </p>
        )}

        <div className="form-actions">
          <button
            type="button"
            className="btn secondary"
            onClick={() => void refreshStatuses()}
            disabled={saving}
          >
            {t('duty.refreshStatus')}
          </button>
          <button
            type="button"
            className="btn primary"
            onClick={() => void handleSave()}
            disabled={saving || !dirty}
          >
            {saving ? t('common.saving') : t('duty.save')}
          </button>
        </div>
      </div>
    </Modal>
  );
}