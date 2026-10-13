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
  monitorState,
} from '../api';
import type { Host, HostInfo, MonitorPoint, MonitorSnapshot } from '../types';
import { useI18n, useT, type TFunc } from '../i18n';
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
  /** 面板隐藏时保持挂载但暂停 5s 轮询（后台采集由会话建立后启动的采集任务持续进行，
   *  重新打开时通过 monitor_state 回填关闭期间的历史） */
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

/** 面板运行时缓存：切走再切回时先回显上次数据，随后由 monitor_state 补齐完整历史 */
interface PanelCache {
  snap: MonitorSnapshot | null;
  history: HistoryPoint[];
  netHistory: NetPoint[];
  prevNet: { ts: number; rx: number; tx: number } | null;
}
const panelCache = new Map<string, PanelCache>();

/** 网卡速率差分允许的最大间隔：超过则视为计数器断档，不产出一个被稀释的假速率 */
const NET_DIFF_MAX_GAP = 60;

/** 由后台采集的累计计数器历史点差分出实时速率；断档（无数据 / 间隔过长）不产出假速率 */
function netFromPoints(points: MonitorPoint[]): {
  history: NetPoint[];
  prev: { ts: number; rx: number; tx: number } | null;
} {
  const history: NetPoint[] = [];
  for (let i = 1; i < points.length; i += 1) {
    const before = points[i - 1];
    const cur = points[i];
    const dt = cur.ts - before.ts;
    if (dt <= 0 || dt > NET_DIFF_MAX_GAP) continue;
    history.push({
      ts: cur.ts,
      rx: cur.rx >= before.rx ? (cur.rx - before.rx) / dt : 0,
      tx: cur.tx >= before.tx ? (cur.tx - before.tx) / dt : 0,
    });
  }
  const last = points[points.length - 1];
  return { history, prev: last ? { ts: last.ts, rx: last.rx, tx: last.tx } : null };
}

/** 合并两段历史点：按 ts 去重升序（同 ts 保留后台回填点），
 *  避免「回填响应晚于轮询响应」时把更新的点冲掉 */
function mergeHistory(base: HistoryPoint[], extra: HistoryPoint[]): HistoryPoint[] {
  const byTs = new Map<number, HistoryPoint>();
  for (const p of extra) byTs.set(p.ts, p);
  for (const p of base) byTs.set(p.ts, p);
  return [...byTs.values()].sort((a, b) => a.ts - b.ts);
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
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
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
              {new Date(hovered.ts * 1000).toLocaleTimeString(locale, { hour12: false })}
            </span>
            <span className="tooltip-value">{hovered.value.toFixed(1)}%</span>
          </div>
        )}
      </div>
      <div className="monitor-chart-axis">
        <span>{formatAgo(window, t)}</span>
        <span>{t('monitor.now')}</span>
      </div>
    </div>
  );
}

function formatAgo(sec: number, t: TFunc): string {
  const minutes = sec / 60;
  if (minutes < 1) return t('monitor.agoWithinMinute');
  if (minutes < 60) return t('monitor.agoMinutes', { n: Math.round(minutes) });
  return t('monitor.agoHours', { n: Math.round(minutes / 60) });
}

