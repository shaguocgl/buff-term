//! 通知配置与发送：邮件（SMTP）与 webhook 机器人（钉钉 / 企微 / 飞书 / 通用）两条通道。
//! 两条通道相互独立，任一失败不影响另一条，也不影响巡检本身的成功状态。

use std::sync::Arc;
use crate::db::Db;
use crate::models::{AlertSettings, InspectionReport};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::Serialize;
use tauri::State;

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

#[tauri::command]
pub fn get_alert_settings(db: State<'_, Arc<Db>>) -> Result<AlertSettings, String> {
    let mut settings = db
        .get_alert_settings()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取邮件设置失败", "Failed to read email settings")))?;
    // 密码不回传渲染进程：只告知是否已保存，前端输入框保持空白；
    // 需要清除时调用 clear_alert_password。
    settings.smtp_password_set = crate::credentials::get_smtp_password().is_some();
    settings.smtp_password = None;
    settings.webhook_secret_set = crate::credentials::get_webhook_secret().is_some();
    settings.webhook_secret = None;
    Ok(settings)
}

/// 清除已保存的 SMTP 密码（不影响其他 SMTP 设置）。
#[tauri::command]
pub fn clear_alert_password(db: State<'_, Arc<Db>>) -> Result<(), String> {
    crate::credentials::delete_smtp_password();
    let mut settings = db
        .get_alert_settings()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取邮件设置失败", "Failed to read email settings")))?;
    settings.smtp_password = None;
    settings.smtp_password_set = false;
    db.save_alert_settings(&settings)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存邮件设置失败", "Failed to save email settings")))
}

/// 清除已保存的 webhook 加签密钥（不影响其他 webhook 设置）。
#[tauri::command]
pub fn clear_webhook_secret(db: State<'_, Arc<Db>>) -> Result<(), String> {
    crate::credentials::delete_webhook_secret();
    let mut settings = db
        .get_alert_settings()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取通知设置失败", "Failed to read notification settings")))?;
    settings.webhook_secret = None;
    settings.webhook_secret_set = false;
    db.save_alert_settings(&settings)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存通知设置失败", "Failed to save notification settings")))
}

#[tauri::command]
pub fn save_alert_settings(db: State<'_, Arc<Db>>, settings: AlertSettings) -> Result<(), String> {
    // 密码与加签密钥单独加密存储，不随 JSON 明文落库
    if let Some(password) = settings.smtp_password.clone() {
        if !password.trim().is_empty() {
            crate::credentials::save_smtp_password(&password)?;
        }
    }
    if let Some(secret) = settings.webhook_secret.clone() {
        if !secret.trim().is_empty() {
            crate::credentials::save_webhook_secret(&secret)?;
        }
    }
    let mut to_save = settings;
    to_save.smtp_password = None;
    to_save.webhook_secret = None;
    // *_set 只由读取接口按钥匙串事实重算，落库一律写 false，避免前端传值污染 JSON
    to_save.smtp_password_set = false;
    to_save.webhook_secret_set = false;
    db.save_alert_settings(&to_save)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存通知设置失败", "Failed to save notification settings")))
}

/// 测试已配置的通知通道（用传入的设置，未保存也可测）。
/// 只测已填写的通道：只配了 webhook 的用户不会因为 SMTP 为空而收到失败。
#[tauri::command]
pub async fn test_alert_settings(settings: AlertSettings) -> Result<TestResult, String> {
    let smtp_ready = non_empty(settings.smtp_host.as_deref());
    let webhook_ready = non_empty(settings.webhook_url.as_deref());
    if !smtp_ready && !webhook_ready {
        return Ok(TestResult {
            ok: false,
            message: crate::i18n::t(
                "尚未配置任何通知通道，请先填写邮件或 webhook",
                "No notification channel is configured yet; fill in email or webhook first",
            )
            .to_string(),
        });
    }
    let title = crate::i18n::t("buffTerm 通知配置测试", "buffTerm notification settings test");
    let mut lines = Vec::new();
    let mut ok = true;
    if smtp_ready {
        match send_html_email(&settings, title, &test_email_html()).await {
            Ok(()) => lines.push(
                crate::i18n::t("邮件：发送成功，请检查收件箱", "Email: sent — check your inbox")
                    .to_string(),
            ),
            Err(e) => {
                ok = false;
                lines.push(format!("{}: {e}", crate::i18n::t("邮件", "Email")));
            }
        }
    }
    if webhook_ready {
        let msg = WebhookMessage {
            title,
            text: &test_webhook_text(),
            risk: "low",
            host_label: crate::i18n::t("（测试消息）", "(test message)"),
        };
        match send_webhook(&settings, &msg).await {
            Ok(()) => lines.push(
                crate::i18n::t("webhook：推送成功，请检查群消息", "Webhook: pushed — check your group chat")
                    .to_string(),
            ),
            Err(e) => {
                ok = false;
                lines.push(format!("webhook: {e}"));
            }
        }
    }
    Ok(TestResult { ok, message: lines.join("\n") })
}

