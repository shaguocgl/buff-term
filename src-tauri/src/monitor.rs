use crate::db::Db;
use crate::models::{Host, HostMetric, MetricDisk, MetricTop};
use crate::russh::{ClientHandler, RusshManager};
use crate::util::now;
use russh::client::Handle;
use serde::Serialize;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Default, Clone)]
pub struct DiskInfo {
    pub mount: String,
    pub fs: String,
    /// 人类可读总容量，如 "40G"（由 total_kb 格式化而来）
    pub total: String,
    /// 人类可读已用量
    pub used: String,
    /// 总容量（1K 块数），供前端汇总磁盘总量
    pub total_kb: u64,
    /// 已用容量（1K 块数）
    pub used_kb: u64,
    pub percent: f64,
}

#[derive(Serialize, Default, Clone)]
pub struct MemInfo {
    pub total_mb: u64,
    pub used_mb: u64,
    pub percent: f64,
}

#[derive(Serialize, Default, Clone)]
pub struct TopProc {
    pub user: String,
    pub cpu: String,
    pub mem: String,
    pub cmd: String,
}

/// 单块网卡的累计收发字节数（自系统启动起，单位 Byte）。
#[derive(Serialize, Default, Clone)]
pub struct NetIface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// 网络流量：rx_bytes/tx_bytes 为所有「物理」网卡累计值之和；
/// ifaces 保留过滤后的逐网卡明细，供后续按网卡展示使用。
#[derive(Serialize, Default, Clone)]
pub struct NetInfo {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub ifaces: Vec<NetIface>,
}

#[derive(Serialize, Default, Clone)]
pub struct MonitorSnapshot {
    pub ts: u64,
    pub host_label: String,
    pub load: String,
    pub cpu_percent: f64,
    pub mem: MemInfo,
    pub swap: MemInfo,
    pub disks: Vec<DiskInfo>,
    pub net: NetInfo,
    /// 按 CPU 排序的 TOP10 进程
    pub top_cpu: Vec<TopProc>,
    /// 按内存排序的 TOP10 进程
    pub top_mem: Vec<TopProc>,
}

/// 服务器静态信息（公网 IP / 国家 / 系统 / CPU / 内存），一次性采集，
/// 不随监控 5s 轮询执行（公网查询走 curl，耗时不稳定）。
#[derive(Serialize, Default)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_model: String,
    pub cores: u32,
    pub threads: u32,
    pub mem_total_mb: u64,
    pub uptime_secs: u64,
    /// 服务器出口公网 IP，获取失败为空串
    pub public_ip: String,
    /// 国家 / 城市，如「美国 · 洛杉矶」，获取失败为空串
    pub location: String,
}

/// 采集 Linux 服务器的资源快照（CPU / 内存 / 磁盘 / 负载 / TOP 进程）。
/// 复用 russh 连接池，避免每次采集都新建系统 ssh 进程。
/// 后台采集任务（终端会话建立后启动）已有新鲜结果时直接复用；
/// 采集成功后自动写入 host_metrics 表，作为历史趋势数据。
#[tauri::command]
pub async fn monitor_snapshot(
    app: AppHandle,
    russh: State<'_, RusshManager>,
    db: State<'_, std::sync::Arc<Db>>,
    monitor: State<'_, MonitorManager>,
    host_id: String,
) -> Result<MonitorSnapshot, String> {
    // 面板正在轮询：标记活跃，采集任务据此切到 5 秒节奏
    monitor.touch_poll(&host_id);
    // 会话连接中后台采集在跑：面板轮询直接命中缓存，不再重复跑远端脚本
    if let Some(snap) = monitor.snapshot_if_fresh(&host_id, SNAPSHOT_FRESH_SECS) {
        return Ok(snap);
    }
    let host = crate::hosts::load_host(&db, &host_id)?;
    let snap = collect_russh(&host, &russh).await?;
    monitor.push(&host.id, &snap);
    if let Some(db) = app.try_state::<std::sync::Arc<Db>>() {
        let _ = save_metric(&db, &host.id, &snap, "manual");
    }
    Ok(snap)
}

/// 通过 russh 连接池采集（复用 AI / MCP 同一 SSH 通道）。
/// 用于面板实时采集兜底（无后台采集任务或结果过期时）与巡检 / AI 的一次性采集。
pub async fn collect_russh(
    host: &Host,
    russh: &RusshManager,
) -> Result<MonitorSnapshot, String> {
    let out = russh
        .exec(host, MONITOR_SCRIPT, Duration::from_secs(25))
        .await?;
    parse(&out.text, host)
}

/// 在终端会话自身的 SSH 连接上采集（后台采集专用）。
/// channel 级执行：失败只关闭本次通道，不影响交互会话；
/// 取消（会话关闭）时向远端脚本发 SIGTERM 并立即返回。
pub async fn collect_on_handle(
    handle: &Handle<ClientHandler>,
    host: &Host,
    cancel: &CancellationToken,
) -> Result<MonitorSnapshot, String> {
    let out = crate::russh::exec_on(handle, MONITOR_SCRIPT, Duration::from_secs(25), cancel).await?;
    parse(&out.text, host)
}

/// 采集服务器静态信息（公网 IP / 国家 / 系统 / CPU 核数线程 / 内存 / 运行时长）。
/// 面板打开时调用一次即可；公网查询在远端 curl，无外网时由本地补查兜底。
#[tauri::command]
pub async fn monitor_host_info(
    russh: State<'_, RusshManager>,
    db: State<'_, std::sync::Arc<Db>>,
    host_id: String,
) -> Result<HostInfo, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    // 远端脚本通过 GEO_LANG 决定 ip-api 返回的国家/城市语言（与界面语言一致）
    let script = format!(
        "GEO_LANG={}\n{}",
        crate::i18n::t("zh-CN", "en"),
        HOST_INFO_SCRIPT
    );
    let out = russh
        .exec(&host, &script, Duration::from_secs(30))
        .await?;
    let mut info = parse_host_info(&out.text)?;
    // 本地兜底：IP 缺失时查连接地址；只有 IP 无归属地时用该 IP 再补一次位置
    if info.public_ip.is_empty() || info.location.is_empty() {
        let target = if info.public_ip.is_empty() {
            &host.address
        } else {
            &info.public_ip
        };
        if let Some((ip, location)) = geo_lookup(target).await {
            if info.public_ip.is_empty() {
                info.public_ip = ip;
            }
            if info.location.is_empty() {
                info.location = location;
            }
        }
    }
    Ok(info)
}

