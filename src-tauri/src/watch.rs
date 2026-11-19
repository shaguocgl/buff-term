//! 值守（实时守护）：与「定时巡检」本质不同——巡检是按周期体检，值守是盯突发状态。
//!
//! 两种模式由用户按主机选择：
//! - client：buffTerm 在线时，每 5 秒采样一次只读指标，本地判定阈值，事件触发 AI 快诊与通知。
//! - server：把同一套规则生成 systemd 常驻脚本装到服务器上，桌面离线也能判定并推送到 webhook。
//!
//! 判定在本地完成（不烧 AI），只有越界事件才调用模型，避免值守本身变成成本中心。
//! 全部采集均为只读命令；服务器端脚本只读 /proc、df、ss、journalctl。

use crate::db::Db;
use crate::models::{GuardHostStatus, GuardSettings, InspectionReport};
use crate::monitor::{self, MonitorSnapshot};
use crate::russh::RusshManager;
use crate::util::now;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use tokio_util::sync::CancellationToken;

/// 采样间隔：5 秒足够发现「突然过高」，又不会给服务器和连接池造成压力。
const SAMPLE_SECS: u64 = 5;
/// 安全类采样（SSH 失败、连接数）每 12 个采样点（约 60 秒）做一次。
const SECURITY_EVERY: u32 = 12;
/// 采集失败后的退避时间，避免主机离线时每 5 秒猛重连。
const ERROR_BACKOFF_SECS: u64 = 30;
/// 服务器端事件同步间隔：桌面在线时每 5 分钟拉取一次，补齐 AI 复盘。
const SERVER_SYNC_SECS: u64 = 300;

pub struct GuardManager {
    tasks: Mutex<HashMap<String, CancellationToken>>,
    status: Mutex<HashMap<String, GuardHostStatus>>,
}

impl Default for GuardManager {
    fn default() -> Self {
        GuardManager {
            tasks: Mutex::new(HashMap::new()),
            status: Mutex::new(HashMap::new()),
        }
    }
}

impl GuardManager {
    fn stop_all(&self) {
        for (_, token) in self.tasks.lock().unwrap().drain() {
            token.cancel();
        }
        self.status.lock().unwrap().clear();
    }

    fn set_status(&self, host_id: &str, mode: &str, patch: StatusPatch) {
        let mut map = self.status.lock().unwrap();
        let entry = map
            .entry(host_id.to_string())
            .or_insert_with(|| GuardHostStatus {
                host_id: host_id.to_string(),
                mode: mode.to_string(),
                ..Default::default()
            });
        entry.mode = mode.to_string();
        match patch {
            StatusPatch::Sample(ts) => {
                entry.running = true;
                entry.last_sample_at = ts;
                entry.error = None;
            }
            StatusPatch::Error(err) => {
                entry.running = false;
                entry.error = Some(err);
            }
            StatusPatch::Event(ts) => entry.last_event_at = ts,
        }
    }
}

enum StatusPatch {
    Sample(u64),
    Error(String),
    Event(u64),
}

/// 应用启动时恢复值守任务（客户端模式）。
pub fn start_from_settings(app: &AppHandle) {
    let settings = match app.state::<Arc<Db>>().get_guard_settings() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[guard] failed to read settings: {e}");
            return;
        }
    };
    restart_client_tasks(app, &settings);
}

/// 保存设置或启动时调用：先停掉所有客户端任务，再按新设置重建。
pub fn restart_client_tasks(app: &AppHandle, settings: &GuardSettings) {
    let manager = app.state::<GuardManager>();
    manager.stop_all();
    // 只有客户端侧守护需要本机常驻；服务器端守护不依赖桌面是否运行
    awake::set(settings.enabled && settings.hosts.iter().any(|h| h.mode == "client"));
    if !settings.enabled {
        return;
    }
    for gh in &settings.hosts {
        let token = CancellationToken::new();
        manager
            .tasks
            .lock()
            .unwrap()
            .insert(gh.host_id.clone(), token.clone());
        if gh.mode == "client" {
            spawn_client_loop(app.clone(), gh.host_id.clone(), settings.clone(), token);
        } else {
            spawn_server_sync_loop(app.clone(), gh.host_id.clone(), token);
        }
    }
}

#[tauri::command]
pub fn get_guard_settings(db: State<'_, Arc<Db>>) -> Result<GuardSettings, String> {
    db.get_guard_settings()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取值守设置失败", "Failed to read guard settings")))
}

#[tauri::command]
pub fn save_guard_settings(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    mut settings: GuardSettings,
) -> Result<GuardSettings, String> {
    settings.cpu_percent = settings.cpu_percent.clamp(10, 99);
    settings.mem_percent = settings.mem_percent.clamp(10, 99);
    settings.disk_percent = settings.disk_percent.clamp(10, 99);
    settings.ssh_fail_per_min = settings.ssh_fail_per_min.clamp(1, 1000);
    settings.conns_threshold = settings.conns_threshold.clamp(10, 100_000);
    settings.load_per_core = settings.load_per_core.clamp(0.5, 64.0);
    settings.inode_percent = settings.inode_percent.clamp(10, 99);
    settings.duration_secs = settings.duration_secs.clamp(10, 600);
    settings.cooldown_minutes = settings.cooldown_minutes.min(1440);

    // 进程名会拼进远端 shell 与服务器脚本：严格白名单过滤，杜绝命令注入
    settings.svc_procs = settings
        .svc_procs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
        })
        .collect();
    settings.svc_procs.sort();
    settings.svc_procs.dedup();
    settings.svc_procs.truncate(16);
    settings.svc_ports.sort_unstable();
    settings.svc_ports.dedup();
    settings.svc_ports.truncate(16);

    // 只保留仍存在的主机并按 host_id 去重；模式只接受 client/server
    let mut seen = std::collections::HashSet::new();
    settings.hosts.retain_mut(|gh| {
        if !seen.insert(gh.host_id.clone()) {
            return false;
        }
        gh.mode = if gh.mode == "server" { "server".into() } else { "client".into() };
        crate::hosts::load_host(&db, &gh.host_id).is_ok()
    });
    if settings.hosts.is_empty() {
        settings.enabled = false;
    }

    db.save_guard_settings(&settings)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存值守设置失败", "Failed to save guard settings")))?;
    restart_client_tasks(&app, &settings);
    // 托盘提示与「关窗隐藏」状态跟随值守开关变化
    crate::tray::refresh(&app);
    Ok(settings)
}

#[tauri::command]
pub fn guard_status(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
) -> Result<Vec<GuardHostStatus>, String> {
    let settings = db.get_guard_settings().unwrap_or_default();
    let manager = app.state::<GuardManager>();
    let statuses = manager.status.lock().unwrap();
    Ok(settings
        .hosts
        .iter()
        .map(|gh| {
            let mut st = statuses
                .get(&gh.host_id)
                .cloned()
                .unwrap_or_else(|| GuardHostStatus {
                    host_id: gh.host_id.clone(),
                    mode: gh.mode.clone(),
                    ..Default::default()
                });
            st.mode = gh.mode.clone();
            st
        })
        .collect())
}

// ---------- 客户端侧守护 ----------

#[derive(Clone, Default)]
struct SecuritySample {
    ssh_fail_per_min: u32,
    established: u32,
    oom_events: u32,
    inode_percent: u32,
    cores: u32,
    ports: Vec<u16>,
    failed_units: Vec<String>,
    proc_missing: Vec<String>,
}

struct Trigger {
    rule: &'static str,
    severity: &'static str,
    message: String,
}

