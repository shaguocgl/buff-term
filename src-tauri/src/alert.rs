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
    db.get_alert_settings()
        .map_err(|e| format!("读取邮件设置失败: {e}"))
}

#[tauri::command]
pub fn save_alert_settings(db: State<'_, Arc<Db>>, settings: AlertSettings) -> Result<(), String> {
    db.save_alert_settings(&settings)
        .map_err(|e| format!("保存邮件设置失败: {e}"))
}

/// 测试 SMTP 邮件发送（用传入的设置，未保存也可测）
#[tauri::command]
pub async fn test_alert_settings(settings: AlertSettings) -> Result<TestResult, String> {
    match send_html_email(
        &settings,
        "buffTerm 通知配置测试",
        &test_email_html(),
    )
    .await
    {
        Ok(()) => Ok(TestResult { ok: true, message: "邮件发送成功，请检查收件箱".to_string() }),
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
        .ok_or_else(|| "未配置 SMTP 服务器".to_string())?;
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
    let mut builder = Message::builder()
        .from(from_mb)
        .subject(title)
        .header(ContentType::TEXT_HTML);
    for addr in to
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mb: Mailbox = addr.parse().map_err(|e| format!("收件人地址无效: {e}"))?;
        builder = builder.to(mb);
    }
    let email = builder
        .body(html.to_string())
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
        .map_err(|e| format!("HTML 邮件发送失败: {e}"))?;
    Ok(())
}

fn test_email_html() -> String {
    r#"<!doctype html><html><head><meta charset="utf-8"><style>
      body{margin:0;padding:28px;background:#f4f5f7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1f2430;}
      .card{max-width:680px;margin:0 auto;background:#ffffff;border-radius:16px;overflow:hidden;box-shadow:0 8px 24px rgba(15,23,42,.08);}
      .hero{padding:28px 30px;background:linear-gradient(135deg,#2f3b52,#4f5d78);color:#ffffff;}
      .hero h1{margin:0;font-size:22px;}
      .hero p{margin:8px 0 0;font-size:13px;opacity:.82;}
      .body{padding:24px 30px;line-height:1.7;font-size:14px;}
      .badge{display:inline-block;margin-top:12px;padding:5px 12px;border-radius:999px;background:#34d399;color:#fff;font-weight:700;font-size:12px;}
      code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:#f1f3f5;padding:2px 6px;border-radius:5px;}
    </style></head><body><div class="card">
      <div class="hero"><h1>buffTerm</h1><p>通知配置测试邮件</p></div>
      <div class="body">
        <p>这是一封测试邮件，说明你的 <strong>邮件通知配置</strong> 已生效。</p>
        <p>后续执行 <strong>AI 巡检</strong> 时，buffTerm 会通过这个 SMTP 渠道自动发送巡检报告。</p>
        <p>如果你不需要接收巡检邮件，可以在“通知配置”中调整收件人。</p>
        <span class="badge">✓ 测试发送成功</span>
      </div>
    </div></body></html>"#.to_string()
}