/// 本地兜底：当远端无外网 / 无 curl 时，对 host.address 直接查询 ip-api。
/// ip-api 会自行解析域名并拒绝私网地址（返回 status=fail），因此连接地址是
/// 域名或公网 IP 时能补出归属地；私网地址则自然落空返回 None。
async fn geo_lookup(address: &str) -> Option<(String, String)> {
    // 国家/城市名跟随界面语言（ip-api 的 lang 参数），避免英文界面里出现中文地名
    let url = format!(
        "http://ip-api.com/json/{}?fields=status,country,city,query&lang={}",
        address,
        crate::i18n::t("zh-CN", "en")
    );
    let client = reqwest::Client::builder()
        // 归属地服务不可达时快速失败，不让 command 挂起
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let body = client.get(url).send().await.ok()?.text().await.ok()?;
    let (ip, country, city) = parse_geo(&body);
    if ip.is_empty() {
        return None;
    }
    Some((ip, format_location(&country, &city)))
}

/// 国家 + 城市拼成展示串，如「美国 · 洛杉矶」；只有国家时返回国家本身。
fn format_location(country: &str, city: &str) -> String {
    match (country.is_empty(), city.is_empty()) {
        (false, false) => format!("{country} · {city}"),
        (false, true) => country.to_string(),
        _ => String::new(),
    }
}

/// 解析 GEO 段 JSON，兼容三种 schema：
/// ip-api {status,country,city,query} / ipinfo {ip,country} / ipify {ip}。
/// 返回 (ip, country, city)。
fn parse_geo(json: &str) -> (String, String, String) {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new(), String::new()),
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    // ip-api 失败时 status="fail"（私网/保留地址），此时 query 也未必可用
    if get("status") == "fail" {
        return (String::new(), String::new(), String::new());
    }
    let ip = if get("query").is_empty() {
        get("ip")
    } else {
        get("query")
    };
    (
        ip.to_string(),
        get("country").to_string(),
        get("city").to_string(),
    )
}

/// 查询主机历史指标（供监控面板打开时回填趋势图）。
/// 复用 Db::list_metrics，窗口按秒级时间戳取 since，窗口 1 分钟 ~ 24 小时。
#[tauri::command]
pub fn monitor_history(
    db: State<'_, std::sync::Arc<Db>>,
    host_id: String,
    window_secs: Option<u64>,
) -> Result<Vec<HostMetric>, String> {
    let window = window_secs.unwrap_or(1800).clamp(60, 86_400);
    let since = now().saturating_sub(window);
    db.list_metrics(&host_id, since, 5000)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取历史指标失败", "Failed to read historical metrics")))
}

/// 采集节拍：采集循环每 5 秒醒来一次判断是否到采集时机（醒来本身无远端开销）。
const MONITOR_TICK: Duration = Duration::from_secs(5);
/// 面板打开时的采集间隔：与面板 5 秒轮询同频。
const MONITOR_INTERVAL: Duration = Duration::from_secs(5);
/// 面板未打开（后台静默）时的采集间隔：只保留趋势所需粒度，
/// 远端 CPU / 流量开销降约 6 倍；面板一打开，下一个节拍即恢复 5 秒。
const MONITOR_IDLE_INTERVAL: Duration = Duration::from_secs(30);
/// 面板活跃判定窗口（秒）：该窗口内面板调用过监控接口即视为「面板打开中」。
const ACTIVE_WINDOW_SECS: u64 = 15;
/// 首次采集延迟：等终端会话建立稳定后再采集，避免与交互初始化争抢连接。
const MONITOR_FIRST_DELAY: Duration = Duration::from_secs(1);
/// 采集连续失败时的退避起始值（此后翻倍）。
const MONITOR_FAIL_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// 采集连续失败时的退避上限：2s → 4s → 8s → 16s → 30s 封顶，
/// 避免主机不可达时频繁重试。
const MONITOR_FAIL_MAX_INTERVAL: Duration = Duration::from_secs(30);
/// 内存历史点保留窗口（秒）：与面板趋势图窗口一致（最近 30 分钟）。
const POINT_WINDOW_SECS: u64 = 1800;
/// 快照新鲜阈值（秒）：monitor_snapshot 命中该阈值内的后台采集结果时直接复用，
/// 避免面板轮询与后台采集任务对同一主机重复跑远端脚本。
const SNAPSHOT_FRESH_SECS: u64 = 30;

/// 内存历史点：面板打开时 5 秒 / 后台静默时 30 秒粒度，
/// 供面板打开时回填「连接以来」的趋势。
#[derive(Serialize, Default, Clone)]
pub struct MonitorPoint {
    /// Unix 秒级时间戳
    pub ts: u64,
    /// CPU 使用率（%）
    pub cpu: f64,
    /// 内存使用率（%）
    pub mem: f64,
    /// 物理网卡累计接收字节数（与相邻点差分得到实时速率）
    pub rx: u64,
    /// 物理网卡累计发送字节数（与相邻点差分得到实时速率）
    pub tx: u64,
}

/// 主机监控实时状态：面板打开时一次性读取，立即拿到最近快照与连接以来的历史点。
#[derive(Serialize, Default)]
pub struct MonitorState {
    /// 最近一次成功采集的快照（后台采集尚未完成或全部失败时为空）
    pub latest: Option<MonitorSnapshot>,
    /// 连接以来的历史点（最近 30 分钟）
    pub points: Vec<MonitorPoint>,
    /// 最近一次采集错误（成功后自动清除），供面板在无快照时提示原因
    pub error: Option<String>,
}

/// 一台主机的后台采集任务句柄。
struct CollectorHandle {
    /// 取消令牌：该主机最后一个会话关闭时触发，采集循环退出
    cancel: CancellationToken,
    /// 正在使用该主机的会话 id 集合（多标签打开同一主机时共用同一采集任务）
    sessions: HashSet<u32>,
}

#[derive(Default)]
struct MonitorInner {
    /// host_id → 后台采集任务
    collectors: HashMap<String, CollectorHandle>,
    /// host_id → 最近一次成功快照
    latest: HashMap<String, MonitorSnapshot>,
    /// host_id → 最近一次采集错误
    errors: HashMap<String, String>,
    /// host_id → 历史点（面板打开 5 秒 / 后台静默 30 秒粒度，最多保留最近 30 分钟）
    points: HashMap<String, VecDeque<MonitorPoint>>,
    /// host_id → 面板最近一次调用监控接口的时间（采集频率自适应的依据）
    last_poll: HashMap<String, u64>,
}

/// 后台监控采集管理器：终端会话建立后即开始采集（而非等面板打开），
/// 采集频率自适应（面板打开 5 秒 / 后台静默 30 秒），且复用会话自身的 SSH 连接；
/// 该主机所有会话关闭时停止；采集结果常驻内存，面板打开秒回显、
/// 多主机切换不丢数据（切走再切回直接读该主机的内存历史）。
#[derive(Default)]
pub struct MonitorManager {
    inner: Mutex<MonitorInner>,
}