fn spawn_client_loop(
    app: AppHandle,
    host_id: String,
    settings: GuardSettings,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        let mut state = EvalState::default();
        let firing: Arc<Mutex<HashSet<&'static str>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut tick: u32 = 0;
        let mut security: Option<SecuritySample> = None;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let started = Instant::now();
            let db = app.state::<Arc<Db>>();
            let host = match crate::hosts::load_host(&db, &host_id) {
                Ok(h) => h,
                Err(e) => {
                    app.state::<GuardManager>()
                        .set_status(&host_id, "client", StatusPatch::Error(e));
                    sleep_or_cancel(ERROR_BACKOFF_SECS, &cancel).await;
                    continue;
                }
            };
            let russh = app.state::<RusshManager>();
            match monitor::collect_russh(&host, &russh).await {
                Ok(snap) => {
                    if tick.is_multiple_of(SECURITY_EVERY) {
                        security = security_sample(&host, &russh, &settings).await.ok();
                    }
                    let triggers = evaluate(&settings, &snap, security.as_ref(), &mut state);
                    app.state::<GuardManager>()
                        .set_status(&host_id, "client", StatusPatch::Sample(snap.ts));
                    for trigger in triggers {
                        // 事件在后台研判/推送，采样不停摆；同一规则同一时间只处理一个，
                        // 防止 AI 研判期间下个采样点又触发一次。
                        let rule = trigger.rule;
                        {
                            let mut active = firing.lock().unwrap();
                            if !active.insert(rule) {
                                continue;
                            }
                        }
                        let app2 = app.clone();
                        let host2 = host.clone();
                        let settings2 = settings.clone();
                        let snap2 = snap.clone();
                        let sec2 = security.clone();
                        let firing2 = firing.clone();
                        tauri::async_runtime::spawn(async move {
                            fire_event(&app2, &host2, &settings2, &snap2, sec2.as_ref(), trigger)
                                .await;
                            firing2.lock().unwrap().remove(rule);
                        });
                    }
                    tick = tick.wrapping_add(1);
                }
                Err(e) => {
                    app.state::<crate::monitor::MonitorManager>()
                        .set_error(&host.id, e.clone());
                    app.state::<GuardManager>()
                        .set_status(&host_id, "client", StatusPatch::Error(e));
                    sleep_or_cancel(ERROR_BACKOFF_SECS, &cancel).await;
                    continue;
                }
            }
            let elapsed = started.elapsed().as_secs();
            let wait = SAMPLE_SECS.saturating_sub(elapsed).max(1);
            sleep_or_cancel(wait, &cancel).await;
        }
        log::info!("[guard] client loop stopped: {host_id}");
    });
}

async fn sleep_or_cancel(secs: u64, cancel: &CancellationToken) {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
        _ = cancel.cancelled() => {}
    }
}

/// 安全/事件类指标：SSH 失败、连接数、OOM、inode、失败单元、进程/端口存活。
/// journalctl/systemctl 不可读时对应项为 0——宁可漏报也不误报。
fn security_script(settings: &GuardSettings) -> String {
    let mut script = String::from(
        "printf 'SSHFAIL '; (journalctl -q --since '-1 min' -u ssh -u sshd 2>/dev/null || true) | grep -c 'Failed password' || true; \
         printf 'ESTAB '; (ss -tn state established 2>/dev/null || true) | tail -n +2 | wc -l; \
         printf 'OOM '; (journalctl -q -k --since '-2 min' 2>/dev/null || true) | grep -cE 'Out of memory|oom-kill' || true; \
         printf 'INODE '; df -Pi / 2>/dev/null | awk 'NR==2{gsub(\"%\",\"\",$5); print $5}'; \
         printf 'CORES '; (nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0); \
         printf 'PORTS '; (ss -tln 2>/dev/null || true) | awk 'NR>1{print $4}' | sed 's/.*[.:]//' | sort -n -u | tr '\\n' ' '; echo; \
         (systemctl --failed --no-legend 2>/dev/null || true) | awk '{print \"FAILEDUNIT \" $2}'; ",
    );
    if !settings.svc_procs.is_empty() {
        let names: Vec<String> = settings.svc_procs.iter().map(|n| sh_quote(n)).collect();
        script.push_str(&format!(
            "for n in {}; do if ps -eo comm= 2>/dev/null | grep -qx \"$n\"; then echo \"PROC $n 1\"; else echo \"PROC $n 0\"; fi; done; ",
            names.join(" ")
        ));
    }
    script
}

async fn security_sample(
    host: &crate::models::Host,
    russh: &RusshManager,
    settings: &GuardSettings,
) -> Result<SecuritySample, String> {
    let out = russh
        .exec(host, &security_script(settings), Duration::from_secs(20))
        .await?;
    parse_security(&out.text).ok_or_else(|| "unexpected security sample output".to_string())
}