fn non_empty(value: Option<&str>) -> bool {
    value.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

/// 发送 HTML 邮件；巡检报告等需要富文本展示时使用。
pub(crate) async fn send_html_email(
    settings: &AlertSettings,
    title: &str,
    html: &str,
) -> Result<(), String> {
    use lettre::message::header::ContentType;
    use lettre::message::Mailbox;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    use lettre::{Message, SmtpTransport, Transport};

    let host = settings
        .smtp_host
        .clone()
        .ok_or_else(|| crate::i18n::t("未配置 SMTP 服务器", "SMTP server is not configured").to_string())?;
    let port = settings.smtp_port.unwrap_or(587);
    let username = settings
        .smtp_username
        .clone()
        .ok_or_else(|| crate::i18n::t("未配置 SMTP 用户名", "SMTP username is not configured").to_string())?;
    let password = settings
        .smtp_password
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(crate::credentials::get_smtp_password)
        .unwrap_or_default();
    let from = settings
        .smtp_from
        .clone()
        .ok_or_else(|| crate::i18n::t("未配置发件人", "Sender is not configured").to_string())?;
    let to = settings
        .smtp_to
        .clone()
        .ok_or_else(|| crate::i18n::t("未配置收件人", "Recipient is not configured").to_string())?;

    let from_mb: Mailbox = from.parse().map_err(|e| format!("{}: {e}", crate::i18n::t("发件人地址无效", "Invalid sender address")))?;
    let mut builder = Message::builder()
        .from(from_mb)
        .subject(title)
        .header(ContentType::TEXT_HTML);
    for addr in to
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mb: Mailbox = addr.parse().map_err(|e| format!("{}: {e}", crate::i18n::t("收件人地址无效", "Invalid recipient address")))?;
        builder = builder.to(mb);
    }
    let email = builder
        .body(html.to_string())
        .map_err(|e| format!("{}: {e}", crate::i18n::t("构建邮件失败", "Failed to build the email")))?;

    let creds = Credentials::new(username, password);
    let tls_mode = settings.smtp_tls.as_deref().unwrap_or("starttls");
    let smtp_builder = SmtpTransport::builder_dangerous(&host)
        .port(port)
        .credentials(creds);
    let transport = match tls_mode {
        "ssl" => {
            let params = TlsParameters::new(host).map_err(|e| format!("{}: {e}", crate::i18n::t("TLS 参数失败", "TLS parameter error")))?;
            smtp_builder.tls(Tls::Wrapper(params)).build()
        }
        "none" => smtp_builder.build(),
        _ => {
            let params = TlsParameters::new(host).map_err(|e| format!("{}: {e}", crate::i18n::t("TLS 参数失败", "TLS parameter error")))?;
            smtp_builder.tls(Tls::Required(params)).build()
        }
    };
    // lettre 的 send 是同步阻塞调用（网络 IO），放到 spawn_blocking 避免卡住 tokio worker
    tokio::task::spawn_blocking(move || transport.send(&email))
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("邮件发送线程异常", "Email sending thread failed")))?
        .map_err(|e| format!("{}: {e}", crate::i18n::t("HTML 邮件发送失败", "Failed to send the HTML email")))?;
    Ok(())
}

// ---------- webhook 机器人 ----------

/// 推送给 webhook 的内容。IM 通道只发摘要（企微 markdown 上限 4096 字节，
/// 中文按 3 字节算，完整报告很容易超限），完整报告始终走邮件与巡检面板。
pub(crate) struct WebhookMessage<'a> {
    pub title: &'a str,
    pub text: &'a str,
    pub risk: &'a str,
    pub host_label: &'a str,
}