impl MonitorManager {
    /// 会话建立后注册后台采集：该主机已有采集任务时仅登记会话（多标签共用），
    /// 没有任务时新建任务并立即开始采集。
    pub fn attach(&self, app: &AppHandle, host: &Host, session_id: u32) {
        if let Some(cancel) = self.register_session(&host.id, session_id) {
            spawn_collector(app.clone(), host.clone(), cancel);
        }
    }

    /// 会话关闭后注销：该主机最后一个会话关闭时停止采集任务。
    /// 已采集的数据保留在内存中，重连 / 切回时仍可直接回显。
    pub fn detach(&self, host_id: &str, session_id: u32) {
        if let Some(cancel) = self.unregister_session(host_id, session_id) {
            cancel.cancel();
        }
    }

    /// 采集成功后更新内存状态：刷新最近快照、追加历史点（按时间戳去重、
    /// 只保留最近 30 分钟），并清除上一次的采集错误。
    pub fn push(&self, host_id: &str, snap: &MonitorSnapshot) {
        let mut inner = self.inner.lock().unwrap();
        inner.errors.remove(host_id);
        // 快照回退（采集任务与面板兜底采集交错返回）时保留更新的那份
        let newer = inner
            .latest
            .get(host_id)
            .map_or(true, |prev| snap.ts >= prev.ts);
        if newer {
            inner.latest.insert(host_id.to_string(), snap.clone());
        }
        let point = MonitorPoint {
            ts: snap.ts,
            cpu: snap.cpu_percent,
            mem: snap.mem.percent,
            rx: snap.net.rx_bytes,
            tx: snap.net.tx_bytes,
        };
        let cutoff = snap.ts.saturating_sub(POINT_WINDOW_SECS);
        let deque = inner.points.entry(host_id.to_string()).or_default();
        if deque.back().map_or(true, |last| point.ts > last.ts) {
            deque.push_back(point);
        }
        while deque.front().is_some_and(|p| p.ts < cutoff) {
            deque.pop_front();
        }
    }

    /// 记录最近一次采集错误（下一次成功后由 push 自动清除）。
    pub fn set_error(&self, host_id: &str, error: String) {
        self.inner
            .lock()
            .unwrap()
            .errors
            .insert(host_id.to_string(), error);
    }

    /// 记录面板最近一次调用监控接口的时间（monitor_state / monitor_snapshot）。
    /// 采集任务据此在「面板打开 5 秒 / 后台静默 30 秒」之间自适应。
    pub fn touch_poll(&self, host_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .last_poll
            .insert(host_id.to_string(), now());
    }

    /// 面板是否处于活跃窗口内（监控面板正打开着）。
    fn panel_active(&self, host_id: &str, window_secs: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .last_poll
            .get(host_id)
            .is_some_and(|ts| now().saturating_sub(*ts) <= window_secs)
    }

    /// 命中新鲜阈值的最近快照：供 monitor_snapshot 复用后台采集结果，
    /// 面板轮询不再重复跑远端脚本。仅在「该主机有会话连接（采集任务在跑）
    /// 且结果新鲜」时复用；无任务 / 结果过期时返回 None，由调用方实时采集兜底。
    pub fn snapshot_if_fresh(&self, host_id: &str, max_age_secs: u64) -> Option<MonitorSnapshot> {
        let inner = self.inner.lock().unwrap();
        if !inner.collectors.contains_key(host_id) {
            return None;
        }
        inner
            .latest
            .get(host_id)
            .filter(|snap| now().saturating_sub(snap.ts) <= max_age_secs)
            .cloned()
    }

    /// 读取主机监控状态：最近快照 + 最近 30 分钟的历史点 + 最近错误。
    /// 读取时顺带清理过期点，避免长时间运行后内存缓慢增长。
    pub fn state(&self, host_id: &str) -> MonitorState {
        let mut inner = self.inner.lock().unwrap();
        let cutoff = now().saturating_sub(POINT_WINDOW_SECS);
        let points = inner
            .points
            .get_mut(host_id)
            .map(|deque| {
                while deque.front().is_some_and(|p| p.ts < cutoff) {
                    deque.pop_front();
                }
                deque.iter().cloned().collect()
            })
            .unwrap_or_default();
        MonitorState {
            latest: inner.latest.get(host_id).cloned(),
            points,
            error: inner.errors.get(host_id).cloned(),
        }
    }

    /// 登记会话：返回新建采集任务的取消令牌（该主机此前无任务时）。
    fn register_session(&self, host_id: &str, session_id: u32) -> Option<CancellationToken> {
        let mut inner = self.inner.lock().unwrap();
        match inner.collectors.entry(host_id.to_string()) {
            Entry::Occupied(mut e) => {
                e.get_mut().sessions.insert(session_id);
                None
            }
            Entry::Vacant(e) => {
                let cancel = CancellationToken::new();
                e.insert(CollectorHandle {
                    cancel: cancel.clone(),
                    sessions: HashSet::from([session_id]),
                });
                Some(cancel)
            }
        }
    }

    /// 注销会话：返回需要停止的采集任务取消令牌（该主机已无会话时）。
    fn unregister_session(&self, host_id: &str, session_id: u32) -> Option<CancellationToken> {
        let mut inner = self.inner.lock().unwrap();
        let Entry::Occupied(mut e) = inner.collectors.entry(host_id.to_string()) else {
            return None;
        };
        let handle = e.get_mut();
        handle.sessions.remove(&session_id);
        if !handle.sessions.is_empty() {
            return None;
        }
        let cancel = handle.cancel.clone();
        e.remove();
        Some(cancel)
    }
}

/// 后台采集循环：会话建立后先延迟 1 秒采集一次，此后按节拍醒来判断是否到采集时机。
/// 采集频率自适应：面板打开中每 5 秒一次，面板关闭（后台静默）降为每 30 秒一次，
/// 面板一打开下一个节拍即恢复 5 秒；采集在终端会话自身的 SSH 连接上执行
/// （不占用连接池，也不受池容量上限影响）；会话关闭（取消令牌触发）时中断远端脚本并退出。
fn spawn_collector(app: AppHandle, host: Host, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        // 首次采集前的等待（同时响应取消）
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(MONITOR_FIRST_DELAY) => {}
        }
        let mut last_collect: Option<std::time::Instant> = None;
        let mut fail_backoff = Duration::ZERO;
        loop {
            let active = app
                .try_state::<MonitorManager>()
                .is_some_and(|m| m.panel_active(&host.id, ACTIVE_WINDOW_SECS));
            let interval = if !fail_backoff.is_zero() {
                fail_backoff
            } else if active {
                MONITOR_INTERVAL
            } else {
                MONITOR_IDLE_INTERVAL
            };
            if last_collect.map_or(true, |t| t.elapsed() >= interval) {
                collect_round(&app, &host, &cancel, &mut fail_backoff).await;
                last_collect = Some(std::time::Instant::now());
                if cancel.is_cancelled() {
                    break;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(MONITOR_TICK) => {}
            }
        }
    });
}