/** 字节速率自适应单位（B/s → KB/s → MB/s → GB/s） */
function fmtRate(bps: number): string {
  if (bps >= 1024 * 1024 * 1024) return `${(bps / 1073741824).toFixed(1)} GB/s`;
  if (bps >= 1024 * 1024) return `${(bps / 1048576).toFixed(1)} MB/s`;
  if (bps >= 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
  return `${bps.toFixed(0)} B/s`;
}

/** 运行时长：3天 5小时 / 2小时 10分 / 30分 */
function fmtUptime(secs: number, t: TFunc): string {
  if (secs <= 0) return '—';
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return t('monitor.uptimeDays', { d, h });
  if (h > 0) return t('monitor.uptimeHours', { h, m });
  return t('monitor.uptimeMinutes', { m });
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
  const t = useT();
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
      ? `${t('monitor.coresThreads', {
          cores: info.cores || info.threads,
          threads: info.threads,
        })}${info.cpu_model ? ` · ${info.cpu_model}` : ''}`
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
              title={t('monitor.copyIp')}
              onClick={copyIp}
            >
              {copied ? <CheckIcon size={11} /> : <CopyIcon size={11} />}
            </button>
          )}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">{t('monitor.location')}</span>
        <span className="info-value">{info.location || '—'}</span>
      </div>
      <div className="info-row">
        <span className="info-label">{t('monitor.system')}</span>
        <span
          className="info-value"
          title={info.kernel ? t('monitor.kernel', { v: info.kernel }) : undefined}
        >
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
        <span className="info-label">{t('monitor.memory')}</span>
        <span className="info-value">
          {info.mem_total_mb > 0
            ? `${(info.mem_total_mb / 1024).toFixed(1)} GB`
            : '—'}
        </span>
      </div>
      <div className="info-row">
        <span className="info-label">{t('monitor.uptime')}</span>
        <span className="info-value">{fmtUptime(info.uptime_secs, t)}</span>
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
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
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
          <i className="dot-rx" /> {t('monitor.net')}
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
              {new Date(hovered.ts * 1000).toLocaleTimeString(locale, {
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
        <span>{formatAgo(window, t)}</span>
        <span className="monitor-net-total">
          {t('monitor.netTotal', {
            rx: formatBytes(totalRx),
            tx: formatBytes(totalTx),
          })}
        </span>
        <span>{t('monitor.now')}</span>
      </div>
    </div>
  );
}

function MonitorPanel({ host, hidden = false, onClose }: Props) {
  const { t, lang } = useI18n();
  const locale = lang === 'en' ? 'en-US' : 'zh-CN';
  const [snap, setSnap] = useState<MonitorSnapshot | null>(
    () => panelCache.get(host.id)?.snap ?? null,
  );
  const [history, setHistory] = useState<HistoryPoint[]>(
    () => panelCache.get(host.id)?.history ?? [],
  );
  const [netHistory, setNetHistory] = useState<NetPoint[]>(
    () => panelCache.get(host.id)?.netHistory ?? [],
  );
  const [info, setInfo] = useState<HostInfo | null>(
    () => infoCache.get(host.id) ?? null,
  );
  const [error, setError] = useState<string | null>(null);
  // TOP10 进程排序维度：cpu / mem
  const [procTab, setProcTab] = useState<'cpu' | 'mem'>('cpu');
  // 有缓存快照则不算「加载中」，直接回显旧数据等待下一次轮询刷新
  const [loading, setLoading] = useState(() => !panelCache.has(host.id));
  const inFlightRef = useRef<Promise<void> | null>(null);
  // 上一次快照的网卡计数器：与当前值差分得到实时速率
  const prevNet = useRef<{ ts: number; rx: number; tx: number } | null>(
    panelCache.get(host.id)?.prevNet ?? null,
  );
  // 趋势数据的 ref 镜像：load 里需要基于上一次的值计算新数组并回写缓存
  const historyRef = useRef(history);
  const netRef = useRef(netHistory);
  // 最近快照的 ref 镜像：回填与轮询响应乱序返回时，避免用较旧的一份覆盖新快照
  const snapRef = useRef<MonitorSnapshot | null>(snap);
  snapRef.current = snap;
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

  // 切换主机时恢复该主机的面板缓存（上次离开时的快照与趋势），
  // 连接以来的完整历史由下面的 monitor_state 回填。
  // 静态信息走模块缓存，未命中才跑远端脚本（含公网查询，最多 ~20s）
  useEffect(() => {
    setInfo(infoCache.get(host.id) ?? null);
    const cached = panelCache.get(host.id);
    setSnap(cached?.snap ?? null);
    snapRef.current = cached?.snap ?? null;
    historyRef.current = cached?.history ?? [];
    netRef.current = cached?.netHistory ?? [];
    prevNet.current = cached?.prevNet ?? null;
    setHistory(historyRef.current);
    setNetHistory(netRef.current);
    setLoading(!cached);
    fetchInfo();
  }, [fetchInfo, host.id]);

  // 切换界面语言后归属地（国家 / 城市）需按新语言重新采集：
  // 清掉静态信息模块缓存并强制刷新一次，避免英文界面下仍显示中文地名
  const infoLangRef = useRef(lang);
  useEffect(() => {
    if (infoLangRef.current === lang) return;
    infoLangRef.current = lang;
    infoCache.clear();
    fetchInfo(true);
  }, [lang, fetchInfo]);

  // 面板打开 / 切换主机时读取后台采集状态：后台采集在会话建立后就已启动，
  // 这里一次性拿到连接以来的完整历史（面板打开 5s / 后台 30s 粒度，含面板关闭期间
  // 的数据）并与面板已积累的点合并，避免趋势图断档；后端无内存数据（如应用刚重启）
  // 时回退到数据库历史（1 分钟粒度）
  useEffect(() => {
    if (hidden) return;
    const hostId = host.id;
    monitorState(hostId)
      .then((st) => {
        // 响应可能晚到（用户已切走）：只回写到仍是当前主机时
        if (hostIdRef.current !== hostId) return;
        if (st.latest) {
          // 轮询响应可能先于本回填返回且带回更新的快照，避免旧数据覆盖新数据
          if (!snapRef.current || st.latest.ts > snapRef.current.ts) {
            setSnap(st.latest);
            snapRef.current = st.latest;
          }
          setLoading(false);
        } else if (st.error) {
          setError(st.error);
          setLoading(false);
        }
        if (st.points.length > 0) {
          // 与面板已积累的点合并（按时间戳去重），回填只补历史、不丢更新
          const points = mergeHistory(
            st.points.map((p) => ({ ts: p.ts, cpu: p.cpu, mem: p.mem })),
            historyRef.current,
          );
          const net = netFromPoints(st.points);
          historyRef.current = points;
          netRef.current = net.history;
          prevNet.current = net.prev;
          setHistory(points);
          setNetHistory(net.history);
          panelCache.set(hostId, {
            snap: snapRef.current ?? st.latest,
            history: points,
            netHistory: net.history,
            prevNet: net.prev,
          });
          return;
        }
        monitorHistory(hostId, 1800)
          .then((rows) => {
            if (hostIdRef.current !== hostId || rows.length === 0) return;
            const points = rows.map((r) => ({
              ts: r.ts,
              cpu: r.cpu_percent,
              mem: r.mem_percent,
            }));
            historyRef.current = points;
            setHistory(points);
          })
          .catch(() => {});
      })
      .catch(() => {});
  }, [hidden, host.id]);

  const load = useCallback(() => {
    if (inFlightRef.current) return inFlightRef.current;
    setError(null);
    const pending = (async () => {
      try {
        const s = await monitorSnapshot(host.id);
        setSnap(s);
        snapRef.current = s;
        // 后台采集与面板轮询可能返回同一快照（缓存复用），按时间戳去重，
        // 避免趋势图出现重复点
        const last = historyRef.current[historyRef.current.length - 1];
        if (!last || last.ts < s.ts) {
          const nextHist = [
            ...historyRef.current,
            { ts: s.ts, cpu: s.cpu_percent, mem: s.mem.percent },
          ].filter((p) => p.ts >= s.ts - 1800); // 只保留最近 30 分钟
          historyRef.current = nextHist;
          setHistory(nextHist);
        }
        // 与上一快照差分网卡计数器 → B/s 速率；计数器回退（网卡重建等）取 0；
        // 间隔过长（面板切走暂停轮询后回来）说明计数器断档，不产出被稀释的假速率
        const prev = prevNet.current;
        prevNet.current = { ts: s.ts, rx: s.net.rx_bytes, tx: s.net.tx_bytes };
        if (prev && s.ts > prev.ts && s.ts - prev.ts <= NET_DIFF_MAX_GAP) {
          const dt = s.ts - prev.ts;
          const rx =
            s.net.rx_bytes >= prev.rx ? (s.net.rx_bytes - prev.rx) / dt : 0;
          const tx =
            s.net.tx_bytes >= prev.tx ? (s.net.tx_bytes - prev.tx) / dt : 0;
          const nextNet = [...netRef.current, { ts: s.ts, rx, tx }].filter(
            (x) => x.ts >= s.ts - 1800,
          );
          netRef.current = nextNet;
          setNetHistory(nextNet);
        }
        // 回写面板缓存：切走再切回时直接回显本次数据
        panelCache.set(host.id, {
          snap: s,
          history: historyRef.current,
          netHistory: netRef.current,
          prevNet: prevNet.current,
        });
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
          <ActivityIcon size={14} /> {t('monitor.title')}
        </div>
        <div className="sftp-actions">
          <button
            className="icon-btn"
            title={t('monitor.refreshNow')}
            onClick={() => {
              setLoading(true);
              void load();
              fetchInfo(true);
            }}
          >
            <RefreshIcon size={14} />
          </button>
          <button className="icon-btn" title={t('common.close')} onClick={onClose}>
            <XIcon size={14} />
          </button>
        </div>
      </div>

      <div className="monitor-body">
        {loading && !snap && <div className="sftp-status">{t('common.loading')}</div>}
        {error && !snap && <div className="sftp-status err">{error}</div>}
        {info && <HostInfoCard info={info} />}
        {snap && (
          <>
            <div className="monitor-summary">
              <span>{t('monitor.loadAvg', { load: snap.load || '—' })}</span>
              <span>
                {new Date(snap.ts * 1000).toLocaleTimeString(locale, { hour12: false })}
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
                label={t('monitor.memory')}
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
                    <i className="dot-disk" /> {t('monitor.disk')}
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
                      label={t('monitor.diskMount', { mount: d.mount })}
                      value={d.percent}
                      display={`${d.used} / ${d.total}`}
                    />
                  ))}
                </div>
              </div>
            )}

            <div className="monitor-section-title monitor-procs-head">
              {t('monitor.topProcs')}
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
