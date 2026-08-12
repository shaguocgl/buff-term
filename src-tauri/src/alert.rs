use crate::db::Db;
use crate::models::{AlertRule, AlertSettings};
use crate::monitor;
use crate::session::SessionManager;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
pub struct AlertInput {
    pub metric: String,
    pub operator: String,
    pub threshold: f64,
    pub channel: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default = "default_cooldown")]
    pub cooldown_min: u64,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone)]
pub struct NotifyChannel {
    pub channel: String,
    pub target: Option<String>,
    pub secret: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
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
        secret: input.secret,
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

#[tauri::command]
pub fn get_alert_settings(db: State<'_, Db>) -> Result<AlertSettings, String> {
    db.get_alert_settings()
        .map_err(|e| format!("读取通知设置失败: {e}"))
}

#[tauri::command]
pub fn save_alert_settings(
    db: State<'_, Db>,
    settings: AlertSettings,
) -> Result<(), String> {
    db.save_alert_settings(&settings)
        .map_err(|e| format!("保存通知设置失败: {e}"))
}

/// 测试 SMTP 邮件发送（用传入的设置，未保存也可测）
#[tauri::command]
pub async fn test_alert_settings(settings: AlertSettings) -> Result<TestResult, String> {
    match send_email(&settings, "KeyWisp 测试通知", "这是一条来自 KeyWisp Agent 的测试邮件。").await {
        Ok(()) => Ok(TestResult { ok: true, message: "邮件发送成功".to_string() }),
        Err(e) => Ok(TestResult { ok: false, message: e }),
    }
}

/// 测试指定渠道（钉钉 / 飞书 / 通用 Webhook；邮件使用已保存的 SMTP 设置）
#[tauri::command]
pub async fn test_alert_channel(
    app: AppHandle,
    channel: String,
    target: Option<String>,
    secret: Option<String>,
) -> Result<TestResult, String> {
    let ch = NotifyChannel { channel, target, secret };
    let result = match ch.channel.as_str() {
        "dingtalk" => send_dingtalk(&ch, "KeyWisp 测试通知", "这是一条来自 KeyWisp Agent 的测试消息。").await,
        "feishu" => send_feishu(&ch, "KeyWisp 测试通知", "这是一条来自 KeyWisp Agent 的测试消息。").await,
        "webhook" => send_webhook(&ch, "KeyWisp 测试通知", "这是一条来自 KeyWisp Agent 的测试消息。").await,
        "email" => {
            let db = app.state::<Db>();
            let settings = db
                .get_alert_settings()
                .map_err(|e| format!("读取通知设置失败: {e}"))?;
            send_email(&settings, "KeyWisp 测试通知", "这是一条来自 KeyWisp Agent 的测试邮件。").await
        }
        _ => Err("不支持的渠道".to_string()),
    };
    Ok(match result {
        Ok(()) => TestResult { ok: true, message: "发送成功".to_string() },
        Err(e) => TestResult { ok: false, message: e },
    })
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
            let channel = NotifyChannel {
                channel: rule.channel.clone(),
                target: rule.target.clone(),
                secret: rule.secret.clone(),
            };
            let message = format!(
                "{}：{} 达到 {:.1}（阈值 {} {}）",
                host.name, rule.metric, value, rule.operator, rule.threshold
            );
            notify(app, "KeyWisp 告警", &message, Some(&channel)).await;
        }
    }
    Ok(())
}

/// 巡检通知：优先走第一条非桌面渠道，否则桌面通知
pub async fn notify_channel_for_inspection(app: &AppHandle, title: &str, body: &str) {
    let db = app.state::<Db>();
    if let Ok(rules) = db.list_alerts(true) {
        if let Some(rule) = rules.iter().find(|r| r.channel != "notification") {
            let ch = NotifyChannel {
                channel: rule.channel.clone(),
                target: rule.target.clone(),
                secret: rule.secret.clone(),
            };
            notify(app, title, body, Some(&ch)).await;
            return;
        }
    }
    notify(app, title, body, None).await;
}

pub async fn notify(app: &AppHandle, title: &str, body: &str, channel: Option<&NotifyChannel>) {
    match channel {
        Some(c) => match c.channel.as_str() {
            "email" => {
                let db = app.state::<Db>();
                if let Ok(settings) = db.get_alert_settings() {
                    let _ = send_email(&settings, title, body).await;
                }
            }
            "dingtalk" => {
                let _ = send_dingtalk(c, title, body).await;
            }
            "feishu" => {
                let _ = send_feishu(c, title, body).await;
            }
            "webhook" => {
                let _ = send_webhook(c, title, body).await;
            }
            _ => desktop(app, title, body),
        },
        None => desktop(app, title, body),
    }
}

