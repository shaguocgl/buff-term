import { useCallback, useEffect, useRef, useState, type MouseEvent } from 'react';
import { monitorSnapshot } from '../api';
import type { Host, MonitorSnapshot } from '../types';
import { ActivityIcon, RefreshIcon, XIcon } from './Icons';

interface Props {
  host: Host;
  onClose: () => void;
}

interface HistoryPoint {
  ts: number;
  cpu: number;
  mem: number;
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

function HistoryChart({
  label,
  points,
  value,
  color,
}: {
  label: string;
  points: { ts: number; value: number }[];
  value: number;
  color: 'cpu' | 'mem';
}) {
  const [hover, setHover] = useState<{ idx: number; x: number } | null>(null);
  const svgRef = useRef<SVGSVGElement | null>(null);
  const W = 320;
  const H = 96;
  const PAD = 4;
  const MAX_WINDOW = 1800; // 最多显示 30 分钟
  const MIN_WINDOW = 60; // 数据不足时至少显示 1 分钟
  const now = Date.now() / 1000;
  const visible = points.filter((p) => p.ts >= now - MAX_WINDOW);
  const earliest = visible.length > 0 ? visible[0].ts : now;
  // 窗口动态收缩：数据少时铺满整个宽度，最多拉到 30 分钟
  const start = Math.min(now - MIN_WINDOW, Math.max(now - MAX_WINDOW, earliest));
  const window = Math.max(MIN_WINDOW, now - start);
  const last = visible.length > 0 ? visible[visible.length - 1] : null;

  const x = (ts: number) => PAD + ((ts - start) / window) * (W - PAD * 2);
  const y = (v: number) => H - PAD - (Math.min(100, Math.max(0, v)) / 100) * (H - PAD * 2);

  const line =
    visible.length > 1
      ? visible.map((p) => `${x(p.ts).toFixed(1)},${y(p.value).toFixed(1)}`).join(' ')
      : '';
  const dot = last ? `${x(last.ts).toFixed(1)},${y(last.value).toFixed(1)}` : '';
  const hovered = hover ? visible[hover.idx] : null;

  const handleMove = (e: MouseEvent<SVGSVGElement>) => {
    const svg = svgRef.current;
    if (!svg || visible.length === 0) return;
    const rect = svg.getBoundingClientRect();
    const scaleX = W / Math.max(1, rect.width);
    const xView = (e.clientX - rect.left) * scaleX;
    const ts = start + ((xView - PAD) / (W - PAD * 2)) * window;
    let bestIdx = 0;
    let bestDist = Infinity;
    visible.forEach((p, idx) => {
      const d = Math.abs(p.ts - ts);
      if (d < bestDist) {
        bestDist = d;
        bestIdx = idx;
      }
    });
    setHover({ idx: bestIdx, x: x(visible[bestIdx].ts) });
  };

  return (
    <div className="monitor-chart">
      <div className="monitor-chart-head">
        <span className="monitor-chart-label">
          <i className={`dot-${color}`} /> {label}
        </span>
        <span className="monitor-chart-value">{value.toFixed(1)}%</span>
      </div>
      <div className="monitor-chart-plot">
        <svg
          ref={svgRef}
          viewBox={`0 0 ${W} ${H}`}
          preserveAspectRatio="none"
          className="monitor-chart-svg"
          onMouseMove={handleMove}
          onMouseLeave={() => setHover(null)}
        >
          {[0, 25, 50, 75, 100].map((g) => (
            <line
              key={g}
              x1={PAD}
              x2={W - PAD}
              y1={y(g)}
              y2={y(g)}
              className="monitor-chart-grid"
            />
          ))}
          {visible.length > 1 && (
            <polyline points={line} className={`chart-line chart-line-${color}`} />
          )}
          {dot && (
            <circle
              cx={x(last!.ts)}
              cy={y(last!.value)}
              r={2.5}
              className={`chart-dot chart-dot-${color}`}
            />
          )}
          {hover && hovered && (
            <>
              <line
                x1={hover.x}
                x2={hover.x}
                y1={PAD}
                y2={H - PAD}
                className="chart-hover-line"
              />
              <circle
                cx={x(hovered.ts)}
                cy={y(hovered.value)}
                r={3}
                className={`chart-dot chart-dot-${color}`}
              />
            </>
          )}
        </svg>
        {hover && hovered && (
          <div
            className="monitor-chart-tooltip"
            style={{ left: `${(hover.x / W) * 100}%` }}
          >
            <span className="tooltip-time">
              {new Date(hovered.ts * 1000).toLocaleTimeString('zh-CN', { hour12: false })}
            </span>
            <span className="tooltip-value">{hovered.value.toFixed(1)}%</span>
          </div>
        )}
      </div>
      <div className="monitor-chart-axis">
        <span>{formatAgo(window)}</span>
        <span>现在</span>
      </div>
    </div>
  );
}

function formatAgo(sec: number): string {
  const minutes = sec / 60;
  if (minutes < 1) return '1分钟内';
  if (minutes < 60) return `${Math.round(minutes)}分钟前`;
  return `${Math.round(minutes / 60)}小时前`;
}

export default function MonitorPanel({ host, onClose }: Props) {
  const [snap, setSnap] = useState<MonitorSnapshot | null>(null);
  const [history, setHistory] = useState<HistoryPoint[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const firstLoad = useRef(true);

  const load = useCallback(async () => {
    setError(null);
    try {
      const s = await monitorSnapshot(host);
      setSnap(s);
      setHistory((prev) => {
        const next = [...prev, { ts: Date.now() / 1000, cpu: s.cpu_percent, mem: s.mem.percent }];
        // 只保留最近 30 分钟
        const cutoff = Date.now() / 1000 - 1800;
        return next.filter((p) => p.ts >= cutoff);
      });
    } catch (e) {
      setError(String(e));
    } finally {
      firstLoad.current = false;
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

            <div className="monitor-charts">
              <HistoryChart
                label="CPU"
                points={history.map((p) => ({ ts: p.ts, value: p.cpu }))}
                value={snap.cpu_percent}
                color="cpu"
              />
              <HistoryChart
                label="内存"
                points={history.map((p) => ({ ts: p.ts, value: p.mem }))}
                value={snap.mem.percent}
                color="mem"
              />
            </div>

            <div className="gauges">
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