fn parse_security(text: &str) -> Option<SecuritySample> {
    let mut sample = SecuritySample::default();
    let mut seen_ssh = false;
    let mut seen_estab = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("SSHFAIL ") {
            sample.ssh_fail_per_min = v.trim().parse().ok()?;
            seen_ssh = true;
        } else if let Some(v) = line.strip_prefix("ESTAB ") {
            sample.established = v.trim().parse().ok()?;
            seen_estab = true;
        } else if let Some(v) = line.strip_prefix("OOM ") {
            sample.oom_events = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("INODE ") {
            sample.inode_percent = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("CORES ") {
            sample.cores = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("PORTS ") {
            sample.ports = v
                .split_whitespace()
                .filter_map(|p| p.parse::<u16>().ok())
                .collect();
        } else if let Some(v) = line.strip_prefix("FAILEDUNIT ") {
            let name = v.trim();
            if !name.is_empty() {
                sample.failed_units.push(name.to_string());
            }
        } else if let Some(v) = line.strip_prefix("PROC ") {
            if let Some((name, ok)) = v.rsplit_once(' ') {
                if ok.trim() == "0" {
                    sample.proc_missing.push(name.trim().to_string());
                }
            }
        }
    }
    if seen_ssh && seen_estab {
        Some(sample)
    } else {
        None
    }
}

fn bump(counters: &mut HashMap<&'static str, u32>, rule: &'static str, over: bool, duration_secs: u32) -> bool {
    let hits = counters.entry(rule).or_insert(0);
    if over {
        *hits += 1;
        (*hits as u64) * SAMPLE_SECS >= u64::from(duration_secs)
    } else {
        *hits = 0;
        false
    }
}

/// 客户端值守的跨采样状态：持续时间计数、服务探测连续失败次数、已告警的失败单元。
#[derive(Default)]
struct EvalState {
    counters: HashMap<&'static str, u32>,
    proc_miss: HashMap<String, u32>,
    port_miss: HashMap<String, u32>,
    alerted_units: HashSet<String>,
}

fn bump_minute(
    counters: &mut HashMap<&'static str, u32>,
    rule: &'static str,
    over: bool,
    duration_secs: u32,
) -> bool {
    let hits = counters.entry(rule).or_insert(0);
    if over {
        *hits += 1;
        (*hits as u64) * 60 >= u64::from(duration_secs.max(60))
    } else {
        *hits = 0;
        false
    }
}

fn evaluate(
    settings: &GuardSettings,
    snap: &MonitorSnapshot,
    security: Option<&SecuritySample>,
    state: &mut EvalState,
) -> Vec<Trigger> {
    let mut out = Vec::new();
    let duration = settings.duration_secs;

    if settings.cpu_enabled
        && bump(&mut state.counters, "cpu", snap.cpu_percent >= f64::from(settings.cpu_percent), duration)
    {
        state.counters.insert("cpu", 0);
        out.push(Trigger {
            rule: "cpu",
            severity: "high",
            message: if crate::i18n::is_en() {
                format!("CPU stayed at {:.0}% ≥ threshold {}%", snap.cpu_percent, settings.cpu_percent)
            } else {
                format!("CPU 占用持续 {:.0}% ≥ 阈值 {}%", snap.cpu_percent, settings.cpu_percent)
            },
        });
    }

    if settings.mem_enabled
        && bump(&mut state.counters, "mem", snap.mem.percent >= f64::from(settings.mem_percent), duration)
    {
        state.counters.insert("mem", 0);
        out.push(Trigger {
            rule: "mem",
            severity: "high",
            message: if crate::i18n::is_en() {
                format!("Memory stayed at {:.0}% ≥ threshold {}%", snap.mem.percent, settings.mem_percent)
            } else {
                format!("内存占用持续 {:.0}% ≥ 阈值 {}%", snap.mem.percent, settings.mem_percent)
            },
        });
    }

    if settings.disk_enabled {
        let worst = snap
            .disks
            .iter()
            .max_by(|a, b| a.percent.partial_cmp(&b.percent).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(disk) = worst {
            if bump(
                &mut state.counters,
                "disk",
                disk.percent >= f64::from(settings.disk_percent),
                duration,
            ) {
                state.counters.insert("disk", 0);
                out.push(Trigger {
                    rule: "disk",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!("Disk {} stayed at {:.0}% ≥ threshold {}%", disk.mount, disk.percent, settings.disk_percent)
                    } else {
                        format!("磁盘 {} 占用持续 {:.0}% ≥ 阈值 {}%", disk.mount, disk.percent, settings.disk_percent)
                    },
                });
            }
        }
    }

    if settings.conns_enabled {
        if let Some(sec) = security {
            if bump(
                &mut state.counters,
                "conns",
                sec.established >= settings.conns_threshold,
                duration,
            ) {
                state.counters.insert("conns", 0);
                out.push(Trigger {
                    rule: "conns",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!("Established connections stayed at {} ≥ threshold {}", sec.established, settings.conns_threshold)
                    } else {
                        format!("已建立连接数持续 {} ≥ 阈值 {}", sec.established, settings.conns_threshold)
                    },
                });
            }
        }
    }

    if settings.ssh_fail_enabled {
        if let Some(sec) = security {
            if sec.ssh_fail_per_min >= settings.ssh_fail_per_min {
                out.push(Trigger {
                    rule: "ssh_fail",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!(
                            "SSH login failures {} per minute ≥ threshold {}, possible brute force",
                            sec.ssh_fail_per_min, settings.ssh_fail_per_min
                        )
                    } else {
                        format!(
                            "SSH 登录失败 {} 次/分钟 ≥ 阈值 {}，疑似暴力破解",
                            sec.ssh_fail_per_min, settings.ssh_fail_per_min
                        )
                    },
                });
            }
        }
    }

    if settings.load_enabled {
        if let Some(cores) = security.map(|s| s.cores).filter(|c| *c > 0) {
            if let Some(load1) = snap
                .load
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
            {
                let per = load1 / f64::from(cores);
                if bump(&mut state.counters, "load", per >= settings.load_per_core, duration) {
                    state.counters.insert("load", 0);
                    out.push(Trigger {
                        rule: "load",
                        severity: "medium",
                        message: if crate::i18n::is_en() {
                            format!(
                                "Load per core {:.1} ≥ threshold {:.1} ({} cores)",
                                per, settings.load_per_core, cores
                            )
                        } else {
                            format!(
                                "每核负载 {:.1} ≥ 阈值 {:.1}（{} 核）",
                                per, settings.load_per_core, cores
                            )
                        },
                    });
                }
            }
        }
    }

    if let Some(sec) = security {
        if settings.inode_enabled
            && bump_minute(
                &mut state.counters,
                "inode",
                sec.inode_percent > 0 && sec.inode_percent >= settings.inode_percent,
                duration,
            )
        {
            state.counters.insert("inode", 0);
            out.push(Trigger {
                rule: "inode",
                severity: "high",
                message: if crate::i18n::is_en() {
                    format!("Inode usage {}% ≥ threshold {}%", sec.inode_percent, settings.inode_percent)
                } else {
                    format!("inode 使用率 {}% ≥ 阈值 {}%", sec.inode_percent, settings.inode_percent)
                },
            });
        }

        if settings.oom_enabled && sec.oom_events > 0 {
            out.push(Trigger {
                rule: "oom",
                severity: "high",
                message: if crate::i18n::is_en() {
                    format!("{} OOM kill event(s) in the last 2 minutes", sec.oom_events)
                } else {
                    format!("最近 2 分钟出现 {} 次 OOM kill", sec.oom_events)
                },
            });
        }

        if settings.units_enabled {
            // 只在出现「新」失败单元时告警；恢复后从集合移除，再次失败会重新告警
            state
                .alerted_units
                .retain(|u| sec.failed_units.iter().any(|f| f == u));
            let new_units: Vec<&String> = sec
                .failed_units
                .iter()
                .filter(|u| !state.alerted_units.contains(*u))
                .collect();
            if !new_units.is_empty() {
                for u in &new_units {
                    state.alerted_units.insert((*u).clone());
                }
                let names = new_units
                    .iter()
                    .map(|u| u.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(Trigger {
                    rule: "units",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!("New failed systemd unit(s): {names}")
                    } else {
                        format!("出现新的 systemd 失败单元：{names}")
                    },
                });
            }
        }

        if settings.svc_enabled {
            let mut missing = Vec::new();
            for name in &settings.svc_procs {
                let miss = state.proc_miss.entry(name.clone()).or_insert(0);
                if sec.proc_missing.iter().any(|m| m == name) {
                    *miss += 1;
                    if *miss >= 3 {
                        missing.push(name.clone());
                        *miss = 0;
                    }
                } else {
                    *miss = 0;
                }
            }
            if !missing.is_empty() {
                out.push(Trigger {
                    rule: "svc_proc",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!("Process(es) not running: {}", missing.join(", "))
                    } else {
                        format!("进程未运行（连续 3 次探测）：{}", missing.join(", "))
                    },
                });
            }

            let mut down = Vec::new();
            for port in &settings.svc_ports {
                let miss = state.port_miss.entry(port.to_string()).or_insert(0);
                if sec.ports.contains(port) {
                    *miss = 0;
                } else {
                    *miss += 1;
                    if *miss >= 3 {
                        down.push(port.to_string());
                        *miss = 0;
                    }
                }
            }
            if !down.is_empty() {
                out.push(Trigger {
                    rule: "svc_port",
                    severity: "high",
                    message: if crate::i18n::is_en() {
                        format!("Port(s) not listening: {}", down.join(", "))
                    } else {
                        format!("端口未监听（连续 3 次探测）：{}", down.join(", "))
                    },
                });
            }
        }
    }

    out
}

fn snapshot_markdown(snap: &MonitorSnapshot, security: Option<&SecuritySample>) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "- CPU：{:.1}%\n- 内存：{:.1}%（{} MB / {} MB）\n- 负载：{}\n",
        snap.cpu_percent, snap.mem.percent, snap.mem.used_mb, snap.mem.total_mb, snap.load
    ));
    if let Some(disk) = snap
        .disks
        .iter()
        .max_by(|a, b| a.percent.partial_cmp(&b.percent).unwrap_or(std::cmp::Ordering::Equal))
    {
        md.push_str(&format!("- 磁盘 {}：{:.1}%\n", disk.mount, disk.percent));
    }
    if let Some(sec) = security {
        md.push_str(&format!(
            "- 已建立连接：{}\n- SSH 失败登录（最近 1 分钟）：{}\n- inode 使用：{}%\n- OOM（最近 2 分钟）：{}\n",
            sec.established, sec.ssh_fail_per_min, sec.inode_percent, sec.oom_events
        ));
        if !sec.failed_units.is_empty() {
            md.push_str(&format!(
                "- systemd 失败单元：{}\n",
                sec.failed_units.join(", ")
            ));
        }
    }
    md.push_str("\n```text\n");
    md.push_str(crate::i18n::t("TOP CPU：\n", "TOP CPU:\n"));
    for p in snap.top_cpu.iter().take(5) {
        md.push_str(&format!("  {}%  {}%\n", p.cpu, p.cmd));
    }
    md.push_str(crate::i18n::t("TOP 内存：\n", "TOP memory:\n"));
    for p in snap.top_mem.iter().take(5) {
        md.push_str(&format!("  {}%  {}%\n", p.mem, p.cmd));
    }
    md.push_str("```\n");
    md
}