/// 执行一轮后台采集：复用该主机存活会话的 SSH 连接执行只读脚本，
/// 成功则更新内存状态并落库，失败记录错误并退避；会话关闭触发的取消静默退出。
async fn collect_round(
    app: &AppHandle,
    host: &Host,
    cancel: &CancellationToken,
    fail_backoff: &mut Duration,
) {
    // 终端会话自身的连接：同一主机多标签时取任意一条存活连接即可
    let handle = app
        .try_state::<crate::session::SessionManager>()
        .and_then(|sessions| sessions.handle_for_host(&host.id));
    let Some(handle) = handle else {
        // 无存活会话（会话刚关闭的竞态窗口）：本轮跳过，等取消令牌或下一拍
        return;
    };
    match collect_on_handle(&handle, host, cancel).await {
        Ok(snap) => {
            if let Some(manager) = app.try_state::<MonitorManager>() {
                manager.push(&host.id, &snap);
            }
            // 落库供趋势分析（DB 内部按主机 60 秒节流，不会写得太密）
            if let Some(db) = app.try_state::<std::sync::Arc<Db>>() {
                let _ = save_metric(&db, &host.id, &snap, "auto");
            }
            *fail_backoff = Duration::ZERO;
        }
        Err(e) => {
            // 会话关闭触发的取消不算采集失败，不写入错误状态
            if cancel.is_cancelled() {
                return;
            }
            if let Some(manager) = app.try_state::<MonitorManager>() {
                manager.set_error(&host.id, e);
            }
            *fail_backoff = if fail_backoff.is_zero() {
                MONITOR_FAIL_MIN_INTERVAL
            } else {
                (*fail_backoff * 2).min(MONITOR_FAIL_MAX_INTERVAL)
            };
        }
    }
}

/// 读取主机监控实时状态：最近快照 + 连接以来的历史点（面板打开 5 秒 /
/// 后台静默 30 秒粒度，内存，最多 30 分钟）+ 最近错误。
/// 面板打开 / 切换主机时调用一次即可回填「连接以来」的趋势，
/// 而不是等面板轮询重新积累数据。
#[tauri::command]
pub fn monitor_state(monitor: State<'_, MonitorManager>, host_id: String) -> MonitorState {
    // 面板打开 / 切换主机即视为活跃：采集任务下一个节拍切到 5 秒节奏
    monitor.touch_poll(&host_id);
    monitor.state(&host_id)
}

const MONITOR_SCRIPT: &str = r#"
echo "BEGIN"
echo "LOAD $(cat /proc/loadavg 2>/dev/null | cut -d' ' -f1-3)"
p1=$(grep '^cpu ' /proc/stat)
sleep 0.3
p2=$(grep '^cpu ' /proc/stat)
echo "CPU_RAW $p1|$p2"
echo "MEM $(free -m 2>/dev/null | awk '/Mem:/{print $2, $3, $7}')"
echo "SWAP $(free -m 2>/dev/null | awk '/^Swap:/{print $2, $3}')"
echo "DISK"
df -Pk 2>/dev/null | awk 'NR>1 {print $6 "|" $1 "|" $2 "|" $3 "|" $5}'
echo "NET"
awk 'NR>2 {n=$1; sub(/:.*/,"",n); print n "|" $2 "|" $10}' /proc/net/dev 2>/dev/null
echo "TOP_CPU"
ps -eo user,%cpu,%mem,args --sort=-%cpu 2>/dev/null | head -11
echo "TOP_MEM"
ps -eo user,%cpu,%mem,args --sort=-%mem 2>/dev/null | head -11
echo "END"
"#;

/// 静态信息采集脚本：`KEY|value` 行 + GEO|START/END 包裹的归属地 JSON。
/// 公网查询链式兜底：ip-api（国家名语言由调用方注入的 GEO_LANG 决定）→ ipinfo.io → ipify（仅 IP）；
/// curl 缺失时尝试 wget。全程只读，远端无外网时 GEO 段为空。
const HOST_INFO_SCRIPT: &str = r#"
echo "BEGIN"
os=$(. /etc/os-release 2>/dev/null; echo "$PRETTY_NAME")
[ -z "$os" ] && os=$(uname -s 2>/dev/null)
echo "OS|$os"
echo "KERNEL|$(uname -r 2>/dev/null)"
echo "ARCH|$(uname -m 2>/dev/null)"
echo "HOSTNAME|$(hostname 2>/dev/null || cat /etc/hostname 2>/dev/null)"
echo "UPTIME|$(cut -d. -f1 /proc/uptime 2>/dev/null)"
th=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || grep -c '^processor' /proc/cpuinfo 2>/dev/null)
echo "THREADS|$th"
cores=$(LC_ALL=C lscpu 2>/dev/null | awk -F: '/^Socket\(s\)/{s=$2} /^Core\(s\) per socket/{c=$2} END{gsub(/[ \t]/,"",s); gsub(/[ \t]/,"",c); if(s+0>0&&c+0>0) printf "%d", s*c}')
[ -z "$cores" ] && cores="$th"
echo "CORES|$cores"
model=$(LC_ALL=C lscpu 2>/dev/null | awk -F: '/^Model name/{sub(/^[ \t]+/,"",$2); print $2; exit}')
[ -z "$model" ] && model=$(awk -F: '/^model name/{sub(/^[ \t]+/,"",$2); print $2; exit}' /proc/cpuinfo 2>/dev/null)
echo "CPU_MODEL|$model"
echo "MEM_MB|$(free -m 2>/dev/null | awk '/^Mem:/{print $2}')"
echo "GEO|START"
get() {
  if command -v curl >/dev/null 2>&1; then curl -s4 --max-time 5 "$1" 2>/dev/null;
  elif command -v wget >/dev/null 2>&1; then wget -qO- -T 5 "$1" 2>/dev/null; fi
}
geo=$(get "http://ip-api.com/json/?fields=status,country,countryCode,city,query&lang=$GEO_LANG")
case "$geo" in
  *'"status":"success"'*) ;;
  *) geo=$(get "https://ipinfo.io/json")
     case "$geo" in *'"ip"'*) ;; *) geo=$(get "https://api.ipify.org?format=json");; esac ;;
esac
echo "$geo"
echo "GEO|END"
echo "END"
"#;

/// 虚拟/容器网卡前缀黑名单：统计流量总量时排除，避免 docker/veth 等虚接口重复计数。
const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "lo", "docker", "veth", "br-", "virbr", "vnet", "tun", "tap", "cni", "flannel", "cali",
    "kube", "wg", "zt", "tailscale", "ppp", "ifb", "sit", "gre", "gretap", "erspan", "ip6tnl",
    "ip_vti", "ip6_vti", "vxlan", "macvlan", "ipvlan", "dummy", "nlmon", "bonding_masters",
    "ovs", "genev", "lxd", "lxcbr", "podman", "vboxnet", "vmnet",
];

