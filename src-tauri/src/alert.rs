//! 通知配置：当前仅支持邮件（SMTP）配置与测试连接。

use std::sync::Arc;
use crate::db::Db;
use crate::models::AlertSettings;
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
    // SMTP 密码加密存凭据表，读取时解密回填供前端展示
    if settings.smtp_password.is_none() {
        settings.smtp_password = crate::credentials::get_smtp_password();
    }
    Ok(settings)
}

#[tauri::command]
pub fn save_alert_settings(db: State<'_, Arc<Db>>, settings: AlertSettings) -> Result<(), String> {
    // 密码单独加密存储，不随 JSON 明文落库
    if let Some(password) = settings.smtp_password.clone() {
        if !password.trim().is_empty() {
            crate::credentials::save_smtp_password(&password)?;
        }
    }
    let mut to_save = settings;
    to_save.smtp_password = None;
    db.save_alert_settings(&to_save)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存邮件设置失败", "Failed to save email settings")))
}

/// 测试 SMTP 邮件发送（用传入的设置，未保存也可测）
#[tauri::command]
pub async fn test_alert_settings(settings: AlertSettings) -> Result<TestResult, String> {
    match send_html_email(
        &settings,
        crate::i18n::t("buffTerm 通知配置测试", "buffTerm notification settings test"),
        &test_email_html(),
    )
    .await
    {
        Ok(()) => Ok(TestResult { ok: true, message: crate::i18n::t("邮件发送成功，请检查收件箱", "Test email sent — check your inbox").to_string() }),
        Err(e) => Ok(TestResult { ok: false, message: e }),
    }
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