/// 事件 → 快照 → AI 快诊 → 落库 → 复用巡检通知链路。
async fn fire_event(
    app: &AppHandle,
    host: &crate::models::Host,
    settings: &GuardSettings,
    snap: &MonitorSnapshot,
    security: Option<&SecuritySample>,
    trigger: Trigger,
) {
    let db = app.state::<Arc<Db>>();
    let now_ts = now();
    // 冷却：同一主机同一规则在窗口内只告警一次，避免持续越界刷屏
    if let Ok(Some(last)) = db.last_guard_event(&host.id, trigger.rule) {
        if now_ts.saturating_sub(last) < u64::from(settings.cooldown_minutes) * 60 {
            return;
        }
    }

    let host_label = format!("{} ({})", host.name, host.label_address());
    let snapshot = snapshot_markdown(snap, security);
    let started = Instant::now();
    let ai = quick_diagnose(app, host, &trigger.message, &snapshot).await;

    let mut markdown = format!(
        "## {}\n\n{}\n\n### {}\n\n{}",
        crate::i18n::t("值守事件", "Guard event"),
        trigger.message,
        crate::i18n::t("现场快照", "Snapshot"),
        snapshot
    );
    match ai {
        Ok(text) => markdown.push_str(&format!(
            "\n### {}\n\n{}\n",
            crate::i18n::t("AI 研判", "AI assessment"),
            text
        )),
        Err(e) => markdown.push_str(&format!(
            "\n### {}\n\n{}\n",
            crate::i18n::t("AI 研判不可用", "AI assessment unavailable"),
            e
        )),
    }

    let (provider_id, provider_name, model) = match crate::ai::resolve_active_ai(&db) {
        Ok((p, m)) => (p.id, p.name, m),
        Err(_) => (String::new(), String::new(), String::new()),
    };
    let risk = trigger.severity.to_string();
    let html = crate::inspection::wrap_email_html(
        &host_label,
        &risk,
        "guard",
        &crate::inspection::markdown_to_html(&markdown),
    );
    let report = InspectionReport {
        id: uuid::Uuid::new_v4().to_string(),
        host_id: host.id.clone(),
        host_label,
        provider_id,
        provider_name,
        model,
        status: "success".to_string(),
        risk_level: risk,
        summary: trigger.message.clone(),
        markdown,
        html,
        email_sent: false,
        webhook_sent: false,
        source: "guard".to_string(),
        notify_skip: None,
        error: None,
        created_at: now_ts,
        finished_at: Some(now()),
        duration_ms: Some(started.elapsed().as_millis() as u64),
    };
    let _ = db.insert_inspection_report(&report);
    let outcome = crate::alert::dispatch_inspection(&db, &report).await;
    let mut updated = report.clone();
    updated.email_sent = outcome.email_sent;
    updated.webhook_sent = outcome.webhook_sent;
    updated.notify_skip = outcome.skip;
    let _ = db.update_inspection_report(&updated);
    let _ = db.insert_guard_event(&crate::models::GuardEventRecord {
        id: uuid::Uuid::new_v4().to_string(),
        host_id: host.id.clone(),
        rule: trigger.rule.to_string(),
        severity: trigger.severity.to_string(),
        message: trigger.message.clone(),
        details: snapshot,
        report_id: Some(report.id.clone()),
        created_at: now_ts,
    });
    app.state::<GuardManager>()
        .set_status(&host.id, "client", StatusPatch::Event(now_ts));
    log::warn!("[guard] {} {}: {}", host.id, trigger.rule, trigger.message);
}

/// 单次快速研判：只发一条消息、不给工具，超时 45 秒。
/// 失败不阻塞告警——规则本身的结论已经足够推送。
async fn quick_diagnose(
    app: &AppHandle,
    host: &crate::models::Host,
    event_message: &str,
    snapshot: &str,
) -> Result<String, String> {
    let db = app.state::<Arc<Db>>();
    let (provider, model) = crate::ai::resolve_active_ai(&db)?;
    let key = crate::credentials::get_api_key(&provider.id)
        .ok_or_else(|| crate::i18n::t("API Key 未找到", "API key not found").to_string())?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("HTTP 客户端初始化失败", "Failed to initialize the HTTP client")))?;
    let system = crate::i18n::t(
        "你是服务器值守助手，负责判断突发告警的真实性。只依据给出的快照分析，不要臆造数据。输出 Markdown：第一行给出最可能原因（不超过 40 字），随后 3-6 条要点，说明是否为真实故障或攻击、证据、以及建议的只读排查命令。不要执行任何命令。",
        "You are a server watch assistant judging whether an alert is real. Use only the provided snapshot; do not invent data. Output Markdown: the first line is the most likely cause (max 40 chars), then 3-6 bullets covering whether it is a real fault or attack, the evidence, and read-only troubleshooting commands. Do not execute anything.",
    );
    let user = format!(
        "{}: {}\n{}: {}\n\n{}",
        crate::i18n::t("主机", "Host"),
        host.name,
        crate::i18n::t("事件", "Event"),
        event_message,
        snapshot
    );
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "max_tokens": 500,
        "temperature": 0.2,
        "stream": false,
    });
    let resp = client
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("请求 AI 失败", "AI request failed")))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("HTTP {}: {}", status.as_u16(), detail));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", crate::i18n::t("解析 AI 响应失败", "Failed to parse the AI response")))?;
    value["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| crate::i18n::t("AI 未返回内容", "The AI returned no content").to_string())
}

// ---------- 服务器端事件回传与 AI 复盘 ----------
//
// 服务器端守护在桌面离线时只做「检测 + 直接推送」，不持有模型密钥。
// 事件同时写进服务器的 events.jsonl；buffTerm 在线后拉取并补做 AI 复盘，
// 报告落进巡检面板（不再重复推送，避免凌晨二次打扰）。

fn fnv1a(input: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in input.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn parse_server_event_line(
    line: &str,
    host_id: &str,
) -> Option<crate::models::ServerGuardEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_ts = value.get("ts")?.as_u64()?;
    let rule = value.get("rule")?.as_str()?.to_string();
    let message = value.get("message")?.as_str()?.trim().to_string();
    if rule.is_empty() || message.is_empty() {
        return None;
    }
    let severity = value
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("high")
        .to_string();
    let metrics = value
        .get("metrics")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(crate::models::ServerGuardEvent {
        id: format!(
            "srv:{host_id}:{event_ts}:{rule}:{:x}",
            fnv1a(&message)
        ),
        host_id: host_id.to_string(),
        rule,
        severity,
        message,
        metrics,
        event_ts,
    })
}

/// 拉取服务器端事件并入库（重复拉取不会重复入库）。返回新增条数。
async fn pull_server_events(app: &AppHandle, host_id: &str) -> Result<u32, String> {
    let db = app.state::<Arc<Db>>();
    let host = crate::hosts::load_host(&db, host_id)?;
    let russh = app.state::<RusshManager>();
    let out = russh
        .exec(
            &host,
            "tail -n 200 /var/lib/buffterm-guard/events.jsonl 2>/dev/null || true",
            Duration::from_secs(20),
        )
        .await?;
    let mut inserted = 0u32;
    for line in out.text.lines() {
        if let Some(event) = parse_server_event_line(line, host_id) {
            if db.insert_server_guard_event(&event).unwrap_or(false) {
                inserted += 1;
            }
        }
    }
    Ok(inserted)
}

/// 对尚未分析的事件补做 AI 复盘。AI 不可用或调用失败时保留待分析状态，
/// 下次同步继续重试；已推送过的原始告警不会重复发送。
async fn analyze_pending_events(app: &AppHandle, host_id: &str) -> Result<u32, String> {
    let db = app.state::<Arc<Db>>();
    let pending = db
        .pending_server_guard_events(host_id, 10)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取待复盘事件失败", "Failed to read pending events")))?;
    if pending.is_empty() {
        return Ok(0);
    }
    if crate::ai::resolve_active_ai(&db).is_err() {
        return Ok(0);
    }
    let host = crate::hosts::load_host(&db, host_id)?;
    let host_label = format!("{} ({})", host.name, host.label_address());
    let mut analyzed = 0u32;
    for event in pending {
        let russh = app.state::<RusshManager>();
        let live = monitor::collect_russh(&host, &russh).await.ok();
        let mut context = format!(
            "{}: {}\n{}: {}\n{}: {}\n{}: {}\n",
            crate::i18n::t("主机", "Host"),
            host.name,
            crate::i18n::t("规则", "Rule"),
            event.rule,
            crate::i18n::t("事件", "Event"),
            event.message,
            crate::i18n::t("事件时刻指标", "Metrics at event time"),
            event.metrics
        );
        if let Some(snap) = &live {
            context.push_str(&format!(
                "\n{}\n{}",
                crate::i18n::t("当前快照：", "Current snapshot:"),
                snapshot_markdown(snap, None)
            ));
        }
        let ai = match quick_diagnose(app, &host, &event.message, &context).await {
            Ok(text) => text,
            Err(e) => {
                log::warn!("[guard] analysis for {} failed: {e}", event.id);
                break;
            }
        };
        let event_time_line = format!(
            "{}: {}",
            crate::i18n::t("事件时间（epoch 秒）", "Event time (epoch seconds)"),
            event.event_ts
        );
        let mut markdown = format!(
            "## {}\n\n{}\n\n- {}: {}\n- {}: {}\n- {}\n\n### {}\n\n{}\n",
            crate::i18n::t("值守事件复盘（服务器端检测）", "Guard event review (server-side detection)"),
            event.message,
            crate::i18n::t("规则", "Rule"),
            event.rule,
            crate::i18n::t("严重度", "Severity"),
            event.severity,
            event_time_line,
            crate::i18n::t("AI 研判", "AI assessment"),
            ai
        );
        if !event.metrics.is_empty() {
            markdown.push_str(&format!(
                "\n### {}\n\n```text\n{}\n```\n",
                crate::i18n::t("事件时刻指标", "Metrics at event time"),
                event.metrics
            ));
        }
        let (provider_id, provider_name, model) = match crate::ai::resolve_active_ai(&db) {
            Ok((p, m)) => (p.id, p.name, m),
            Err(_) => (String::new(), String::new(), String::new()),
        };
        let risk = event.severity.clone();
        let html = crate::inspection::wrap_email_html(
            &host_label,
            &risk,
            "guard",
            &crate::inspection::markdown_to_html(&markdown),
        );
        let report = InspectionReport {
            id: uuid::Uuid::new_v4().to_string(),
            host_id: host.id.clone(),
            host_label: host_label.clone(),
            provider_id,
            provider_name,
            model,
            status: "success".to_string(),
            risk_level: risk,
            summary: event.message.clone(),
            markdown,
            html,
            email_sent: false,
            webhook_sent: false,
            source: "guard".to_string(),
            notify_skip: None,
            error: None,
            created_at: now(),
            finished_at: Some(now()),
            duration_ms: None,
        };
        let _ = db.insert_inspection_report(&report);
        let _ = db.set_server_guard_report(&event.id, &report.id);
        analyzed += 1;
    }
    Ok(analyzed)
}