fn is_virtual_iface(name: &str) -> bool {
    VIRTUAL_IFACE_PREFIXES
        .iter()
        .any(|p| name.starts_with(p))
}

/// 伪文件系统（df 输出的第 1 列 source）：出现即视为系统挂载，默认不展示。
const PSEUDO_FS: &[&str] = &[
    "tmpfs", "devtmpfs", "overlay", "squashfs", "efivarfs", "udev", "none", "shm", "proc",
    "sysfs", "cgroup", "cgroup2", "devpts", "mqueue", "autofs", "configfs", "debugfs",
    "tracefs", "securityfs", "pstore", "hugetlbfs", "fusectl", "bpf", "nsfs", "ramfs",
    "rpc_pipefs", "selinuxfs", "binfmt_misc", "sunrpc", "portal",
];

/// 系统/引导类挂载点前缀：即使是真实块设备也不计入默认展示。
/// 注意 /dev、/run 不在此列：其下的系统挂载（pts、shm、mqueue、udev 等）
/// 已由伪文件系统黑名单覆盖；而真实块设备挂载到这些路径下（如 /dev/vda2、
/// udisks 自动挂载的 /run/media/…）恰恰属于数据盘，前缀过滤会误杀。
const SYSTEM_MOUNT_PREFIXES: &[&str] = &[
    "/boot", "/efi", "/sys", "/proc", "/snap", "/var/snap", "/var/lib", "/var/run",
    "/var/lock", "/etc", "/usr", "/lib", "/bin", "/sbin", "/tmp",
];

/// 容器运行时相关路径子串：出现在挂载点中即过滤。
const CONTAINER_MOUNT_HINTS: &[&str] =
    &["docker", "containerd", "kubelet", "snapd", "podman", "lxc"];

/// 判定该挂载是否作为「数据盘」默认展示：/ 恒保留；伪文件系统、系统路径、
/// 容器运行时路径过滤；/data、/mnt、/media、/home、/opt、/var、NFS、ZFS 等保留。
fn is_data_disk(mount: &str, fs: &str) -> bool {
    if mount == "/" {
        return true;
    }
    let fs_base = fs.rsplit('/').next().unwrap_or(fs);
    if PSEUDO_FS.contains(&fs_base) || fs_base.starts_with("fuse.") {
        return false;
    }
    if SYSTEM_MOUNT_PREFIXES
        .iter()
        .any(|p| mount == *p || mount.starts_with(&format!("{p}/")))
    {
        return false;
    }
    if CONTAINER_MOUNT_HINTS.iter().any(|h| mount.contains(h)) {
        return false;
    }
    true
}

fn parse(text: &str, host: &Host) -> Result<MonitorSnapshot, String> {
    let mut snap = MonitorSnapshot {
        ts: now(),
        host_label: format!("{} ({}@{}:{})", host.name, host.username, host.address, host.port),
        ..Default::default()
    };
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "BEGIN" || line.is_empty() {
            continue;
        }
        if line == "END" {
            break;
        }
        if line == "DISK" {
            section = "disk".to_string();
            continue;
        }
        if line == "NET" {
            section = "net".to_string();
            continue;
        }
        if line == "TOP_CPU" {
            section = "top_cpu".to_string();
            continue;
        }
        if line == "TOP_MEM" {
            section = "top_mem".to_string();
            continue;
        }
        match section.as_str() {
            "disk" => {
                if let Some(d) = parse_disk_line(line) {
                    if is_data_disk(&d.mount, &d.fs) {
                        snap.disks.push(d);
                    }
                }
            }
            "net" => {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() == 3 && !is_virtual_iface(parts[0]) {
                    if let (Ok(rx), Ok(tx)) =
                        (parts[1].parse::<u64>(), parts[2].parse::<u64>())
                    {
                        snap.net.rx_bytes = snap.net.rx_bytes.saturating_add(rx);
                        snap.net.tx_bytes = snap.net.tx_bytes.saturating_add(tx);
                        snap.net.ifaces.push(NetIface {
                            name: parts[0].to_string(),
                            rx_bytes: rx,
                            tx_bytes: tx,
                        });
                    }
                }
            }
            "top_cpu" | "top_mem" => {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // parts[0]=="USER" 是 ps 表头行，不是进程
                if parts.len() >= 4 && parts[0] != "USER" {
                    let proc = TopProc {
                        user: parts[0].to_string(),
                        cpu: parts[1].to_string(),
                        mem: parts[2].to_string(),
                        cmd: parts[3..].join(" "),
                    };
                    if section == "top_cpu" {
                        snap.top_cpu.push(proc);
                    } else {
                        snap.top_mem.push(proc);
                    }
                }
            }
            _ => {
                if let Some(v) = line.strip_prefix("LOAD ") {
                    snap.load = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("CPU_RAW ") {
                    if let Some((before, after)) = v.split_once('|') {
                        if let Some(p) = cpu_percent_from_stat(before.trim(), after.trim()) {
                            snap.cpu_percent = p;
                        }
                    }
                } else if let Some(v) = line.strip_prefix("MEM ") {
                    let nums: Vec<&str> = v.split_whitespace().collect();
                    // free -m 输出：total used ... available；used 采用 total - available（htop 口径）
                    if nums.len() >= 3 {
                        if let (Ok(t), Ok(a)) =
                            (nums[0].parse::<u64>(), nums[2].parse::<u64>())
                        {
                            snap.mem = mem_info(t, t.saturating_sub(a));
                        }
                    }
                } else if let Some(v) = line.strip_prefix("SWAP ") {
                    let nums: Vec<&str> = v.split_whitespace().collect();
                    // free -m Swap 行：total used
                    if nums.len() >= 2 {
                        if let (Ok(t), Ok(u)) =
                            (nums[0].parse::<u64>(), nums[1].parse::<u64>())
                        {
                            snap.swap = mem_info(t, u);
                        }
                    }
                }
            }
        }
    }
    if snap.mem.total_mb == 0 && snap.cpu_percent == 0.0 && snap.disks.is_empty() {
        return Err(crate::i18n::t("无法解析监控数据（服务器可能不是 Linux 或缺少 /proc）", "Cannot parse monitoring data (the server may not be Linux or /proc may be missing)").to_string());
    }
    Ok(snap)
}

fn mem_info(total_mb: u64, used_mb: u64) -> MemInfo {
    MemInfo {
        total_mb,
        used_mb,
        percent: if total_mb > 0 {
            (used_mb as f64 / total_mb as f64) * 100.0
        } else {
            0.0
        },
    }
}