fn webhook_kind(settings: &AlertSettings) -> &str {
    match settings.webhook_kind.as_deref() {
        Some("wecom") => "wecom",
        Some("feishu") => "feishu",
        Some("slack") => "slack",
        Some("discord") => "discord",
        Some("googlechat") => "googlechat",
        Some("teams") => "teams",
        Some("generic") => "generic",
        _ => "dingtalk",
    }
}

/// 钉钉加签：待签名串是 `{timestamp}\n{secret}`，HMAC 密钥同样是 secret，
/// 结果 base64 后必须 URL 编码（base64 里的 `+ / =` 会破坏 query）。
fn dingtalk_signature(secret: &str, timestamp_ms: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let string_to_sign = format!("{timestamp_ms}\n{secret}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(string_to_sign.as_bytes());
    let sign = B64.encode(mac.finalize().into_bytes());
    percent_encoding::utf8_percent_encode(&sign, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// 飞书加签与钉钉完全不同：待签名串 `{timestamp}\n{secret}` 本身当 HMAC **密钥**，
/// 对**空消息**做 SHA256，结果 base64 放进请求体（timestamp 是秒，不是毫秒）。
fn feishu_signature(secret: &str, timestamp_secs: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let string_to_sign = format!("{timestamp_secs}\n{}", secret.trim());
    let mut mac = Hmac::<Sha256>::new_from_slice(string_to_sign.as_bytes())
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(b"");
    B64.encode(mac.finalize().into_bytes())
}

/// 拼出最终请求 URL。只有钉钉且填了密钥时才追加 timestamp/sign。
fn signed_webhook_url(kind: &str, url: &str, secret: Option<&str>, now_ms: u64) -> String {
    let url = url.trim();
    let Some(secret) = secret.filter(|s| !s.trim().is_empty()) else {
        return url.to_string();
    };
    if kind != "dingtalk" {
        return url.to_string();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!(
        "{url}{sep}timestamp={now_ms}&sign={}",
        dingtalk_signature(secret.trim(), now_ms)
    )
}

fn webhook_payload(
    kind: &str,
    msg: &WebhookMessage<'_>,
    feishu_sign: Option<(u64, &str)>,
) -> serde_json::Value {
    match kind {
        "wecom" => serde_json::json!({
            "msgtype": "markdown",
            "markdown": { "content": format!("{}\n\n{}", msg.title, msg.text) }
        }),
        "feishu" => {
            let mut body = serde_json::json!({
                "msg_type": "interactive",
                "card": {
                    "config": { "wide_screen_mode": true },
                    "header": {
                        "title": { "tag": "plain_text", "content": msg.title },
                        "template": match msg.risk {
                            "high" => "red",
                            "medium" => "orange",
                            "low" => "green",
                            _ => "grey",
                        }
                    },
                    "elements": [{ "tag": "markdown", "content": msg.text }]
                }
            });
            // 签名在顶层，且 timestamp 以字符串提交
            if let Some((ts, sign)) = feishu_sign {
                body["timestamp"] = serde_json::Value::String(ts.to_string());
                body["sign"] = serde_json::Value::String(sign.to_string());
            }
            body
        }
        "generic" => serde_json::json!({
            "source": "buffterm",
            "title": msg.title,
            "text": msg.text,
            "risk": msg.risk,
            "host": msg.host_label,
        }),
        // Slack mrkdwn 的加粗是 *x*（单星号），**x** 会原样露出星号
        "slack" => serde_json::json!({
            "text": format!("*{}*\n\n{}", msg.title, msg.text.replace("**", "*"))
        }),
        // Discord.content 上限 2000 字符，超限会被 400 拒绝
        "discord" => serde_json::json!({
            "content": crate::util::truncate(
                &format!("**{}**\n\n{}", msg.title, msg.text),
                1900
            )
        }),
        // Google Chat 文本上限 4096 字符，加粗同为 *x*
        "googlechat" => serde_json::json!({
            "text": crate::util::truncate(
                &format!("*{}*\n\n{}", msg.title, msg.text.replace("**", "*")),
                4000
            )
        }),
        // MessageCard 同时被旧版连接器与 Workflows webhook 接受，不必按 URL 猜协议
        "teams" => serde_json::json!({
            "@type": "MessageCard",
            "@context": "https://schema.org/extensions",
            "themeColor": match msg.risk {
                "high" => "D93025",
                "medium" => "F59E0B",
                "low" => "22C55E",
                _ => "6B7280",
            },
            "summary": msg.title,
            "title": msg.title,
            "text": msg.text,
        }),
        _ => serde_json::json!({
            "msgtype": "markdown",
            "markdown": { "title": msg.title, "text": format!("{}\n\n{}", msg.title, msg.text) }
        }),
    }
}

/// 这三个平台即使 token 错误也返回 HTTP 200，真正的成败在响应体的错误码里，
/// 只看 HTTP 状态会把「推送失败」误报成成功。
fn check_webhook_response(kind: &str, status: u16, body: &str) -> Result<(), String> {
    if !(200..300).contains(&status) {
        return Err(format!(
            "{}: HTTP {status} {}",
            crate::i18n::t("webhook 返回错误", "webhook returned an error"),
            crate::util::truncate(body, 200)
        ));
    }
    // 这些平台成功即 2xx、失败走非 2xx；钉钉/企微/飞书则用 200 包裹错误码
    if matches!(
        kind,
        "generic" | "slack" | "discord" | "googlechat" | "teams"
    ) {
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let code = value
        .get("errcode")
        .and_then(|v| v.as_i64())
        .or_else(|| value.get("code").and_then(|v| v.as_i64()))
        .or_else(|| value.get("StatusCode").and_then(|v| v.as_i64()));
    match code {
        Some(0) | None => Ok(()),
        Some(code) => {
            let detail = value
                .get("errmsg")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("msg").and_then(|v| v.as_str()))
                .or_else(|| value.get("StatusMessage").and_then(|v| v.as_str()))
                .unwrap_or(body);
            Err(format!(
                "{}: {code} {}",
                crate::i18n::t("webhook 拒绝推送", "webhook rejected the push"),
                crate::util::truncate(detail, 200)
            ))
        }
    }
}

pub(crate) async fn send_webhook(
    settings: &AlertSettings,
    msg: &WebhookMessage<'_>,
) -> Result<(), String> {
    let url = settings
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::i18n::t("未配置 webhook 地址", "Webhook URL is not configured").to_string())?;
    let kind = webhook_kind(settings);
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(crate::i18n::t(
            "webhook 地址必须以 http:// 或 https:// 开头",
            "The webhook URL must start with http:// or https://",
        )
        .to_string());
    }
    // 三家机器人的 token/加签都在 URL 或正文里，明文 http 等于把凭据交给链路中间人；
    // generic 可能是用户自建的局域网中继，保留 http。
    if kind != "generic" && !url.starts_with("https://") {
        return Err(crate::i18n::t(
            "内置机器人通道必须使用 https:// 地址",
            "Built-in bot channels require an https:// URL",
        )
        .to_string());
    }
    let secret = settings
        .webhook_secret
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(crate::credentials::get_webhook_secret);
    let now_ts = crate::util::now();
    // 钉钉的签名进 URL（毫秒），飞书的签名进请求体（秒），走同一份已保存密钥
    let target = signed_webhook_url(kind, url, secret.as_deref(), now_ts * 1000);
    let feishu_sign = if kind == "feishu" {
        secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| (now_ts, feishu_signature(s, now_ts)))
    } else {
        None
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("HTTP 客户端初始化失败", "Failed to initialize the HTTP client")))?;
    let resp = client
        .post(&target)
        .json(&webhook_payload(
            kind,
            msg,
            feishu_sign.as_ref().map(|(ts, sign)| (*ts, sign.as_str())),
        ))
        .send()
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("请求 webhook 失败", "Request to the webhook failed")))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    check_webhook_response(kind, status, &body)
}