async fn sync_server_host(app: &AppHandle, host_id: &str) -> Result<(u32, u32), String> {
    let pulled = pull_server_events(app, host_id).await?;
    let analyzed = analyze_pending_events(app, host_id).await?;
    Ok((pulled, analyzed))
}

/// 手动同步：拉取服务器事件并补做 AI 复盘（界面「立即同步」按钮）。
#[tauri::command]
pub async fn sync_server_guard(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    host_id: String,
) -> Result<String, String> {
    let _ = crate::hosts::load_host(&db, &host_id)?;
    let (pulled, analyzed) = sync_server_host(&app, &host_id).await?;
    Ok(if crate::i18n::is_en() {
        format!("Pulled {pulled} new event(s), completed {analyzed} AI review(s)")
    } else {
        format!("拉取 {pulled} 条新事件，完成 {analyzed} 条 AI 复盘")
    })
}

fn spawn_server_sync_loop(app: AppHandle, host_id: String, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let settings = app.state::<Arc<Db>>().get_guard_settings().unwrap_or_default();
            let still_server = settings.enabled
                && settings
                    .hosts
                    .iter()
                    .any(|h| h.host_id == host_id && h.mode == "server");
            if !still_server {
                break;
            }
            match sync_server_host(&app, &host_id).await {
                Ok((pulled, analyzed)) if pulled > 0 || analyzed > 0 => {
                    log::info!("[guard] server sync {host_id}: pulled {pulled}, analyzed {analyzed}");
                }
                Ok(_) => {}
                Err(e) => log::warn!("[guard] server sync {host_id}: {e}"),
            }
            sleep_or_cancel(SERVER_SYNC_SECS, &cancel).await;
        }
        log::info!("[guard] server sync loop stopped: {host_id}");
    });
}

// ---------- 服务器端守护 ----------

#[tauri::command]
pub async fn install_server_guard(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    host_id: String,
) -> Result<String, String> {
    let settings = db.get_guard_settings().unwrap_or_default();
    if !settings.hosts.iter().any(|h| h.host_id == host_id && h.mode == "server") {
        return Err(crate::i18n::t(
            "该主机未设置为服务器值守模式，请先在值守设置中保存",
            "This host is not in server guard mode; save it in guard settings first",
        )
        .to_string());
    }
    let alert = db.get_alert_settings().unwrap_or_default();
    let url = alert
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            crate::i18n::t(
                "服务器端守护需要先配置 webhook 推送通道（桌面离线时由服务器自己推送）",
                "Server-side guard needs a webhook channel first (the server pushes while the desktop is offline)",
            )
            .to_string()
        })?;
    let secret = alert
        .webhook_secret
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(crate::credentials::get_webhook_secret);
    let script = render_server_script(&settings, &alert, url, secret.as_deref());
    let unit = render_systemd_unit();
    let host = crate::hosts::load_host(&db, &host_id)?;

    let cmd = format!(
        "set -e; \
         if [ \"$(id -u)\" -ne 0 ]; then echo NEED_ROOT; exit 1; fi; \
         command -v systemctl >/dev/null 2>&1 || {{ echo NO_SYSTEMD; exit 1; }}; \
         mkdir -p /usr/local/bin /etc/systemd/system; \
         echo '{}' | base64 -d > /usr/local/bin/buffterm-guard.sh; \
         chmod 700 /usr/local/bin/buffterm-guard.sh; \
         echo '{}' | base64 -d > /etc/systemd/system/buffterm-guard.service; \
         systemctl daemon-reload; \
         systemctl enable buffterm-guard >/dev/null 2>&1; \
         systemctl restart buffterm-guard; \
         sleep 1; \
         systemctl is-active buffterm-guard || true",
        B64.encode(script.as_bytes()),
        B64.encode(unit.as_bytes())
    );
    let russh = app.state::<RusshManager>();
    let out = russh
        .exec(&host, &cmd, Duration::from_secs(90))
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("安装服务器守护失败", "Failed to install the server guard")))?;
    if out.text.contains("NEED_ROOT") {
        return Err(crate::i18n::t(
            "需要 root 权限（当前登录用户不是 root）",
            "Root permission is required (the current user is not root)",
        )
        .to_string());
    }
    if out.text.contains("NO_SYSTEMD") {
        return Err(crate::i18n::t(
            "服务器没有 systemd，无法安装常驻守护",
            "The server has no systemd; the resident guard cannot be installed",
        )
        .to_string());
    }
    Ok(out.text.trim().to_string())
}

#[tauri::command]
pub async fn uninstall_server_guard(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    host_id: String,
) -> Result<String, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    let cmd = "set -e; \
        if [ \"$(id -u)\" -ne 0 ]; then echo NEED_ROOT; exit 1; fi; \
        systemctl disable --now buffterm-guard >/dev/null 2>&1 || true; \
        rm -f /etc/systemd/system/buffterm-guard.service /usr/local/bin/buffterm-guard.sh; \
        systemctl daemon-reload 2>/dev/null || true; \
        echo REMOVED";
    let russh = app.state::<RusshManager>();
    let out = russh
        .exec(&host, cmd, Duration::from_secs(30))
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("卸载服务器守护失败", "Failed to uninstall the server guard")))?;
    if out.text.contains("NEED_ROOT") {
        return Err(crate::i18n::t("需要 root 权限", "Root permission is required").to_string());
    }
    Ok(out.text.trim().to_string())
}

#[tauri::command]
pub async fn server_guard_status(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    host_id: String,
) -> Result<String, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    let cmd = "systemctl is-active buffterm-guard 2>/dev/null || true; echo '---'; journalctl -q -u buffterm-guard -n 5 --no-pager 2>/dev/null | tail -n 5";
    let russh = app.state::<RusshManager>();
    let out = russh
        .exec(&host, cmd, Duration::from_secs(20))
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("查询服务器守护状态失败", "Failed to query the server guard status")))?;
    Ok(out.text.trim().to_string())
}