fn parse_disk_line(line: &str) -> Option<DiskInfo> {
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() != 5 {
        return None;
    }
    // df -Pk 输出 1K 块数；total/used 转成 K/M/G/T 展示串，原始数值保留给前端汇总
    let total_kb: u64 = parts[2].trim().parse().ok()?;
    let used_kb: u64 = parts[3].trim().parse().ok()?;
    Some(DiskInfo {
        mount: parts[0].to_string(),
        fs: parts[1].to_string(),
        total: fmt_kb(total_kb),
        used: fmt_kb(used_kb),
        total_kb,
        used_kb,
        percent: parts[4].trim().trim_end_matches('%').parse().unwrap_or(0.0),
    })
}

/// 1K 块数 → df -h 风格展示串：40G / 6.3M / 197M / 1.5T
fn fmt_kb(kb: u64) -> String {
    const K: f64 = 1024.0;
    let (v, unit) = if (kb as f64) >= K * K * K {
        (kb as f64 / (K * K * K), "T")
    } else if (kb as f64) >= K * K {
        (kb as f64 / (K * K), "G")
    } else if kb >= 1024 {
        (kb as f64 / K, "M")
    } else {
        (kb as f64, "K")
    };
    let s = format!("{v:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}{unit}")
}

/// 解析 HOST_INFO_SCRIPT 输出：`KEY|value` 行填充字段；
/// `GEO|START`/`GEO|END` 之间的行 join 后交给 parse_geo 解析归属地 JSON。
fn parse_host_info(text: &str) -> Result<HostInfo, String> {
    let mut info = HostInfo::default();
    let mut in_geo = false;
    let mut geo_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line == "BEGIN" {
            continue;
        }
        if line == "END" {
            break;
        }
        if line == "GEO|START" {
            in_geo = true;
            continue;
        }
        if line == "GEO|END" {
            in_geo = false;
            continue;
        }
        if in_geo {
            geo_lines.push(line);
            continue;
        }
        let Some((key, value)) = line.split_once('|') else {
            continue;
        };
        let value = value.trim();
        match key {
            "OS" => info.os = value.to_string(),
            "KERNEL" => info.kernel = value.to_string(),
            "ARCH" => info.arch = value.to_string(),
            "HOSTNAME" => info.hostname = value.to_string(),
            "UPTIME" => info.uptime_secs = value.parse().unwrap_or(0),
            "THREADS" => info.threads = value.parse().unwrap_or(0),
            "CORES" => info.cores = value.parse().unwrap_or(0),
            "CPU_MODEL" => info.cpu_model = value.to_string(),
            "MEM_MB" => info.mem_total_mb = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    let (ip, country, city) = parse_geo(&geo_lines.join("\n"));
    info.public_ip = ip;
    info.location = format_location(&country, &city);
    if info.os.is_empty() && info.threads == 0 {
        return Err(crate::i18n::t("无法解析服务器信息（服务器可能不是 Linux）", "Cannot parse server information (the server may not be Linux)").to_string());
    }
    Ok(info)
}

fn parse_cpu_times(line: &str) -> Option<Vec<u64>> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let values = parts
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() >= 4 {
        Some(values)
    } else {
        None
    }
}

fn cpu_percent_from_stat(before: &str, after: &str) -> Option<f64> {
    let a = parse_cpu_times(before)?;
    let b = parse_cpu_times(after)?;
    let field = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    let total = |v: &[u64]| -> u64 { (0..8).map(|i| field(v, i)).sum() };
    let idle_all = |v: &[u64]| -> u64 { field(v, 3) + field(v, 4) };
    let delta_total = total(&b).saturating_sub(total(&a));
    let delta_idle = idle_all(&b).saturating_sub(idle_all(&a));
    if delta_total == 0 {
        return Some(0.0);
    }
    let used = delta_total.saturating_sub(delta_idle);
    Some(((used as f64 / delta_total as f64) * 100.0).clamp(0.0, 100.0))
}

