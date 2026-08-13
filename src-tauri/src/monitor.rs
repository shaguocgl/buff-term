use crate::models::Host;
use crate::russh::RusshManager;
use serde::Serialize;
use std::time::Duration;
use tauri::State;

#[derive(Serialize, Default)]
pub struct DiskInfo {
    pub mount: String,
    pub fs: String,
    pub total: String,
    pub used: String,
    pub percent: f64,
}

#[derive(Serialize, Default)]
pub struct MemInfo {
    pub total_mb: u64,
    pub used_mb: u64,
    pub percent: f64,
}

#[derive(Serialize, Default)]
pub struct TopProc {
    pub user: String,
    pub cpu: String,
    pub mem: String,
    pub cmd: String,
}

#[derive(Serialize, Default)]
pub struct MonitorSnapshot {
    pub ts: u64,
    pub host_label: String,
    pub load: String,
    pub cpu_percent: f64,
    pub mem: MemInfo,
    pub disks: Vec<DiskInfo>,
    pub top: Vec<TopProc>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 采集 Linux 服务器的资源快照（CPU / 内存 / 磁盘 / 负载 / TOP 进程）
/// 复用 russh 连接池，避免每次采集都新建系统 ssh 进程
#[tauri::command]
pub async fn monitor_snapshot(
    russh: State<'_, RusshManager>,
    host: Host,
) -> Result<MonitorSnapshot, String> {
    collect_russh(&host, &russh).await
}

/// 通过 russh 连接池采集（复用 AI / MCP 同一 SSH 通道）
pub async fn collect_russh(
    host: &Host,
    russh: &RusshManager,
) -> Result<MonitorSnapshot, String> {
    let out = russh
        .exec(host, MONITOR_SCRIPT, Duration::from_secs(25))
        .await?;
    parse(&out.text, host)
}

const MONITOR_SCRIPT: &str = r#"
echo "BEGIN"
echo "LOAD $(cat /proc/loadavg 2>/dev/null | cut -d' ' -f1-3)"
p1=$(grep '^cpu ' /proc/stat)
sleep 0.3
p2=$(grep '^cpu ' /proc/stat)
CPU=$(awk -v a="$p1" -v b="$p2" 'BEGIN { split(a,A); split(b,B); u1=A[2]+A[3]+A[4]; u2=B[2]+B[3]+B[4]; d1=u1+A[5]; d2=u2+B[5]; d=d2-d1; if(d<=0){print 0; exit} printf "%.1f\n", (u2-u1)/d*100 }')
echo "CPU $CPU"
echo "MEM $(free -m 2>/dev/null | awk '/Mem:/{print $2, $3, $7}')"
echo "DISK"
df -hP 2>/dev/null | awk 'NR>1 {print $6 "|" $1 "|" $2 "|" $3}'
echo "TOP"
ps -eo user,%cpu,%mem,args --sort=-%cpu 2>/dev/null | head -8
echo "END"
"#;

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
        if line == "TOP" {
            section = "top".to_string();
            continue;
        }
        match section.as_str() {
            "disk" => {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() == 4 {
                    let mut d = DiskInfo {
                        mount: parts[0].to_string(),
                        fs: parts[1].to_string(),
                        total: parts[2].to_string(),
                        used: parts[3].to_string(),
                        percent: 0.0,
                    };
                    d.percent = percent_from_df(&d.total, &d.used);
                    snap.disks.push(d);
                }
            }
            "top" => {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    snap.top.push(TopProc {
                        user: parts[0].to_string(),
                        cpu: parts[1].to_string(),
                        mem: parts[2].to_string(),
                        cmd: parts[3..].join(" "),
                    });
                }
            }
            _ => {
                if let Some(v) = line.strip_prefix("LOAD ") {
                    snap.load = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("CPU ") {
                    snap.cpu_percent = v.trim().parse().unwrap_or(0.0);
                } else if let Some(v) = line.strip_prefix("MEM ") {
                    let nums: Vec<&str> = v.split_whitespace().collect();
                    // free -m 输出：total used ... available；used 采用 total - available（htop 口径）
                    if nums.len() >= 3 {
                        if let (Ok(t), Ok(a)) =
                            (nums[0].parse::<u64>(), nums[2].parse::<u64>())
                        {
                            let used = t.saturating_sub(a);
                            snap.mem = MemInfo {
                                total_mb: t,
                                used_mb: used,
                                percent: if t > 0 {
                                    (used as f64 / t as f64) * 100.0
                                } else {
                                    0.0
                                },
                            };
                        }
                    }
                }
            }
        }
    }
    if snap.mem.total_mb == 0 && snap.cpu_percent == 0.0 && snap.disks.is_empty() {
        return Err("无法解析监控数据（服务器可能不是 Linux 或缺少 /proc）".to_string());
    }
    Ok(snap)
}

/// 从 df -hP 的带单位大小计算百分比
fn percent_from_df(total: &str, used: &str) -> f64 {
    let to_gb = |s: &str| -> Option<f64> {
        let s = s.trim();
        let (num, unit) = s.split_at(s.len().saturating_sub(1));
        let n: f64 = num.trim().parse().ok()?;
        match unit {
            "G" => Some(n),
            "T" => Some(n * 1024.0),
            "M" => Some(n / 1024.0),
            "K" => Some(n / 1024.0 / 1024.0),
            _ => Some(n),
        }
    };
    match (to_gb(total), to_gb(used)) {
        (Some(t), Some(u)) if t > 0.0 => (u / t) * 100.0,
        _ => 0.0,
    }
}