fn render_systemd_unit() -> String {
    "[Unit]\nDescription=buffTerm Guard (read-only server monitoring)\nAfter=network.target\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/buffterm-guard.sh\nRestart=always\nRestartSec=10\nNice=10\n\n[Install]\nWantedBy=multi-user.target\n".to_string()
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 生成服务器端守护脚本：同一套规则、只读采集、事件直接推送到已配置 webhook。
fn render_server_script(
    settings: &GuardSettings,
    alert: &crate::models::AlertSettings,
    url: &str,
    secret: Option<&str>,
) -> String {
    let kind = match alert.webhook_kind.as_deref() {
        Some("wecom") => "wecom",
        Some("feishu") => "feishu",
        Some("slack") => "slack",
        Some("discord") => "discord",
        Some("googlechat") => "googlechat",
        Some("teams") => "teams",
        Some("generic") => "generic",
        _ => "dingtalk",
    };
    let template = r#"#!/bin/sh
# buffTerm server-side guard (generated). Read-only checks; webhook push on events.
CPU_ON=__CPU_ON__; CPU_PCT=__CPU_PCT__
MEM_ON=__MEM_ON__; MEM_PCT=__MEM_PCT__
DISK_ON=__DISK_ON__; DISK_PCT=__DISK_PCT__
SSH_ON=__SSH_ON__; SSH_FAIL=__SSH_FAIL__
CONNS_ON=__CONNS_ON__; CONNS_PCT=__CONNS__
LOAD_ON=__LOAD_ON__; LOAD_PC=__LOAD_PC__
INODE_ON=__INODE_ON__; INODE_PCT=__INODE_PCT__
OOM_ON=__OOM_ON__
UNITS_ON=__UNITS_ON__
SVC_ON=__SVC_ON__
PROC_LIST=__PROC_LIST__
PORT_LIST=__PORT_LIST__
DURATION=__DURATION__
COOLDOWN=__COOLDOWN__
INTERVAL=5
WEBHOOK_KIND=__KIND__
WEBHOOK_URL=__URL__
WEBHOOK_SECRET=__SECRET__
STATE=/var/lib/buffterm-guard
HOST=$(hostname 2>/dev/null || echo server)
mkdir -p "$STATE" 2>/dev/null || true

jsonq() { sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

push() {
  msg="[buffTerm guard] $HOST | $1 | $3"
  msg=$(printf '%s' "$msg" | jsonq)
  case "$WEBHOOK_KIND" in
    dingtalk)
      u="$WEBHOOK_URL"; sep='?'; case "$u" in *\?*) sep='&';; esac
      if [ -n "$WEBHOOK_SECRET" ] && command -v openssl >/dev/null 2>&1; then
        ts=$(( $(date +%s) * 1000 ))
        sig=$(printf '%s\n%s' "$ts" "$WEBHOOK_SECRET" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" -binary | base64 | sed -e 's/+/%2B/g' -e 's/\//%2F/g' -e 's/=/%3D/g')
        u="$WEBHOOK_URL${sep}timestamp=$ts&sign=$sig"
      fi
      payload="{\"msgtype\":\"text\",\"text\":{\"content\":\"$msg\"}}" ;;
    wecom) payload="{\"msgtype\":\"text\",\"text\":{\"content\":\"$msg\"}}" ;;
    feishu)
      if [ -n "$WEBHOOK_SECRET" ] && command -v openssl >/dev/null 2>&1; then
        ts=$(date +%s)
        sig=$(printf '' | openssl dgst -sha256 -hmac "$(printf '%s\n%s' "$ts" "$WEBHOOK_SECRET")" -binary | base64)
        payload="{\"timestamp\":\"$ts\",\"sign\":\"$sig\",\"msg_type\":\"text\",\"content\":{\"text\":\"$msg\"}}"
      else
        payload="{\"msg_type\":\"text\",\"content\":{\"text\":\"$msg\"}}"
      fi ;;
    slack) payload="{\"text\":\"$msg\"}" ;;
    googlechat) payload="{\"text\":\"$msg\"}" ;;
    discord) payload="{\"content\":\"$msg\"}" ;;
    teams) payload="{\"@type\":\"MessageCard\",\"@context\":\"https://schema.org/extensions\",\"themeColor\":\"D93025\",\"summary\":\"$msg\",\"text\":\"$msg\"}" ;;
    *) payload="{\"source\":\"buffterm-guard\",\"host\":\"$HOST\",\"rule\":\"$1\",\"severity\":\"$2\",\"message\":\"$msg\"}" ;;
  esac
  if command -v curl >/dev/null 2>&1; then
    curl -sS -m 15 -H 'Content-Type: application/json' -d "$payload" "$u" >/dev/null 2>&1 || true
  fi
  echo "$(date '+%F %T') EVENT $1 $2 $3"
}

cooled() {
  f="$STATE/$1.last"
  [ -f "$f" ] || return 0
  last=$(cat "$f" 2>/dev/null || echo 0)
  [ $(( $(date +%s) - last )) -ge $((COOLDOWN * 60)) ]
}
snap_metrics() {
  l=$(awk '{print $1}' /proc/loadavg 2>/dev/null)
  m=$(awk '/^MemTotal:/{t=$2}/^MemAvailable:/{a=$2}END{if(t>0)printf "%d", 100*(t-a)/t}' /proc/meminfo 2>/dev/null)
  d=$(df -P / 2>/dev/null | awk 'NR==2{gsub("%","",$5); print $5}')
  i=$(df -Pi / 2>/dev/null | awk 'NR==2{gsub("%","",$5); print $5}')
  echo "load=$l mem=${m}% disk=${d}% inode=${i}%"
}
fire() {
  if cooled "$1"; then
    date +%s > "$STATE/$1.last" 2>/dev/null
    metrics=$(snap_metrics)
    printf '{"ts":%s,"rule":"%s","severity":"%s","message":"%s","metrics":"%s"}\n' \
      "$(date +%s)" "$1" "$2" "$(printf '%s' "$3" | jsonq)" "$metrics" >> "$STATE/events.jsonl" 2>/dev/null
    tail -n 200 "$STATE/events.jsonl" > "$STATE/events.tmp" 2>/dev/null && mv "$STATE/events.tmp" "$STATE/events.jsonl" 2>/dev/null
    push "$1" "$2" "$3"
  fi
}