/// 将 MonitorSnapshot 转换为 host_metrics 行并写入数据库。
/// 供 monitor_snapshot / agent 会话 / 巡检三个采集点复用。
pub fn save_metric(
    db: &Db,
    host_id: &str,
    snap: &MonitorSnapshot,
    source: &str,
) -> rusqlite::Result<()> {
    let load1: f64 = snap
        .load
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let disks: Vec<MetricDisk> = snap
        .disks
        .iter()
        .map(|d| MetricDisk {
            mount: d.mount.clone(),
            percent: d.percent,
        })
        .collect();
    let top: Vec<MetricTop> = snap
        .top_cpu
        .iter()
        .map(|p| MetricTop {
            cmd: p.cmd.clone(),
            cpu: p.cpu.clone(),
            mem: p.mem.clone(),
        })
        .collect();
    let disks_json = serde_json::to_string(&disks).unwrap_or_else(|_| "[]".to_string());
    let top_json = serde_json::to_string(&top).unwrap_or_else(|_| "[]".to_string());
    db.insert_metric(crate::models::NewMetric {
        host_id,
        ts: snap.ts,
        cpu_percent: snap.cpu_percent,
        load1,
        mem_total_mb: snap.mem.total_mb,
        mem_used_mb: snap.mem.used_mb,
        mem_percent: snap.mem.percent,
        disks_json: &disks_json,
        top_json: &top_json,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_basic_delta() {
        let p = cpu_percent_from_stat(
            "cpu 100 0 100 200 0 0 0 0 0 0",
            "cpu 150 0 150 250 0 0 0 0 0 0",
        )
        .unwrap();
        assert!((p - 66.666).abs() < 0.01, "实际 {p}");
    }

    #[test]
    fn cpu_percent_iowait_counts_as_idle() {
        assert_eq!(
            cpu_percent_from_stat("cpu 0 0 0 0 0 0 0 0", "cpu 0 0 0 0 100 0 0 0"),
            Some(0.0)
        );
        assert_eq!(
            cpu_percent_from_stat("cpu 0 0 0 0 0 0 0 0", "cpu 50 0 0 0 50 0 0 0"),
            Some(50.0)
        );
    }

    #[test]
    fn cpu_percent_counts_irq_and_steal_as_busy() {
        assert_eq!(
            cpu_percent_from_stat("cpu 0 0 0 0 0 0 0 0", "cpu 0 0 0 0 0 25 25 50"),
            Some(100.0)
        );
    }

    #[test]
    fn cpu_percent_guest_not_double_counted() {
        assert_eq!(
            cpu_percent_from_stat("cpu 10 0 0 0 0 0 0 0 0 0", "cpu 10 0 0 0 0 0 0 0 50 50"),
            Some(0.0)
        );
    }

    #[test]
    fn cpu_percent_handles_missing_and_garbage() {
        assert_eq!(cpu_percent_from_stat("cpu 1 2", "cpu 1 2 3 4"), None);
        assert_eq!(cpu_percent_from_stat("notcpu 1 2 3 4", "cpu 1 2 3 4"), None);
        assert_eq!(
            cpu_percent_from_stat("cpu 1 bad 3 4", "cpu 1 2 3 4 5 6 7 8"),
            None
        );
    }

    #[test]
    fn disk_line_parses_kb_and_formats_display() {
        let d = parse_disk_line("/|/dev/sda1|41943040|14680064|35%").unwrap();
        assert_eq!(d.mount, "/");
        assert_eq!(d.fs, "/dev/sda1");
        assert_eq!(d.total_kb, 41943040);
        assert_eq!(d.used_kb, 14680064);
        assert_eq!(d.total, "40G");
        assert_eq!(d.used, "14G");
        assert_eq!(d.percent, 35.0);
        assert!(parse_disk_line("/|/dev/sda1|41943040|14680064").is_none());
        // 非数值容量解析失败返回 None
        assert!(parse_disk_line("/|/dev/sda1|40G|14G|35%").is_none());
        assert_eq!(
            parse_disk_line("/boot|x|1048576|102400|7")
                .unwrap()
                .percent,
            7.0
        );
    }

    #[test]
    fn fmt_kb_matches_df_human_style() {
        assert_eq!(fmt_kb(41943040), "40G");
        assert_eq!(fmt_kb(102236160), "97.5G");
        assert_eq!(fmt_kb(201728), "197M");
        assert_eq!(fmt_kb(1536), "1.5M");
        assert_eq!(fmt_kb(1099511627776 / 1024), "1T");
        assert_eq!(fmt_kb(512), "512K");
    }

    #[test]
    fn data_disk_keeps_root_and_data_mounts() {
        // / 恒保留，即便是 overlay
        assert!(is_data_disk("/", "overlay"));
        assert!(is_data_disk("/", "/dev/mapper/ubuntu--vg-root"));
        // 常见数据盘
        for m in ["/data", "/data1", "/mnt/disk", "/media/usb", "/home", "/opt", "/var", "/srv"] {
            assert!(is_data_disk(m, "/dev/sdb1"), "{m} 应保留");
        }
        // 真实块设备挂载在 /dev、/run 下也是数据盘（实测案例：/dev/vdb1 → /dev/vda2，
        // udisks 自动挂载 U 盘走 /run/media/…）
        assert!(is_data_disk("/dev/vda2", "/dev/vdb1"));
        assert!(is_data_disk("/run/media/user/usb", "/dev/sdc1"));
        // NFS / ZFS 源也保留
        assert!(is_data_disk("/mnt/nfs", "192.168.1.10:/share"));
        assert!(is_data_disk("/data", "rpool/data"));
    }

    #[test]
    fn data_disk_filters_system_and_pseudo_mounts() {
        // 伪文件系统
        for (m, fs) in [
            ("/run", "tmpfs"),
            ("/dev/shm", "tmpfs"),
            ("/tmp", "tmpfs"),
            ("/run/credentials/getty@tty1.service", "tmpfs"),
            ("/sys/firmware/efi/efivars", "efivarfs"),
            ("/var/lib/docker/overlay2/abc", "overlay"),
            ("/snap/core/123", "squashfs"),
        ] {
            assert!(!is_data_disk(m, fs), "{m}({fs}) 应过滤");
        }
        // 真实块设备但系统路径
        for m in ["/boot", "/boot/efi", "/usr", "/etc/ssl", "/var/lib/docker"] {
            assert!(!is_data_disk(m, "/dev/sda1"), "{m} 应过滤");
        }
        // 容器运行时路径子串
        assert!(!is_data_disk("/var/lib/kubelet/pods/x", "/dev/sda1"));
    }

    #[test]
    fn virtual_iface_prefixes_detected() {
        for n in ["lo", "docker0", "veth1234", "br-abc", "tun0", "wg0", "cali123", "vxlan.calico"] {
            assert!(is_virtual_iface(n), "{n} 应为虚拟网卡");
        }
        for n in ["eth0", "ens33", "enp0s3", "eno1", "bond0", "wlan0", "em1"] {
            assert!(!is_virtual_iface(n), "{n} 应为物理网卡");
        }
    }

    #[test]
    fn parse_geo_handles_three_schemas() {
        // ip-api
        let (ip, country, city) = parse_geo(
            r#"{"status":"success","country":"美国","countryCode":"US","city":"Los Angeles","query":"1.2.3.4"}"#,
        );
        assert_eq!((ip.as_str(), country.as_str(), city.as_str()), ("1.2.3.4", "美国", "Los Angeles"));
        // ipinfo
        let (ip, country, _) = parse_geo(r#"{"ip":"5.6.7.8","country":"JP"}"#);
        assert_eq!((ip.as_str(), country.as_str()), ("5.6.7.8", "JP"));
        // ipify 仅 IP
        let (ip, country, _) = parse_geo(r#"{"ip":"9.9.9.9"}"#);
        assert_eq!((ip.as_str(), country.as_str()), ("9.9.9.9", ""));
        // 私网被 ip-api 拒绝 / 非法 JSON / 空串
        assert_eq!(parse_geo(r#"{"status":"fail","message":"private range","query":"192.168.1.1"}"#).0, "");
        assert_eq!(parse_geo("not json").0, "");
        assert_eq!(parse_geo("").0, "");
    }

    #[test]
    fn host_info_parses_full_output() {
        let out = r#"
BEGIN
OS|Ubuntu 24.04.2 LTS
KERNEL|6.8.0-31-generic
ARCH|x86_64
HOSTNAME|web-01
UPTIME|305400
THREADS|8
CORES|4
CPU_MODEL|Intel(R) Xeon(R) Platinum
MEM_MB|16000
GEO|START
{"status":"success","country":"中国","city":"上海","query":"202.96.1.1"}
GEO|END
END
"#;
        let info = parse_host_info(out).unwrap();
        assert_eq!(info.hostname, "web-01");
        assert_eq!(info.os, "Ubuntu 24.04.2 LTS");
        assert_eq!(info.kernel, "6.8.0-31-generic");
        assert_eq!(info.threads, 8);
        assert_eq!(info.cores, 4);
        assert_eq!(info.mem_total_mb, 16000);
        assert_eq!(info.uptime_secs, 305400);
        assert_eq!(info.public_ip, "202.96.1.1");
        assert_eq!(info.location, "中国 · 上海");
    }

    #[test]
    fn host_info_tolerates_missing_geo() {
        let out = "BEGIN\nOS|Debian GNU/Linux 12\nTHREADS|2\nCORES|2\nGEO|START\n\nGEO|END\nEND\n";
        let info = parse_host_info(out).unwrap();
        assert_eq!(info.os, "Debian GNU/Linux 12");
        assert_eq!(info.threads, 2);
        assert!(info.public_ip.is_empty());
        assert!(info.location.is_empty());
    }

    #[test]
    fn snapshot_parses_net_and_swap_and_filters_disks() {
        let host = test_host();
        let out = r#"
BEGIN
LOAD 0.20 0.24 0.30
CPU_RAW cpu 100 0 100 200 0 0 0 0 0 0|cpu 150 0 150 250 0 0 0 0 0 0
MEM 16000 4000 12000
SWAP 2048 512
DISK
/|/dev/mapper/root|103079424|7759462|8%
/run|tmpfs|3145728|1536|1%
/data|/dev/sdb1|524288000|104857600|20%
/boot/efi|/dev/sda1|201728|6451|4%
NET
lo|1000|1000
eth0|1048576|524288
docker0|9999|8888
ens34|2097152|1048576
TOP_CPU
USER       %CPU %MEM COMMAND
root 10.0 1.0 /usr/bin/x
TOP_MEM
USER       %CPU %MEM COMMAND
mysql 1.0 40.0 /usr/sbin/mysqld
END
"#;
        let snap = parse(out, &host).unwrap();
        assert_eq!(snap.swap.total_mb, 2048);
        assert_eq!(snap.swap.used_mb, 512);
        assert!((snap.swap.percent - 25.0).abs() < 0.01);
        // 只剩 / 和 /data
        let mounts: Vec<&str> = snap.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/data"]);
        // lo/docker0 被排除，eth0+ens34 汇总
        assert_eq!(snap.net.rx_bytes, 1048576 + 2097152);
        assert_eq!(snap.net.tx_bytes, 524288 + 1048576);
        assert_eq!(snap.net.ifaces.len(), 2);
        // 两段 TOP 各取进程行，ps 表头（USER 行）被跳过
        assert_eq!(snap.top_cpu.len(), 1);
        assert_eq!(snap.top_cpu[0].cmd, "/usr/bin/x");
        assert_eq!(snap.top_mem.len(), 1);
        assert_eq!(snap.top_mem[0].cmd, "/usr/sbin/mysqld");
    }

    #[test]
    fn monitor_points_dedupe_and_prune() {
        let manager = MonitorManager::default();
        let t = now();
        let snap = |ts: u64, cpu: f64| MonitorSnapshot {
            ts,
            cpu_percent: cpu,
            ..Default::default()
        };
        manager.push("h1", &snap(t, 10.0));
        // 同一时间戳重复推送（面板轮询与后台采集命中同一快照）只保留一个点
        manager.push("h1", &snap(t, 10.0));
        let st = manager.state("h1");
        assert_eq!(st.points.len(), 1);
        assert_eq!(st.points[0].cpu, 10.0);
        assert_eq!(st.latest.unwrap().ts, t);

        // 超过 30 分钟的旧点被清理
        manager.push("h1", &snap(t + POINT_WINDOW_SECS + 1, 20.0));
        let st = manager.state("h1");
        assert_eq!(st.points.len(), 1);
        assert_eq!(st.points[0].ts, t + POINT_WINDOW_SECS + 1);
        assert_eq!(st.points[0].cpu, 20.0);
    }

    #[test]
    fn monitor_snapshot_reuses_only_fresh_result() {
        let manager = MonitorManager::default();
        let t = now();
        // 无会话连接（没有后台采集任务）：不复用缓存，交回调用方实时采集
        manager.push("h1", &MonitorSnapshot { ts: t, ..Default::default() });
        assert!(manager.snapshot_if_fresh("h1", SNAPSHOT_FRESH_SECS).is_none());

        // 会话连接中：新鲜快照直接复用
        manager.register_session("h1", 1);
        assert!(manager.snapshot_if_fresh("h1", SNAPSHOT_FRESH_SECS).is_some());
        // 任务刚建立、尚未采集出结果的主机不复用
        manager.register_session("h2", 2);
        assert!(manager.snapshot_if_fresh("h2", SNAPSHOT_FRESH_SECS).is_none());

        // 过期快照不再复用（交回调用方实时采集兜底）
        let stale = MonitorManager::default();
        stale.register_session("h1", 1);
        stale.push(
            "h1",
            &MonitorSnapshot {
                ts: t - SNAPSHOT_FRESH_SECS - 5,
                ..Default::default()
            },
        );
        assert!(stale.snapshot_if_fresh("h1", SNAPSHOT_FRESH_SECS).is_none());

        // 会话全部关闭（任务停止）后同样不复用，保持面板实时采集
        manager.unregister_session("h1", 1);
        assert!(manager.snapshot_if_fresh("h1", SNAPSHOT_FRESH_SECS).is_none());
    }

    #[test]
    fn monitor_error_cleared_on_success() {
        let manager = MonitorManager::default();
        manager.set_error("h1", "连接失败".to_string());
        let st = manager.state("h1");
        assert_eq!(st.error.as_deref(), Some("连接失败"));
        assert!(st.latest.is_none());
        manager.push("h1", &MonitorSnapshot { ts: now(), ..Default::default() });
        assert!(manager.state("h1").error.is_none());
    }

    #[test]
    fn monitor_panel_activity_window() {
        let manager = MonitorManager::default();
        // 未调用过监控接口的主机不算面板活跃（后台静默 → 低频采集）
        assert!(!manager.panel_active("h1", ACTIVE_WINDOW_SECS));
        // 面板调用过监控接口后进入活跃窗口（5 秒采集节奏）
        manager.touch_poll("h1");
        assert!(manager.panel_active("h1", ACTIVE_WINDOW_SECS));
        // 只影响对应主机
        assert!(!manager.panel_active("h2", ACTIVE_WINDOW_SECS));
    }

    #[test]
    fn monitor_collector_lifecycle_per_host() {
        let manager = MonitorManager::default();
        // 首个会话创建采集任务
        assert!(manager.register_session("h1", 1).is_some());
        // 同一主机第二个会话复用同一任务
        assert!(manager.register_session("h1", 2).is_none());
        // 其它主机独立建任务
        assert!(manager.register_session("h2", 3).is_some());
        // 该主机仍有会话时任务继续
        assert!(manager.unregister_session("h1", 1).is_none());
        // 最后一个会话关闭：返回取消令牌，任务停止
        let cancel = manager.unregister_session("h1", 2).expect("应返回取消令牌");
        cancel.cancel();
        assert!(cancel.is_cancelled());
        // 重新连接会新建任务；注销未知会话是安全空操作
        assert!(manager.register_session("h1", 4).is_some());
        assert!(manager.unregister_session("h3", 9).is_none());
    }

    fn test_host() -> Host {
        Host {
            id: "h1".to_string(),
            name: "test".to_string(),
            address: "127.0.0.1".to_string(),
            port: 22,
            username: "u".to_string(),
            auth_type: crate::models::AuthType::Password,
            key_path: None,
            notes: None,
            created_at: 0,
        }
    }
}