fn test_webhook_text() -> String {
    crate::i18n::t(
        "这是一条测试推送，说明 webhook 通道已生效。后续智能值守发现中/高风险时会自动推送到这里。",
        "This is a test push confirming the webhook channel works. Smart monitoring will push medium/high risk findings here automatically.",
    )
    .to_string()
}

// ---------- 巡检结论分发 ----------

#[derive(Default)]
pub(crate) struct NotifyOutcome {
    pub email_sent: bool,
    pub webhook_sent: bool,
    /// 保留字段：值守事件与手动巡检都直接通知，当前恒为 None。
    pub skip: Option<String>,
}

/// 向已配置的通道分发巡检/值守结论。手动巡检和值守事件都立即通知，
/// 值守的防刷屏由事件层（按主机+规则）的冷却负责，不在这里静默。
pub(crate) async fn dispatch_inspection(db: &Db, report: &InspectionReport) -> NotifyOutcome {
    let Ok(settings) = db.get_alert_settings() else {
        return NotifyOutcome::default();
    };

    let guard_event = report.source == "guard";
    let subject = format!(
        "{} {} - {}",
        if guard_event {
            crate::i18n::t("[buffTerm 智能值守]", "[buffTerm smart guard]")
        } else {
            crate::i18n::t("[buffTerm 巡检]", "[buffTerm inspection]")
        },
        report.host_label,
        report.risk_level
    );
    let mut outcome = NotifyOutcome::default();

    if non_empty(settings.smtp_host.as_deref()) && non_empty(settings.smtp_to.as_deref()) {
        outcome.email_sent = send_html_email(&settings, &subject, &report.html).await.is_ok();
    }
    if non_empty(settings.webhook_url.as_deref()) {
        // 用空行而非单个 \n 分段：钉钉 markdown 会把单换行折叠成空格，飞书卡片各版本
        // 对软换行的处理也不一致，空行是两边都确定会换行的写法（代价只是行距变大）。
        let text = format!(
            "**{}**\n\n{}：{}\n\n{}：{}\n\n{}\n\n{}",
            report.host_label,
            crate::i18n::t("风险等级", "Risk level"),
            report.risk_level,
            crate::i18n::t("触发方式", "Trigger"),
            if guard_event {
                crate::i18n::t("智能值守", "smart guard")
            } else {
                crate::i18n::t("手动巡检", "manual inspection")
            },
            report.summary,
            // 不用 `>` 引用块：飞书卡片 markdown 子集不保证支持 blockquote，
            // 原文照发会在部分版本里露出一个孤立的 > 字符。
            crate::i18n::t(
                "完整报告见 buffTerm 巡检与值守面板",
                "Full report is in the buffTerm inspection & guard panel",
            ),
        );
        let msg = WebhookMessage {
            title: &subject,
            text: &text,
            risk: &report.risk_level,
            host_label: &report.host_label,
        };
        outcome.webhook_sent = send_webhook(&settings, &msg).await.is_ok();
    }
    outcome
}