cpu_hits=0; mem_hits=0; disk_hits=0; conn_hits=0
prev_total=0; prev_idle=0; tick=0
load_hits=0; inode_hits=0
CORES=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0)
while :; do
  set -- $(head -n 1 /proc/stat 2>/dev/null)
  if [ "$1" = "cpu" ]; then
    shift
    total=0; idle=$4
    for v in "$@"; do total=$(( total + v )); done
    if [ "$prev_total" -gt 0 ]; then
      dt=$(( total - prev_total )); di=$(( idle - prev_idle ))
      if [ "$dt" -gt 0 ]; then cpu=$(( 100 * (dt - di) / dt )); else cpu=0; fi
      if [ "$CPU_ON" = 1 ]; then
        if [ "$cpu" -ge "$CPU_PCT" ]; then
          cpu_hits=$(( cpu_hits + 1 ))
          if [ $(( cpu_hits * INTERVAL )) -ge "$DURATION" ]; then fire cpu high "CPU ${cpu}% >= ${CPU_PCT}%"; cpu_hits=0; fi
        else cpu_hits=0; fi
      fi
    fi
    prev_total=$total; prev_idle=$idle
  fi

  memtotal=$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null)
  memavail=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo 2>/dev/null)
  if [ -n "$memtotal" ] && [ "$memtotal" -gt 0 ] 2>/dev/null; then
    mem=$(( 100 * (memtotal - memavail) / memtotal ))
    if [ "$MEM_ON" = 1 ]; then
      if [ "$mem" -ge "$MEM_PCT" ]; then
        mem_hits=$(( mem_hits + 1 ))
        if [ $(( mem_hits * INTERVAL )) -ge "$DURATION" ]; then fire mem high "MEM ${mem}% >= ${MEM_PCT}%"; mem_hits=0; fi
      else mem_hits=0; fi
    fi
  fi

  if [ "$DISK_ON" = 1 ]; then
    disk=$(df -P / 2>/dev/null | awk 'NR==2{gsub("%","",$5); print $5}')
    if [ -n "$disk" ]; then
      if [ "$disk" -ge "$DISK_PCT" ]; then
        disk_hits=$(( disk_hits + 1 ))
        if [ $(( disk_hits * INTERVAL )) -ge "$DURATION" ]; then fire disk high "DISK / ${disk}% >= ${DISK_PCT}%"; disk_hits=0; fi
      else disk_hits=0; fi
    fi
  fi

  if [ "$LOAD_ON" = 1 ] && [ "$CORES" -gt 0 ] 2>/dev/null; then
    load1=$(awk '{print $1}' /proc/loadavg 2>/dev/null)
    over=$(awk -v l="$load1" -v c="$CORES" -v t="$LOAD_PC" 'BEGIN{print (l/c>=t)?1:0}' 2>/dev/null)
    if [ "$over" = 1 ]; then
      load_hits=$(( load_hits + 1 ))
      if [ $(( load_hits * INTERVAL )) -ge "$DURATION" ]; then fire load medium "LOAD/core ${load1}/${CORES} >= ${LOAD_PC}"; load_hits=0; fi
    else load_hits=0; fi
  fi

  if [ "$CONNS_ON" = 1 ]; then
    conns=$(ss -tn state established 2>/dev/null | awk 'NR>1{c++} END{print c+0}')
    case "$conns" in ''|*[!0-9]*) conns=0;; esac
    if [ "$conns" -ge "$CONNS_PCT" ]; then
      conn_hits=$(( conn_hits + 1 ))
      if [ $(( conn_hits * INTERVAL )) -ge "$DURATION" ]; then fire conns high "CONNS ${conns} >= ${CONNS_PCT}"; conn_hits=0; fi
    else conn_hits=0; fi
  fi

  tick=$(( tick + 1 ))
  if [ $(( tick % 12 )) -eq 0 ]; then
    if [ "$SSH_ON" = 1 ]; then
      fails=$(journalctl -q --since '-1 min' -u ssh -u sshd 2>/dev/null | grep -c 'Failed password' || true)
      case "$fails" in ''|*[!0-9]*) fails=0;; esac
      if [ "$fails" -ge "$SSH_FAIL" ]; then fire ssh_fail high "SSH failures ${fails}/min >= ${SSH_FAIL}"; fi
    fi

    if [ "$INODE_ON" = 1 ]; then
      inode=$(df -Pi / 2>/dev/null | awk 'NR==2{gsub("%","",$5); print $5}')
      case "$inode" in ''|*[!0-9]*) inode=0;; esac
      if [ "$inode" -ge "$INODE_PCT" ]; then
        inode_hits=$(( inode_hits + 1 ))
        if [ $(( inode_hits * 60 )) -ge "$DURATION" ]; then fire inode high "INODE ${inode}% >= ${INODE_PCT}%"; inode_hits=0; fi
      else inode_hits=0; fi
    fi

    if [ "$OOM_ON" = 1 ]; then
      oom=$(journalctl -q -k --since '-2 min' 2>/dev/null | grep -cE 'Out of memory|oom-kill' || true)
      case "$oom" in ''|*[!0-9]*) oom=0;; esac
      if [ "$oom" -gt 0 ]; then fire oom high "OOM kill ${oom} in last 2min"; fi
    fi

    if [ "$UNITS_ON" = 1 ]; then
      failed=$(systemctl --failed --no-legend 2>/dev/null | awk '{print $2}' | sort)
      prevf=$(cat "$STATE/failed.last" 2>/dev/null || true)
      newf=""
      for u in $failed; do echo "$prevf" | grep -qx "$u" || newf="$newf $u"; done
      if [ -n "$newf" ]; then fire units high "new failed units:$newf"; fi
      printf '%s\n' "$failed" > "$STATE/failed.last" 2>/dev/null
    fi

    if [ "$SVC_ON" = 1 ]; then
      for n in $PROC_LIST; do
        f="$STATE/miss.proc.$n"
        if ps -eo comm= 2>/dev/null | grep -qx "$n"; then
          echo 0 > "$f" 2>/dev/null
        else
          m=$(cat "$f" 2>/dev/null || echo 0); m=$(( m + 1 )); echo "$m" > "$f" 2>/dev/null
          if [ "$m" -ge 3 ]; then fire svc_proc high "process not running: $n"; echo 0 > "$f" 2>/dev/null; fi
        fi
      done
      ports=$(ss -tln 2>/dev/null | awk 'NR>1{print $4}' | sed 's/.*[.:]//')
      for p in $PORT_LIST; do
        f="$STATE/miss.port.$p"
        if echo "$ports" | grep -qx "$p"; then
          echo 0 > "$f" 2>/dev/null
        else
          m=$(cat "$f" 2>/dev/null || echo 0); m=$(( m + 1 )); echo "$m" > "$f" 2>/dev/null
          if [ "$m" -ge 3 ]; then fire svc_port high "port not listening: $p"; echo 0 > "$f" 2>/dev/null; fi
        fi
      done
    fi
  fi

  sleep "$INTERVAL"
done
"#;
    let on = |b: bool| if b { "1" } else { "0" };
    template
        .replace("__CPU_ON__", on(settings.cpu_enabled))
        .replace("__CPU_PCT__", &settings.cpu_percent.to_string())
        .replace("__MEM_ON__", on(settings.mem_enabled))
        .replace("__MEM_PCT__", &settings.mem_percent.to_string())
        .replace("__DISK_ON__", on(settings.disk_enabled))
        .replace("__DISK_PCT__", &settings.disk_percent.to_string())
        .replace("__SSH_ON__", on(settings.ssh_fail_enabled))
        .replace("__SSH_FAIL__", &settings.ssh_fail_per_min.to_string())
        .replace("__CONNS_ON__", on(settings.conns_enabled))
        .replace("__CONNS__", &settings.conns_threshold.to_string())
        .replace("__LOAD_ON__", on(settings.load_enabled))
        .replace("__LOAD_PC__", &format!("{:.2}", settings.load_per_core))
        .replace("__INODE_ON__", on(settings.inode_enabled))
        .replace("__INODE_PCT__", &settings.inode_percent.to_string())
        .replace("__OOM_ON__", on(settings.oom_enabled))
        .replace("__UNITS_ON__", on(settings.units_enabled))
        .replace("__SVC_ON__", on(settings.svc_enabled))
        .replace("__PROC_LIST__", &sh_quote(&settings.svc_procs.join(" ")))
        .replace(
            "__PORT_LIST__",
            &sh_quote(
                &settings
                    .svc_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        )
        .replace("__DURATION__", &settings.duration_secs.to_string())
        .replace("__COOLDOWN__", &settings.cooldown_minutes.to_string())
        .replace("__KIND__", &sh_quote(kind))
        .replace("__URL__", &sh_quote(url))
        .replace("__SECRET__", &sh_quote(secret.unwrap_or("")))
}

/// 客户端值守是否活跃：决定关窗是否隐藏到托盘、系统睡眠是否被阻止。
pub fn is_active() -> bool {
    crate::db::global()
        .and_then(|db| db.get_guard_settings().ok())
        .map(|s| s.enabled && s.hosts.iter().any(|h| h.mode == "client"))
        .unwrap_or(false)
}

/// 阻止系统睡眠，让客户端值守整夜有效。
///
/// Windows 的 `SetThreadExecutionState` 是**线程级**状态：在 tokio worker 上设置后，
/// 下一个采样可能换了线程，开关就会失效。因此固定用一个专属线程持有该状态。
/// 只阻止系统睡眠、不要求显示器常亮——值守不需要点亮屏幕。
mod awake {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    static CURRENT: AtomicBool = AtomicBool::new(false);
    /// 「判断 + 发送」必须原子：否则两线程交错时通道消息顺序可能与 CURRENT 终值相反，
    /// 之后又因相等提前返回不再纠正，睡眠抑制会永久卡在错误状态。
    static SET_LOCK: Mutex<()> = Mutex::new(());

    pub fn set(want: bool) {
        let _guard = SET_LOCK.lock().unwrap();
        if CURRENT.load(Ordering::SeqCst) == want {
            return;
        }
        CURRENT.store(want, Ordering::SeqCst);
        imp::apply(want);
    }

    #[cfg(windows)]
    mod imp {
        use std::sync::mpsc::{channel, Sender};
        use std::sync::OnceLock;

        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

        extern "system" {
            fn SetThreadExecutionState(es_flags: u32) -> u32;
        }

        fn sender() -> &'static Sender<bool> {
            static TX: OnceLock<Sender<bool>> = OnceLock::new();
            TX.get_or_init(|| {
                let (tx, rx) = channel::<bool>();
                let spawned = std::thread::Builder::new()
                    .name("guard-awake".into())
                    .spawn(move || {
                        while let Ok(want) = rx.recv() {
                            let flags = if want {
                                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
                            } else {
                                ES_CONTINUOUS
                            };
                            unsafe { SetThreadExecutionState(flags) };
                        }
                        // 通道关闭（进程收尾）时恢复默认，避免把睡眠抑制留给系统
                        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                    });
                if spawned.is_err() {
                    log::error!("[guard] failed to spawn the awake thread");
                }
                tx
            })
        }

