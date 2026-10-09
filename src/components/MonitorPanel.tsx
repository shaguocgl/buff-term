import {
  memo,
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent,
} from 'react';
import {
  clipboardWriteText,
  monitorHistory,
  monitorHostInfo,
  monitorSnapshot,
} from '../api';
import type { Host, HostInfo, MonitorSnapshot } from '../types';
import { fmtError } from '../utils/errors';
import { formatBytes } from '../utils/remote';
import {
  ActivityIcon,
  CheckIcon,
  CopyIcon,
  GlobeIcon,
  RefreshIcon,
  XIcon,
} from './Icons';

interface Props {
  host: Host;
  /** 面板隐藏时保持挂载但暂停 5s 轮询（避免后台无效采集） */
  hidden?: boolean;
  onClose: () => void;
}

interface HistoryPoint {
  ts: number;
  cpu: number;
  mem: number;
}

/** 流量趋势点（B/s，由相邻快照计数器差分得到） */
interface NetPoint {
  ts: number;
  rx: number;
  tx: number;
}

/** 静态信息按 host.id 缓存：面板重开 / 来回切主机时秒回显，不重复跑远端脚本 */
const infoCache = new Map<string, HostInfo>();

function Gauge({
  label,
  value,
  display,
}: {
  label: string;
  value: number;
  display: string;
}) {
  const [tip, setTip] = useState(false);
  const color = value >= 85 ? 'red' : value >= 60 ? 'amber' : 'green';
  return (
    <div className="gauge">
      <div className="gauge-head">
        <span
          className="gauge-label"
          onMouseEnter={() => setTip(true)}
          onMouseLeave={() => setTip(false)}
        >
          {label}
        </span>
        <span className="gauge-value">{display}</span>
      </div>
      <div className="gauge-track">
        <div
          className={`gauge-fill ${color}`}
          style={{ width: `${Math.min(100, Math.max(0, value))}%` }}
        />
      </div>
      {tip && <div className="monitor-tooltip gauge-tooltip">{label}</div>}
    </div>
  );
}

