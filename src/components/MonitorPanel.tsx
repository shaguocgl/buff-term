import { useCallback, useEffect, useState } from 'react';
import { monitorSnapshot } from '../api';
import type { Host, MonitorSnapshot } from '../types';
import { ActivityIcon, RefreshIcon, XIcon } from './Icons';

interface Props {
  host: Host;
  onClose: () => void;
}

function Gauge({
  label,
  value,
  display,
}: {
  label: string;
  value: number;
  display: string;
}) {
  const color = value >= 85 ? 'red' : value >= 60 ? 'amber' : 'green';
  return (
    <div className="gauge">
      <div className="gauge-head">
        <span className="gauge-label">{label}</span>
        <span className="gauge-value">{display}</span>
      </div>
      <div className="gauge-track">
        <div
          className={`gauge-fill ${color}`}
          style={{ width: `${Math.min(100, Math.max(0, value))}%` }}
        />
      </div>
    </div>
  );
}

export default function MonitorPanel({ host, onClose }: Props) {
  const [snap, setSnap] = useState<MonitorSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      setSnap(await monitorSnapshot(host));
    } catch (e) {
      setError(String(e));
    }
  }, [host]);

  useEffect(() => {
    load();
    const timer = window.setInterval(load, 5000);
    return () => window.clearInterval(timer);
  }, [load]);

  return (
    <aside className="monitor-panel">
      <div className="sftp-header">
        <div className="sftp-path">
          <ActivityIcon size={14} /> 资源监控（每 5 秒刷新）
        </div>
        <div className="sftp-actions">
          <button
            className="icon-btn"
            title="立即刷新"
            onClick={() => {
              setLoading(true);
              load().finally(() => setLoading(false));
            }}
          >
            <RefreshIcon size={14} />
          </button>
          <button className="icon-btn" title="关闭" onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      <div className="monitor-body">
        {loading && !snap && <div className="sftp-status">加载中…</div>}
        {error && !snap && <div className="sftp-status err">{error}</div>}
        {snap && (
          <>
            <div className="monitor-summary">
              <span>负载 1/5/15：{snap.load || '—'}</span>
              <span>
                {new Date(snap.ts * 1000).toLocaleTimeString('zh-CN', { hour12: false })}
              </span>
            </div>

            <div className="gauges">
              <Gauge
                label="CPU"
                value={snap.cpu_percent}
                display={`${snap.cpu_percent.toFixed(1)}%`}
              />
              <Gauge
                label="内存"
                value={snap.mem.percent}
                display={`${(snap.mem.used_mb / 1024).toFixed(1)}G / ${(
                  snap.mem.total_mb / 1024
                ).toFixed(1)}G`}
              />
              {snap.disks.map((d) => (
                <Gauge
                  key={d.mount}
                  label={`磁盘 ${d.mount}`}
                  value={d.percent}
                  display={`${d.used} / ${d.total}`}
                />
              ))}
            </div>

            <div className="monitor-section-title">TOP 进程（CPU）</div>
            <div className="monitor-procs">
              {snap.top.map((p, idx) => (
                <div className="proc-row" key={idx}>
                  <span className="proc-rank">{idx + 1}</span>
                  <span className="proc-user">{p.user}</span>
                  <span className="proc-cpu">{p.cpu}%</span>
                  <span className="proc-mem">{p.mem}%</span>
                  <span className="proc-cmd">{p.cmd}</span>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </aside>
  );
}