        pub fn apply(want: bool) {
            let _ = sender().send(want);
        }
    }

    #[cfg(target_os = "macos")]
    mod imp {
        use std::process::{Child, Command};
        use std::sync::{Mutex, OnceLock};

        fn child() -> &'static Mutex<Option<Child>> {
            static CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
            CHILD.get_or_init(|| Mutex::new(None))
        }

        pub fn apply(want: bool) {
            let mut guard = child().lock().unwrap();
            if want {
                if guard.is_none() {
                    // -i 阻止空闲睡眠；-w 绑定本进程 pid，应用退出后 caffeinate 自行结束
                    *guard = Command::new("caffeinate")
                        .args(["-i", "-w", &std::process::id().to_string()])
                        .spawn()
                        .ok();
                }
            } else if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    mod imp {
        pub fn apply(_want: bool) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AlertSettings, GuardHost};

    fn settings() -> GuardSettings {
        GuardSettings {
            enabled: true,
            hosts: vec![GuardHost { host_id: "h1".into(), mode: "client".into() }],
            ..Default::default()
        }
    }

    fn snap(cpu: f64, mem: f64, disk: f64) -> MonitorSnapshot {
        let mut s = MonitorSnapshot {
            ts: 1,
            cpu_percent: cpu,
            mem: crate::monitor::MemInfo {
                percent: mem,
                ..Default::default()
            },
            ..Default::default()
        };
        s.disks.push(crate::monitor::DiskInfo { mount: "/".into(), percent: disk, ..Default::default() });
        s
    }

    #[test]
    fn duration_must_be_reached_before_firing() {
        let mut state = EvalState::default();
        let s = settings();
        let mut fired = 0;
        // 阈值 90%，持续 60 秒 = 12 个 5 秒采样点
        for _ in 0..11 {
            fired += evaluate(&s, &snap(95.0, 50.0, 50.0), None, &mut state).len();
        }
        assert_eq!(fired, 0);
        fired += evaluate(&s, &snap(95.0, 50.0, 50.0), None, &mut state).len();
        assert_eq!(fired, 1);
        // 触发后计数器归零，不会下一个采样点立刻重复
        assert_eq!(evaluate(&s, &snap(95.0, 50.0, 50.0), None, &mut state).len(), 0);
    }

    #[test]
    fn drop_below_threshold_resets_counter() {
        let mut state = EvalState::default();
        let s = settings();
        for _ in 0..8 {
            evaluate(&s, &snap(95.0, 50.0, 50.0), None, &mut state);
        }
        evaluate(&s, &snap(10.0, 50.0, 50.0), None, &mut state);
        let mut fired = 0;
        for _ in 0..11 {
            fired += evaluate(&s, &snap(95.0, 50.0, 50.0), None, &mut state).len();
        }
        assert_eq!(fired, 0);
    }

    #[test]
    fn security_rules_fire_on_rate_and_connections() {
        let mut state = EvalState::default();
        let s = settings();
        let sec = SecuritySample {
            ssh_fail_per_min: 30,
            established: 400,
            ..Default::default()
        };
        let triggers = evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state);
        // SSH 失败是即时规则；连接数需要持续满足
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].rule, "ssh_fail");
    }

    #[test]
    fn disabled_rules_stay_silent() {
        let mut s = settings();
        s.cpu_enabled = false;
        let mut state = EvalState::default();
        let mut fired = 0;
        for _ in 0..20 {
            fired += evaluate(&s, &snap(99.9, 10.0, 10.0), None, &mut state).len();
        }
        assert_eq!(fired, 0);
    }

    #[test]
    fn security_sample_parses_markers() {
        let sec = parse_security(
            "SSHFAIL 42\nESTAB 133\nOOM 2\nINODE 88\nCORES 4\nPORTS 22 443 \nFAILEDUNIT nginx.service\nPROC nginx 0\nPROC sshd 1\n",
        )
        .expect("parse");
        assert_eq!(sec.ssh_fail_per_min, 42);
        assert_eq!(sec.established, 133);
        assert_eq!(sec.oom_events, 2);
        assert_eq!(sec.inode_percent, 88);
        assert_eq!(sec.cores, 4);
        assert_eq!(sec.ports, vec![22, 443]);
        assert_eq!(sec.failed_units, vec!["nginx.service"]);
        assert_eq!(sec.proc_missing, vec!["nginx"]);
        assert!(parse_security("garbage").is_none());
    }

    #[test]
    fn oom_and_new_failed_units_fire_once() {
        let mut state = EvalState::default();
        let s = settings();
        let sec = SecuritySample {
            oom_events: 2,
            failed_units: vec!["nginx.service".into()],
            ..Default::default()
        };
        let t = evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state);
        assert!(t.iter().any(|x| x.rule == "oom"));
        assert!(t.iter().any(|x| x.rule == "units"));
        // 同一失败单元不重复告警
        let t2 = evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state);
        assert!(!t2.iter().any(|x| x.rule == "units"));
        // 单元恢复后再次失败 → 重新告警
        let recovered = SecuritySample::default();
        evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&recovered), &mut state);
        let t3 = evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state);
        assert!(t3.iter().any(|x| x.rule == "units"));
    }

    #[test]
    fn service_liveness_requires_three_consecutive_misses() {
        let mut s = settings();
        s.svc_procs = vec!["nginx".into()];
        s.svc_ports = vec![443];
        let mut state = EvalState::default();
        let sec = SecuritySample {
            proc_missing: vec!["nginx".into()],
            ports: vec![22],
            ..Default::default()
        };
        assert!(evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state).is_empty());
        assert!(evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state).is_empty());
        let t = evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state);
        assert!(t.iter().any(|x| x.rule == "svc_proc"));
        assert!(t.iter().any(|x| x.rule == "svc_port"));
        // 恢复一次后计数清零
        let ok = SecuritySample {
            ports: vec![443],
            ..Default::default()
        };
        evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&ok), &mut state);
        assert!(evaluate(&s, &snap(10.0, 10.0, 10.0), Some(&sec), &mut state).is_empty());
    }

    #[test]
    fn load_per_core_uses_cached_cores() {
        let mut state = EvalState::default();
        let s = settings();
        let sec = SecuritySample {
            cores: 2,
            ..Default::default()
        };
        let mut sn = snap(10.0, 10.0, 10.0);
        // 9.0 / 2 核 = 4.5，超过默认阈值 4.0
        sn.load = "9.00 8.00 7.00".into();
        let mut fired = 0;
        for _ in 0..12 {
            fired += evaluate(&s, &sn, Some(&sec), &mut state).len();
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn server_event_lines_parse_and_ids_are_stable() {
        let line = r#"{"ts":1700000000,"rule":"cpu","severity":"high","message":"CPU 100%","metrics":"load=9 mem=80% disk=70% inode=20%"}"#;
        let a = parse_server_event_line(line, "h1").expect("parse");
        let b = parse_server_event_line(line, "h1").expect("parse");
        assert_eq!(a.id, b.id);
        assert_eq!(a.rule, "cpu");
        assert_eq!(a.event_ts, 1_700_000_000);
        assert!(a.metrics.contains("load=9"));
        assert!(parse_server_event_line("not json", "h1").is_none());
        assert_ne!(a.id, parse_server_event_line(line, "h2").unwrap().id);
    }

    #[test]
    fn server_script_embeds_thresholds_and_quotes_secrets() {
        let mut s = settings();
        s.svc_procs = vec!["sshd".into(), "nginx".into()];
        s.svc_ports = vec![22, 443];
        let alert = AlertSettings { webhook_kind: Some("feishu".into()), ..Default::default() };
        let script = render_server_script(&s, &alert, "https://open.feishu.cn/x?k=1", Some("it's-secret"));
        assert!(script.contains("CPU_PCT=90"));
        assert!(script.contains("LOAD_PC=4.00"));
        assert!(script.contains("INODE_PCT=90"));
        assert!(script.contains("PROC_LIST='sshd nginx'"));
        assert!(script.contains("PORT_LIST='22 443'"));
        // 事件回传：桌面在线后据此补做 AI 复盘
        assert!(script.contains("events.jsonl"));
        assert!(script.contains("snap_metrics"));
        assert!(script.contains("WEBHOOK_KIND='feishu'"));
        assert!(script.contains("WEBHOOK_URL='https://open.feishu.cn/x?k=1'"));
        // 单引号被正确转义，脚本不会因密钥截断
        assert!(script.contains("WEBHOOK_SECRET='it'\\''s-secret'"));
        assert!(script.starts_with("#!/bin/sh"));
    }
}