fn desktop(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

async fn send_email(
    settings: &AlertSettings,
    title: &str,
    body: &str,
) -> Result<(), String> {
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    use lettre::{Message, SmtpTransport, Transport};

    let host = settings.smtp_host.clone().ok_or_else(|| "未配置 SMTP 服务器".to_string())?;
    let port = settings.smtp_port.unwrap_or(587);
    let username = settings
        .smtp_username
        .clone()
        .ok_or_else(|| "未配置 SMTP 用户名".to_string())?;
    let password = settings.smtp_password.clone().unwrap_or_default();
    let from = settings
        .smtp_from
        .clone()
        .ok_or_else(|| "未配置发件人".to_string())?;
    let to = settings
        .smtp_to
        .clone()
        .ok_or_else(|| "未配置收件人".to_string())?;

    let from_mb: Mailbox = from.parse().map_err(|e| format!("发件人地址无效: {e}"))?;
    let mut builder = Message::builder().from(from_mb).subject(title);
    for addr in to.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()) {
        let mb: Mailbox = addr.parse().map_err(|e| format!("收件人地址无效: {e}"))?;
        builder = builder.to(mb);
    }
    let email = builder
        .body(body.to_string())
        .map_err(|e| format!("构建邮件失败: {e}"))?;

    let creds = Credentials::new(username, password);
    let tls_mode = settings.smtp_tls.as_deref().unwrap_or("starttls");
    let smtp_builder = SmtpTransport::builder_dangerous(&host)
        .port(port)
        .credentials(creds);
    let transport = match tls_mode {
        "ssl" => {
            let params = TlsParameters::new(host).map_err(|e| format!("TLS 参数失败: {e}"))?;
            smtp_builder.tls(Tls::Wrapper(params)).build()
        }
        "none" => smtp_builder.build(),
        _ => {
            let params = TlsParameters::new(host).map_err(|e| format!("TLS 参数失败: {e}"))?;
            smtp_builder.tls(Tls::Required(params)).build()
        }
    };
    transport
        .send(&email)
        .map_err(|e| format!("邮件发送失败: {e}"))?;
    Ok(())
}

async fn send_dingtalk(
    channel: &NotifyChannel,
    title: &str,
    body: &str,
) -> Result<(), String> {
    let url = channel
        .target
        .clone()
        .ok_or_else(|| "未配置钉钉机器人 Webhook 地址".to_string())?;
    let mut url = url;
    let mut payload = serde_json::json!({
        "msgtype": "text",
        "text": { "content": format!("{title}\n{body}") }
    });
    if let Some(secret) = &channel.secret {
        if !secret.trim().is_empty() {
            let ts = now();
            let string_to_sign = format!("{ts}\n{secret}");
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
                .map_err(|e| format!("签名失败: {e}"))?;
            mac.update(string_to_sign.as_bytes());
            let sig = base64::engine::general_purpose::STANDARD
                .encode(mac.finalize().into_bytes());
            let sign = percent_encoding::utf8_percent_encode(
                &sig,
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string();
            let sep = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{sep}timestamp={ts}&sign={sign}");
            payload["timestamp"] = serde_json::json!(ts);
            payload["sign"] = serde_json::json!(sign);
        }
    }
    post_json(&url, &payload).await
}

async fn send_feishu(
    channel: &NotifyChannel,
    title: &str,
    body: &str,
) -> Result<(), String> {
    let url = channel
        .target
        .clone()
        .ok_or_else(|| "未配置飞书机器人 Webhook 地址".to_string())?;
    let mut payload = serde_json::json!({
        "msg_type": "text",
        "content": { "text": format!("{title}\n{body}") }
    });
    if let Some(secret) = &channel.secret {
        if !secret.trim().is_empty() {
            let ts = now();
            let string_to_sign = format!("{ts}\n{secret}");
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
                .map_err(|e| format!("签名失败: {e}"))?;
            mac.update(string_to_sign.as_bytes());
            let sign = base64::engine::general_purpose::STANDARD
                .encode(mac.finalize().into_bytes());
            payload["timestamp"] = serde_json::json!(ts.to_string());
            payload["sign"] = serde_json::json!(sign);
        }
    }
    post_json(&url, &payload).await
}

async fn send_webhook(
    channel: &NotifyChannel,
    title: &str,
    body: &str,
) -> Result<(), String> {
    let url = channel
        .target
        .clone()
        .ok_or_else(|| "未配置 Webhook 地址".to_string())?;
    let payload = serde_json::json!({
        "event": "alert",
        "title": title,
        "body": body,
        "ts": now(),
    });
    post_json(&url, &payload).await
}

async fn post_json(url: &str, payload: &serde_json::Value) -> Result<(), String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .json(payload)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}: {}", status.as_u16(), text.chars().take(200).collect::<String>()))
    }
}