fn test_email_html() -> String {
    // 邮件正文跟随界面语言，保证测试邮件与当前语言一致
    let (subtitle, line1, line2, line3, badge) = if crate::i18n::is_en() {
        (
            "Notification settings test email",
            "This is a test email confirming that your <strong>email notification settings</strong> are working.",
            "From now on, when <strong>AI inspection</strong> runs, buffTerm sends the inspection report automatically through this SMTP channel.",
            "If you do not want to receive inspection emails, adjust the recipients in \"Notifications\".",
            "✓ Test email sent successfully",
        )
    } else {
        (
            "通知配置测试邮件",
            "这是一封测试邮件，说明你的 <strong>邮件通知配置</strong> 已生效。",
            "后续执行 <strong>AI 巡检</strong> 时，buffTerm 会通过这个 SMTP 渠道自动发送巡检报告。",
            "如果你不需要接收巡检邮件，可以在“通知配置”中调整收件人。",
            "✓ 测试发送成功",
        )
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>
      body{{margin:0;padding:28px;background:#f4f5f7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1f2430;}}
      .card{{max-width:680px;margin:0 auto;background:#ffffff;border-radius:16px;overflow:hidden;box-shadow:0 8px 24px rgba(15,23,42,.08);}}
      .hero{{padding:28px 30px;background:linear-gradient(135deg,#2f3b52,#4f5d78);color:#ffffff;}}
      .hero h1{{margin:0;font-size:22px;}}
      .hero p{{margin:8px 0 0;font-size:13px;opacity:.82;}}
      .body{{padding:24px 30px;line-height:1.7;font-size:14px;}}
      .badge{{display:inline-block;margin-top:12px;padding:5px 12px;border-radius:999px;background:#34d399;color:#fff;font-weight:700;font-size:12px;}}
      code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:#f1f3f5;padding:2px 6px;border-radius:5px;}}
    </style></head><body><div class="card">
      <div class="hero"><h1>buffTerm</h1><p>{subtitle}</p></div>
      <div class="body">
        <p>{line1}</p>
        <p>{line2}</p>
        <p>{line3}</p>
        <span class="badge">{badge}</span>
      </div>
    </div></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_flags_reach_frontend_but_never_come_back() {
        // skip 会连序列化一起跳过，前端就永远看不到「已保存」态；
        // skip_deserializing 才是要的：出得去、进不来。
        let s = AlertSettings {
            smtp_password_set: true,
            webhook_secret_set: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"smtp_password_set\":true"));
        assert!(json.contains("\"webhook_secret_set\":true"));
        let back: AlertSettings = serde_json::from_str(&json).unwrap();
        assert!(!back.smtp_password_set);
        assert!(!back.webhook_secret_set);
    }

    #[test]
    fn dingtalk_signature_is_stable_and_url_encoded() {
        let sign = dingtalk_signature("SECtestsecret", 1_700_000_000_000);
        assert_eq!(sign, dingtalk_signature("SECtestsecret", 1_700_000_000_000));
        assert!(!sign.contains('+') && !sign.contains('/') && !sign.contains('='));
        assert_ne!(sign, dingtalk_signature("SECother", 1_700_000_000_000));
        assert_ne!(sign, dingtalk_signature("SECtestsecret", 1_700_000_000_001));
    }

    #[test]
    fn overseas_payloads_match_each_platform_shape() {
        let msg = WebhookMessage {
            title: "T",
            text: "**h1**\n\nB",
            risk: "high",
            host_label: "h1",
        };
        // Slack / Google Chat 的加粗是单星号，**x** 会露出星号
        assert_eq!(
            webhook_payload("slack", &msg, None)["text"],
            "*T*\n\n*h1*\n\nB"
        );
        assert_eq!(
            webhook_payload("googlechat", &msg, None)["text"],
            "*T*\n\n*h1*\n\nB"
        );
        // Discord 支持 **x**，原样保留
        assert_eq!(
            webhook_payload("discord", &msg, None)["content"],
            "**T**\n\n**h1**\n\nB"
        );
        let teams = webhook_payload("teams", &msg, None);
        assert_eq!(teams["@type"], "MessageCard");
        assert_eq!(teams["title"], "T");
        assert_eq!(teams["themeColor"], "D93025");
    }

    #[test]
    fn discord_content_is_capped_below_its_2000_limit() {
        let long = "字".repeat(2500);
        let msg = WebhookMessage { title: "T", text: &long, risk: "low", host_label: "h" };
        let content = webhook_payload("discord", &msg, None)["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(content.chars().count() <= 1901);
    }

    #[test]
    fn webhook_kind_maps_all_builtin_channels() {
        for (raw, want) in [
            ("dingtalk", "dingtalk"),
            ("wecom", "wecom"),
            ("feishu", "feishu"),
            ("slack", "slack"),
            ("discord", "discord"),
            ("googlechat", "googlechat"),
            ("teams", "teams"),
            ("generic", "generic"),
        ] {
            let s = AlertSettings {
                webhook_kind: Some(raw.to_string()),
                ..Default::default()
            };
            assert_eq!(webhook_kind(&s), want);
        }
    }

    #[test]
    fn feishu_signature_matches_reference_vector() {
        // 参考向量由 openssl 独立计算（不是用本实现自测）：
        // printf '' | openssl dgst -sha256 -hmac $'1700000000\nSECtestsecret' -binary | base64
        assert_eq!(
            feishu_signature("SECtestsecret", 1_700_000_000),
            "qctwDqaazOo8xxU2d5mAVhFAk6TEeaDHQUh0YMWFIL8="
        );
        assert_ne!(
            feishu_signature("secondsecret", 1_700_000_000),
            "qctwDqaazOo8xxU2d5mAVhFAk6TEeaDHQUh0YMWFIL8="
        );
    }

    #[test]
    fn feishu_signed_body_carries_string_timestamp_and_sign() {
        let msg = WebhookMessage { title: "T", text: "B", risk: "low", host_label: "h1" };
        assert!(webhook_payload("feishu", &msg, None).get("timestamp").is_none());
        let signed = webhook_payload("feishu", &msg, Some((1_700_000_000, "SIG")));
        assert_eq!(signed["timestamp"], "1700000000");
        assert_eq!(signed["sign"], "SIG");
        assert_eq!(signed["msg_type"], "interactive");
    }

    #[test]
    fn dingtalk_signature_matches_reference_vector() {
        // 参照钉钉官方算法：HmacSHA256(key=secret, data="{ts}\n{secret}") → base64 → urlencode
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = "SECabcdef";
        let ts = 1_234_567_890_123u64;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{ts}\n{secret}").as_bytes());
        let expected = percent_encoding::utf8_percent_encode(
            &B64.encode(mac.finalize().into_bytes()),
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string();
        assert_eq!(dingtalk_signature(secret, ts), expected);
    }

    #[test]
    fn only_dingtalk_with_secret_gets_signed() {
        let url = "https://oapi.dingtalk.com/robot/send?access_token=abc";
        let signed = signed_webhook_url("dingtalk", url, Some("SECx"), 1000);
        assert!(signed.starts_with(url));
        assert!(signed.contains("&timestamp=1000&sign="));
        // 无密钥 / 非钉钉 / 空密钥都不改写 URL
        assert_eq!(signed_webhook_url("dingtalk", url, None, 1000), url);
        assert_eq!(signed_webhook_url("dingtalk", url, Some("  "), 1000), url);
        assert_eq!(signed_webhook_url("wecom", url, Some("SECx"), 1000), url);
        // 没有 query 时用 ? 起头
        assert!(signed_webhook_url("dingtalk", "https://x.dev/hook", Some("SECx"), 1000)
            .contains("?timestamp=1000&sign="));
    }

    #[test]
    fn payloads_match_each_platform_shape() {
        let msg = WebhookMessage { title: "T", text: "B", risk: "high", host_label: "h1" };
        let dingtalk = webhook_payload("dingtalk", &msg, None);
        assert_eq!(dingtalk["msgtype"], "markdown");
        assert_eq!(dingtalk["markdown"]["title"], "T");
        // 标题与正文之间用空行：单换行在钉钉 markdown 里会被折叠
        assert_eq!(dingtalk["markdown"]["text"], "T\n\nB");

        let wecom = webhook_payload("wecom", &msg, None);
        assert_eq!(wecom["msgtype"], "markdown");
        assert_eq!(wecom["markdown"]["content"], "T\n\nB");
        assert!(wecom["markdown"].get("title").is_none());

        let feishu = webhook_payload("feishu", &msg, None);
        assert_eq!(feishu["msg_type"], "interactive");
        assert_eq!(feishu["card"]["header"]["template"], "red");
        assert_eq!(feishu["card"]["elements"][0]["content"], "B");

        let generic = webhook_payload("generic", &msg, None);
        assert_eq!(generic["risk"], "high");
        assert_eq!(generic["host"], "h1");
        assert_eq!(generic["source"], "buffterm");
    }

    #[test]
    fn feishu_header_color_follows_risk() {
        for (risk, color) in [("high", "red"), ("medium", "orange"), ("low", "green"), ("unknown", "grey")] {
            let msg = WebhookMessage { title: "T", text: "B", risk, host_label: "h" };
            assert_eq!(webhook_payload("feishu", &msg, None)["card"]["header"]["template"], color);
        }
    }

    #[test]
    fn webhook_error_code_in_http_200_is_detected() {
        // 钉钉/企微 token 错误时仍是 HTTP 200，必须靠 errcode 判失败
        assert!(check_webhook_response("dingtalk", 200, r#"{"errcode":310000,"errmsg":"keywords not in content"}"#).is_err());
        assert!(check_webhook_response("wecom", 200, r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#).is_err());
        assert!(check_webhook_response("feishu", 200, r#"{"code":19021,"msg":"sign match fail"}"#).is_err());
        assert!(check_webhook_response("dingtalk", 200, r#"{"errcode":0,"errmsg":"ok"}"#).is_ok());
        assert!(check_webhook_response("feishu", 200, r#"{"StatusCode":0,"code":0,"msg":"success"}"#).is_ok());
        // 通用通道只看 HTTP 状态
        assert!(check_webhook_response("generic", 200, "anything").is_ok());
        assert!(check_webhook_response("generic", 500, "boom").is_err());
        assert!(check_webhook_response("dingtalk", 404, "not found").is_err());
    }

    #[test]
    fn webhook_kind_defaults_to_dingtalk() {
        let mut settings = AlertSettings::default();
        assert_eq!(webhook_kind(&settings), "dingtalk");
        settings.webhook_kind = Some("wecom".to_string());
        assert_eq!(webhook_kind(&settings), "wecom");
        settings.webhook_kind = Some("nonsense".to_string());
        assert_eq!(webhook_kind(&settings), "dingtalk");
    }
}