function ProcRow({
  rank,
  user,
  cpu,
  mem,
  cmd,
}: {
  rank: number;
  user: string;
  cpu: string;
  mem: string;
  cmd: string;
}) {
  const [tip, setTip] = useState(false);
  return (
    <div className="proc-row">
      <span className="proc-rank">{rank}</span>
      <span className="proc-user">{user}</span>
      <span className="proc-cpu">{cpu}%</span>
      <span className="proc-mem">{mem}%</span>
      <span
        className="proc-cmd"
        onMouseEnter={() => setTip(true)}
        onMouseLeave={() => setTip(false)}
      >
        {cmd}
      </span>
      {tip && <div className="monitor-tooltip proc-tooltip">{cmd}</div>}
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

/** 字节速率自适应单位（B/s → KB/s → MB/s → GB/s） */
function fmtRate(bps: number): string {
  if (bps >= 1024 * 1024 * 1024) return `${(bps / 1073741824).toFixed(1)} GB/s`;
  if (bps >= 1024 * 1024) return `${(bps / 1048576).toFixed(1)} MB/s`;
  if (bps >= 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
  return `${bps.toFixed(0)} B/s`;
}

/** 运行时长：3天 5小时 / 2小时 10分 / 30分 */
function fmtUptime(secs: number): string {
  if (secs <= 0) return '—';
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}天 ${h}小时`;
  if (h > 0) return `${h}小时 ${m}分`;
  return `${m}分`;
}

/** KB → df -h 风格展示串：40G / 6.3M / 1.5T（与后端 fmt_kb 同口径） */
function fmtKb(kb: number): string {
  const K = 1024;
  const trim = (v: number) => v.toFixed(1).replace(/\.0$/, '');
  if (kb >= K * K * K) return `${trim(kb / K / K / K)}T`;
  if (kb >= K * K) return `${trim(kb / K / K)}G`;
  if (kb >= K) return `${trim(kb / K)}M`;
  return `${kb}K`;
}

/** 把流量最大值取整到 1/2/5×10ⁿ 的「好看」刻度，避免 Y 轴出现零碎上限 */
function niceCeil(v: number): number {
  if (v <= 0) return 1;
  const exp = Math.floor(Math.log10(v));
  const base = 10 ** exp;
  for (const f of [1, 2, 5, 10]) {
    if (f * base >= v) return f * base;
  }
  return 10 * base;
}

/** 顶部服务器信息卡：公网 IP（可复制）/ 位置 / 系统 / CPU / 内存 / 运行时长 */
function HostInfoCard({ info }: { info: HostInfo }) {
  const [copied, setCopied] = useState(false);
  const copyIp = () => {
    if (!info.public_ip) return;
    void clipboardWriteText(info.public_ip).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    });
  };
  const sys = [info.os, info.arch].filter(Boolean).join(' ') || '—';
  const cpu =
    info.cores > 0 || info.threads > 0
      ? `${info.cores || info.threads} 核 / ${info.threads} 线程${
          info.cpu_model ? ` · ${info.cpu_model}` : ''
        }`
      : info.cpu_model || '—';
  return (
    <div className="monitor-info">
      <div className="monitor-info-head">
        <span className="monitor-info-host">
          <GlobeIcon size={13} /> {info.hostname || '—'}
        </span>
        <span className="monitor-info-ip">
          {info.public_ip || '—'}
          {info.public_ip && (
            <button
              className="icon-btn monitor-copy"
              title="复制公网 IP"
              onClick={copyIp}
            >
              {copied ? <CheckIcon size={11} /> : <CopyIcon size={11} />}
            </button>
          )}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">位置</span>
        <span className="info-value">{info.location || '—'}</span>
      </div>
      <div className="info-row">
        <span className="info-label">系统</span>
        <span className="info-value" title={info.kernel ? `内核 ${info.kernel}` : undefined}>
          {sys}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">CPU</span>
        <span className="info-value" title={info.cpu_model || undefined}>
          {cpu}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">内存</span>
        <span className="info-value">
          {info.mem_total_mb > 0
            ? `${(info.mem_total_mb / 1024).toFixed(1)} GB`
            : '—'}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">运行</span>
        <span className="info-value">{fmtUptime(info.uptime_secs)}</span>
      </div>
    </div>
  );
}

/** 流量卡：↓rx / ↑tx 双折线，Y 轴自适应峰值；头部实时速率，底部累计流量 */
function NetChart({
  points,
  totalRx,
  totalTx,
}: {
  points: NetPoint[];
  totalRx: number;
  totalTx: number;
}) {
  const [hover, setHover] = useState<{ idx: number; x: number } | null>(null);
  const svgRef = useRef<SVGSVGElement | null>(null);
  const W = 320;
  const H = 96;
  const PAD = 4;
  const MAX_WINDOW = 1800;
  const MIN_WINDOW = 60;
  const now = Date.now() / 1000;
  const visible = points.filter((p) => p.ts >= now - MAX_WINDOW);
  const earliest = visible.length > 0 ? visible[0].ts : now;
  const start = Math.min(now - MIN_WINDOW, Math.max(now - MAX_WINDOW, earliest));
  const window = Math.max(MIN_WINDOW, now - start);
  const last = visible.length > 0 ? visible[visible.length - 1] : null;
  // 至少 1KB/s 量程，避免全 0 时除零 / 空白
  const maxV = niceCeil(
    Math.max(1024, ...visible.flatMap((p) => [p.rx, p.tx])),
  );

  const x = (ts: number) => PAD + ((ts - start) / window) * (W - PAD * 2);
  const y = (v: number) =>
    H - PAD - (Math.min(maxV, Math.max(0, v)) / maxV) * (H - PAD * 2);
  const lineOf = (key: 'rx' | 'tx') =>
    visible.length > 1
      ? visible
          .map((p) => `${x(p.ts).toFixed(1)},${y(p[key]).toFixed(1)}`)
          .join(' ')
      : '';
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
          <i className="dot-rx" /> 流量
        </span>
        <span className="monitor-chart-value net">
          <span className="rx">↓ {last ? fmtRate(last.rx) : '—'}</span>
          <span className="tx">↑ {last ? fmtRate(last.tx) : '—'}</span>
        </span>
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
          {[0, 0.25, 0.5, 0.75, 1].map((g) => (
            <line
              key={g}
              x1={PAD}
              x2={W - PAD}
              y1={y(g * maxV)}
              y2={y(g * maxV)}
              className="monitor-chart-grid"
            />
          ))}
          {visible.length > 1 && (
            <>
              <polyline points={lineOf('rx')} className="chart-line chart-line-rx" />
              <polyline points={lineOf('tx')} className="chart-line chart-line-tx" />
            </>
          )}
          {last && (
            <>
              <circle cx={x(last.ts)} cy={y(last.rx)} r={2.5} className="chart-dot chart-dot-rx" />
              <circle cx={x(last.ts)} cy={y(last.tx)} r={2.5} className="chart-dot chart-dot-tx" />
            </>
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
              <circle cx={x(hovered.ts)} cy={y(hovered.rx)} r={3} className="chart-dot chart-dot-rx" />
              <circle cx={x(hovered.ts)} cy={y(hovered.tx)} r={3} className="chart-dot chart-dot-tx" />
            </>
          )}
        </svg>
        {hover && hovered && (
          <div
            className="monitor-chart-tooltip"
            style={{ left: `${(hover.x / W) * 100}%` }}
          >
            <span className="tooltip-time">
              {new Date(hovered.ts * 1000).toLocaleTimeString('zh-CN', {
                hour12: false,
              })}
            </span>
            <span className="tooltip-value">
              ↓ {fmtRate(hovered.rx)} ↑ {fmtRate(hovered.tx)}
            </span>
          </div>
        )}
      </div>
      <div className="monitor-chart-axis">
        <span>{formatAgo(window)}</span>
        <span className="monitor-net-total">
          累计 ↓{formatBytes(totalRx)} ↑{formatBytes(totalTx)}
        </span>
        <span>现在</span>
      </div>
    </div>
  );
}

function MonitorPanel({ host, hidden = false, onClose }: Props) {
  const [snap, setSnap] = useState<MonitorSnapshot | null>(null);
  const [history, setHistory] = useState<HistoryPoint[]>([]);
  const [netHistory, setNetHistory] = useState<NetPoint[]>([]);
  const [info, setInfo] = useState<HostInfo | null>(
    () => infoCache.get(host.id) ?? null,
  );
  const [error, setError] = useState<string | null>(null);
  // TOP10 进程排序维度：cpu / mem
  const [procTab, setProcTab] = useState<'cpu' | 'mem'>('cpu');
  // 首次加载默认 true，避免面板打开瞬间出现空白
  const [loading, setLoading] = useState(true);
  const inFlightRef = useRef<Promise<void> | null>(null);
  // 上一次快照的网卡计数器：与当前值差分得到实时速率
  const prevNet = useRef<{ ts: number; rx: number; tx: number } | null>(null);
  // 记录当前 host.id：静态信息异步返回时校验，防止切主机后旧结果覆盖新面板
  const hostIdRef = useRef(host.id);
  hostIdRef.current = host.id;

  // 静态信息拉取：cache 命中直接用，未命中才跑远端脚本（含公网查询，最多 ~20s）
  const fetchInfo = useCallback(
    (force = false) => {
      if (!force) {
        const cached = infoCache.get(host.id);
        if (cached) {
          setInfo(cached);
          return;
        }
      }
      monitorHostInfo(host.id)
        .then((i) => {
          infoCache.set(host.id, i);
          if (hostIdRef.current === host.id) setInfo(i);
        })
        .catch(() => {
          // 刷新失败回退到缓存旧值，避免已展示的信息卡突然消失
          if (hostIdRef.current === host.id) {
            setInfo(infoCache.get(host.id) ?? null);
          }
        });
    },
    [host.id],
  );

  // 切换主机时重置全部趋势状态并按新主机回填：
  // - CPU/内存从 host_metrics 回填最近 30 分钟（快照每次采集都会写库）
  // - 网卡计数器与流量历史随快照重新开始累积
  // - 静态信息走模块缓存，未命中才跑远端脚本（含公网查询，最多 ~20s）
  useEffect(() => {
    setInfo(infoCache.get(host.id) ?? null);
    prevNet.current = null;
    setNetHistory([]);
    setHistory([]);
    fetchInfo();
    monitorHistory(host.id, 1800)
      .then((rows) => {
        // 响应可能晚到（用户已切走）：只回写到仍是当前主机时
        if (hostIdRef.current === host.id && rows.length > 0) {
          setHistory(
            rows.map((r) => ({
              ts: r.ts,
              cpu: r.cpu_percent,
              mem: r.mem_percent,
            })),
          );
        }
      })
      .catch(() => {});
  }, [fetchInfo, host.id]);

  const load = useCallback(() => {
    if (inFlightRef.current) return inFlightRef.current;
    setError(null);
    const pending = (async () => {
      try {
        const s = await monitorSnapshot(host.id);
        setSnap(s);
        setHistory((prev) => {
          const next = [...prev, { ts: Date.now() / 1000, cpu: s.cpu_percent, mem: s.mem.percent }];
          // 只保留最近 30 分钟
          const cutoff = Date.now() / 1000 - 1800;
          return next.filter((p) => p.ts >= cutoff);
        });
        // 与上一快照差分网卡计数器 → B/s 速率；计数器回退（网卡重建等）取 0
        const prev = prevNet.current;
        prevNet.current = { ts: s.ts, rx: s.net.rx_bytes, tx: s.net.tx_bytes };
        if (prev && s.ts > prev.ts) {
          const dt = s.ts - prev.ts;
          const rx =
            s.net.rx_bytes >= prev.rx ? (s.net.rx_bytes - prev.rx) / dt : 0;
          const tx =
            s.net.tx_bytes >= prev.tx ? (s.net.tx_bytes - prev.tx) / dt : 0;
          setNetHistory((p) => {
            const next = [...p, { ts: s.ts, rx, tx }];
            const cutoff = s.ts - 1800;
            return next.filter((x) => x.ts >= cutoff);
          });
        }
      } catch (e) {
        setError(fmtError(e));
      } finally {
        setLoading(false);
        inFlightRef.current = null;
      }
    })();
    inFlightRef.current = pending;
    return pending;
  }, [host]);

  useEffect(() => {
    if (hidden) return;
    let stopped = false;
    let timer: number | undefined;
    const tick = async () => {
      await load();
      if (!stopped) {
        timer = window.setTimeout(() => void tick(), 5000);
      }
    };
    void tick();
    return () => {
      stopped = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [load, hidden]);

  return (
    <aside
      className="monitor-panel"
      style={{
        width: 'var(--right-panel-width, 400px)',
        display: hidden ? 'none' : undefined,
      }}
    >
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
              void load();
              fetchInfo(true);
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
        {info && <HostInfoCard info={info} />}
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
              <NetChart
                points={netHistory}
                totalRx={snap.net.rx_bytes}
                totalTx={snap.net.tx_bytes}
              />
            </div>

            {(snap.disks.length > 0 || snap.swap.total_mb > 0) && (
              <div className="monitor-chart monitor-disks">
                <div className="monitor-chart-head">
                  <span className="monitor-chart-label">
                    <i className="dot-disk" /> 磁盘
                  </span>
                  <span className="monitor-chart-value">
                    {fmtKb(snap.disks.reduce((s, d) => s + d.used_kb, 0))}
                    {' / '}
                    {fmtKb(snap.disks.reduce((s, d) => s + d.total_kb, 0))}
                  </span>
                </div>
                <div className="gauges">
                  {snap.swap.total_mb > 0 && (
                    <Gauge
                      label="SWAP"
                      value={snap.swap.percent}
                      display={`${snap.swap.used_mb}M / ${snap.swap.total_mb}M`}
                    />
                  )}
                  {snap.disks.map((d) => (
                    <Gauge
                      key={d.mount}
                      label={`磁盘 ${d.mount}`}
                      value={d.percent}
                      display={`${d.used} / ${d.total}`}
                    />
                  ))}
                </div>
              </div>
            )}

            <div className="monitor-section-title monitor-procs-head">
              TOP10 进程
              <span className="proc-switch">
                <button
                  className={procTab === 'cpu' ? 'active' : ''}
                  onClick={() => setProcTab('cpu')}
                >
                  CPU
                </button>
                <button
                  className={procTab === 'mem' ? 'active' : ''}
                  onClick={() => setProcTab('mem')}
                >
                  MEM
                </button>
              </span>
            </div>
            <div className={`monitor-procs sort-${procTab}`}>
              {(procTab === 'cpu' ? snap.top_cpu : snap.top_mem).map((p, idx) => (
                <ProcRow
                  key={`${p.cmd}-${p.cpu}-${p.mem}-${idx}`}
                  rank={idx + 1}
                  user={p.user}
                  cpu={p.cpu}
                  mem={p.mem}
                  cmd={p.cmd}
                />
              ))}
            </div>
          </>
        )}
      </div>
    </aside>
  );
}

export default memo(MonitorPanel);
