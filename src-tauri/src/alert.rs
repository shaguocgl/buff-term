use crate::db::Db;
use crate::models::{AlertRule, Host};
use crate::monitor;
use crate::session::SessionManager;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

#[derive(Debug, Deserialize)]
pub struct AlertInput {
    pub metric: String,
    pub operator: String,
    pub threshold: f64,
    pub channel: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default = "default_cooldown")]
    pub cooldown_min: u64,
    #[serde(default)]
    pub enabled: bool,
}

fn default_cooldown() -> u64 {
    10
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tauri::command]
pub fn list_alerts(db: State<'_, Db>) -> Result<Vec<AlertRule>, String> {
    db.list_alerts(false)
        .map_err(|e| format!("读取告警规则失败: {e}"))
}

#[tauri::command]
pub fn save_alert(
    db: State<'_, Db>,
    input: AlertInput,
    id: Option<String>,
) -> Result<AlertRule, String> {
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let exists = db
        .list_alerts(false)
        .map_err(|e| format!("读取告警规则失败: {e}"))?
        .iter()
        .any(|r| r.id == id);
    let rule = AlertRule {
        id,
        metric: input.metric,
        operator: input.operator,
        threshold: input.threshold,
        channel: input.channel,
        target: input.target,
        cooldown_min: input.cooldown_min,
        enabled: input.enabled,
        created_at: now(),
    };
    if exists {
        db.update_alert(&rule)
            .map_err(|e| format!("更新告警规则失败: {e}"))?;
    } else {
        db.insert_alert(&rule)
            .map_err(|e| format!("保存告警规则失败: {e}"))?;
    }
    Ok(rule)
}

#[tauri::command]
pub fn delete_alert(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_alert(&id)
        .map_err(|e| format!("删除告警规则失败: {e}"))
}

/// 后台定时评估告警：每 30 秒对已连接主机采集一次快照，命中规则即通知
pub fn spawn_alert_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last_fired: HashMap<(String, String), u64> = HashMap::new();
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = evaluate_once(&app, &mut last_fired).await;
        }
    });
}

async fn evaluate_once(
    app: &AppHandle,
    last_fired: &mut HashMap<(String, String), u64>,
) -> Result<(), String> {
    let db = app.state::<Db>();
    let sessions = app.state::<SessionManager>();
    let rules = db
        .list_alerts(true)
        .map_err(|e| format!("读取告警规则失败: {e}"))?;
    if rules.is_empty() {
        return Ok(());
    }
    let now = now();
    for host in sessions.hosts() {
        let snap = match monitor::collect(&host) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut metrics = HashMap::new();
        metrics.insert("cpu".to_string(), snap.cpu_percent);
        metrics.insert("mem".to_string(), snap.mem.percent);
        metrics.insert(
            "disk".to_string(),
            snap.disks.iter().map(|d| d.percent).fold(0.0, f64::max),
        );
        if let Some(v) = snap
            .load
            .split_whitespace()
            .next()
            .and_then(|x| x.parse::<f64>().ok())
        {
            metrics.insert("load".to_string(), v);
        }

        for rule in &rules {
            let Some(value) = metrics.get(&rule.metric) else {
                continue;
            };
            let fired = match rule.operator.as_str() {
                ">" => value > &rule.threshold,
                "<" => value < &rule.threshold,
                _ => false,
            };
            if !fired {
                continue;
            }
            let key = (host.id.clone(), rule.id.clone());
            let last = last_fired.get(&key).copied().unwrap_or(0);
            if now.saturating_sub(last) < rule.cooldown_min * 60 {
                continue;
            }
            last_fired.insert(key, now);
            fire(app, rule, &host, *value).await;
        }
    }
    Ok(())
}

async fn fire(app: &AppHandle, rule: &AlertRule, host: &Host, value: f64) {
    let message = format!(
        "{}：{} 达到 {:.1}（阈值 {} {}）",
        host.name, rule.metric, value, rule.operator, rule.threshold
    );
    let webhook = if rule.channel == "webhook" {
        rule.target.as_deref()
    } else {
        None
    };
    notify(app, "KeyWisp 告警", &message, webhook).await;
}

/// 发送桌面通知或 Webhook（告警与巡检共用）
pub async fn notify(app: &AppHandle, title: &str, body: &str, webhook: Option<&str>) {
    if let Some(url) = webhook {
        let url = url.to_string();
        let title = title.to_string();
        let body = body.to_string();
        let _ = tokio::spawn(async move {
            let payload = serde_json::json!({
                "event": "alert",
                "title": title,
                "body": body,
                "ts": now(),
            });
            let client = reqwest::Client::new();
            let _ = client
                .post(&url)
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
        });
    } else {
        use tauri_plugin_notification::NotificationExt;
        let _ = app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show();
    }
}
