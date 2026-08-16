use crate::db::Db;
use crate::models::{AlertSettings, Host, InspectionReport};
use crate::russh::RusshManager;
use crate::safety::{sanitize, validate_readonly_command};
use crate::util::{extract_error, now, truncate, truncate_output};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
pub struct InspectionManager {
    flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl InspectionManager {
    fn register(&self, id: String) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.flags.lock().unwrap().insert(id, flag.clone());
        flag
    }

    fn cancel(&self, id: &str) -> bool {
        let flag = self.flags.lock().unwrap().get(id).cloned();
        if let Some(flag) = flag {
            flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn unregister(&self, id: &str) {
        self.flags.lock().unwrap().remove(id);
    }
}

#[derive(Clone, Serialize)]
pub struct InspectionProgress {
    pub report_id: String,
    pub phase: String,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub struct InspectionDone {
    pub report_id: String,
    pub status: String,
}

#[derive(Clone, Serialize)]
pub struct InspectionError {
    pub report_id: String,
    pub message: String,
}

#[tauri::command]
pub async fn start_inspection(
    app: AppHandle,
    state: State<'_, InspectionManager>,
    host: Host,
) -> Result<String, String> {
    let db = app.state::<Db>();
    let (provider, model) = crate::ai::resolve_active_ai(&db)?;

    let report = InspectionReport {
        id: uuid::Uuid::new_v4().to_string(),
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        model: model.clone(),
        status: "running".to_string(),
        risk_level: "unknown".to_string(),
        summary: String::new(),
        markdown: String::new(),
        html: String::new(),
        email_sent: false,
        error: None,
        created_at: now(),
        finished_at: None,
        duration_ms: None,
    };
    db.insert_inspection_report(&report)
        .map_err(|e| format!("创建巡检任务失败: {e}"))?;

    let report_id = report.id.clone();
    let flag = state.register(report_id.clone());
    let app_for_task = app.clone();
    let base_url = provider.base_url.clone();
    let host_for_task = host.clone();
    tauri::async_runtime::spawn(async move {
        run_inspection(app_for_task, report, flag, host_for_task, base_url).await;
    });
    Ok(report_id)
}

#[tauri::command]
pub fn get_inspection_report(
    db: State<'_, Db>,
    id: String,
) -> Result<Option<InspectionReport>, String> {
    db.get_inspection_report(&id)
        .map_err(|e| format!("读取巡检报告失败: {e}"))
}

#[tauri::command]
pub fn list_inspection_reports(
    db: State<'_, Db>,
    host_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<InspectionReport>, String> {
    db.list_inspection_reports(host_id.as_deref(), limit.unwrap_or(30))
        .map_err(|e| format!("读取巡检报告列表失败: {e}"))
}

#[tauri::command]
pub fn delete_inspection_report(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_inspection_report(&id)
        .map_err(|e| format!("删除巡检报告失败: {e}"))
}

#[tauri::command]
pub fn cancel_inspection(
    state: State<'_, InspectionManager>,
    id: String,
) -> Result<(), String> {
    if state.cancel(&id) {
        Ok(())
    } else {
        Err("巡检任务不存在或已完成".to_string())
    }
}

async fn run_inspection(
    app: AppHandle,
    mut report: InspectionReport,
    flag: Arc<AtomicBool>,
    host: Host,
    base_url: String,
) {
    let started = Instant::now();
    let report_id = report.id.clone();
    let result = run_inspection_inner(&app, &mut report, &flag, &host, &base_url).await;

    report.finished_at = Some(now());
    report.duration_ms = Some(started.elapsed().as_millis() as u64);
    if report.status == "cancelled" {
        let _ = app.state::<Db>().update_inspection_report(&report);
        let _ = app.emit(
            "inspection:done",
            InspectionDone {
                report_id: report_id.clone(),
                status: report.status.clone(),
            },
        );
        let _ = app.state::<InspectionManager>().unregister(&report_id);
        return;
    }
    if let Err(message) = result {
        report.status = "failed".to_string();
        report.error = Some(message.clone());
        let _ = app.state::<Db>().update_inspection_report(&report);
        let _ = app.emit(
            "inspection:error",
            InspectionError {
                report_id: report_id.clone(),
                message,
            },
        );
    } else {
        report.status = "success".to_string();
        let _ = app.state::<Db>().update_inspection_report(&report);
        let _ = app.emit(
            "inspection:done",
            InspectionDone {
                report_id: report_id.clone(),
                status: report.status.clone(),
            },
        );
    }
    let _ = app.state::<InspectionManager>().unregister(&report_id);
}

async fn run_inspection_inner(
    app: &AppHandle,
    report: &mut InspectionReport,
    flag: &Arc<AtomicBool>,
    host: &Host,
    base_url: &str,
) -> Result<(), String> {
    if cancelled(flag) {
        report.status = "cancelled".to_string();
        report.error = Some("用户取消".to_string());
        return Ok(());
    }

    emit_progress(app, &report.id, "collect", "开始采集服务器基线数据");
    let russh = app.state::<RusshManager>();
    let baseline = russh
        .exec(host, BASELINE_SCRIPT, Duration::from_secs(50))
        .await?;
    let baseline_text = sanitize(&truncate_output(&baseline.text, 52000));

    if cancelled(flag) {
        report.status = "cancelled".to_string();
        report.error = Some("用户取消".to_string());
        return Ok(());
    }

    let db = app.state::<Db>();
    let api_key = crate::credentials::get_api_key(&report.provider_id)
        .ok_or_else(|| "API Key 未找到，请在 AI 配置中检查".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    emit_progress(app, &report.id, "analyze", "AI 正在分析服务器状态");
    let markdown = run_ai_inspection(
        app,
        &client,
        &url,
        &api_key,
        &report.model,
        host,
        &report.id,
        &baseline_text,
        flag,
    )
    .await?;
    if cancelled(flag) {
        report.status = "cancelled".to_string();
        report.error = Some("用户取消".to_string());
        return Ok(());
    }

    report.risk_level = risk_level(&markdown);
    report.summary = summary_from_markdown(&markdown);
    report.markdown = markdown.clone();

    emit_progress(app, &report.id, "render", "正在生成 HTML 报告");
    let body_html = markdown_to_html(&markdown);
    report.html = wrap_email_html(&report.host_label, &report.risk_level, &body_html);

    emit_progress(app, &report.id, "email", "正在发送巡检邮件");
    let settings: AlertSettings = db
        .get_alert_settings()
        .map_err(|e| format!("读取邮件设置失败: {e}"))?;
    if settings.smtp_host.as_deref().map(|s| !s.trim().is_empty()) == Some(true)
        && settings.smtp_to.as_deref().map(|s| !s.trim().is_empty()) == Some(true)
    {
        let subject = format!("[KeyWisp 巡检] {} - {}", report.host_label, report.risk_level);
        if crate::alert::send_html_email(&settings, &subject, &report.html).await.is_ok() {
            report.email_sent = true;
        }
    }

    Ok(())
}

async fn run_ai_inspection(
    app: &AppHandle,
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    model: &str,
    host: &Host,
    report_id: &str,
    baseline: &str,
    flag: &Arc<AtomicBool>,
) -> Result<String, String> {
    let system = inspection_system_prompt(host);
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({
            "role": "user",
            "content": format!(
                "以下是服务器只读巡检采集结果，请生成完整的中文 Markdown 巡检报告。\n\n{}",
                baseline
            )
        }),
    ];

    for _ in 0..7 {
        if cancelled(flag) {
            return Ok(String::new());
        }
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "tools": inspection_tools_schema(),
            "tool_choice": "auto",
        });
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("请求 AI 平台失败: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(extract_error(&text, status));
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析 AI 响应失败: {e}"))?;
        let message = value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_calls = message
            .get("tool_calls")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));

        let calls = tool_calls.as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            if content.trim().is_empty() {
                return Err("AI 未返回巡检报告".to_string());
            }
            return Ok(content);
        }

        messages.push(serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        }));

        for call in calls {
            if cancelled(flag) {
                return Ok(String::new());
            }
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let function = call.get("function").cloned().unwrap_or(serde_json::json!({}));
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let raw_args = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let args: serde_json::Value =
                serde_json::from_str(raw_args).unwrap_or(serde_json::json!({}));
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");

            if name != "inspect_exec" || command.trim().is_empty() {
                messages.push(tool_message(&id, "只读检查已拒绝：工具名或命令参数无效"));
                continue;
            }
            if let Err(reason) = validate_readonly_command(command) {
                messages.push(tool_message(&id, &format!("只读检查已拒绝: {reason}")));
                continue;
            }

            emit_progress(app, report_id, "exec", command);
            let host = host.clone();
            let out = app
                .state::<RusshManager>()
                .exec(&host, command, Duration::from_secs(20))
                .await
                .map(|o| truncate_output(&o.text, 8000))
                .unwrap_or_else(|e| format!("执行失败: {e}"));
            messages.push(tool_message(&id, &out));
        }
    }

    Err("AI 巡检工具调用次数过多，已停止".to_string())
}

fn emit_progress(app: &AppHandle, report_id: &str, phase: &str, message: &str) {
    let _ = app.emit(
        "inspection:progress",
        InspectionProgress {
            report_id: report_id.to_string(),
            phase: phase.to_string(),
            message: message.to_string(),
        },
    );
}

fn cancelled(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

fn tool_message(id: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"role": "tool", "tool_call_id": id, "content": content})
}

fn inspection_system_prompt(host: &Host) -> String {
    format!(
        "你是 KeyWisp Agent Ops 的服务器巡检专家，负责对当前连接的服务器生成专业、可执行的中文巡检报告。\n\
         当前服务器：{}（{}@{}:{}）\n\
         你只能调用 inspect_exec 工具执行只读检查；不得执行任何写操作。\n\
         报告必须包含以下四个模块：\n\
         1. 资源使用情况\n\
         2. 运行服务\n\
         3. 安全基线\n\
         4. 木马与挖矿风险\n\
         5. 登录与风险事件\n\
         最后给出总体风险等级（低/中/高）和优先级明确的整改建议。\n\
         整改建议必须严格基于采集结果中的实际配置与数值，禁止凭空猜测或套用模板：\n\
         - 若某项配置已经满足推荐阈值（例如 fail2ban maxretry 已小于等于 3、bantime 已大于等于 3600 秒），\
         不要再次建议“降低/提高”该值；应明确说明“已满足，无需整改”，并引用实际数值。\n\
         - 若采集结果中缺少某项配置数据，不要臆造当前值，应说明“未采集到，建议人工确认”。\n\
         木马与挖矿判断只依据采集到的高 CPU 进程、/tmp|/dev/shm 可疑可执行文件、异常对外连接、\
         cron/systemd timer、SSH 授权文件变动等证据；只报告可疑点和证据，并建议人工确认，不要仅凭进程名就断定已感染。",
        host.name,
        host.username,
        host.address,
        host.port
    )
}

fn inspection_tools_schema() -> serde_json::Value {
    serde_json::json!([{
        "type": "function",
        "function": {
            "name": "inspect_exec",
            "description": "在服务器上执行一条只读巡检命令，返回文本输出。",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "只读 shell 命令" }
                },
                "required": ["command"]
            }
        }
    }])
}

fn risk_level(markdown: &str) -> String {
    if markdown.contains("高风险") || markdown.contains("严重") || markdown.contains("高危") {
        "high".to_string()
    } else if markdown.contains("中风险") || markdown.contains("中等") {
        "medium".to_string()
    } else if markdown.contains("低风险") || markdown.contains("正常") {
        "low".to_string()
    } else {
        "unknown".to_string()
    }
}

fn summary_from_markdown(markdown: &str) -> String {
    let text = markdown.trim();
    truncate(text, 300)
}

fn markdown_to_html(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    comrak::markdown_to_html(markdown, &options)
}

fn wrap_email_html(host_label: &str, risk: &str, body: &str) -> String {
    let risk_color = match risk {
        "high" => "#e5484d",
        "medium" => "#f5a623",
        "low" => "#30a46c",
        _ => "#8b8d98",
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <style>table{{border-collapse:collapse;width:100%;margin:14px 0;font-size:13px;}}th,td{{border:1px solid #dfe3e8;padding:8px 10px;text-align:left;vertical-align:top;}}th{{background:#f4f5f7;font-weight:650;}}code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12px;background:#f1f3f5;padding:2px 5px;border-radius:4px;}}pre{{background:#f4f5f7;padding:12px 14px;border-radius:8px;overflow-x:auto;}}pre code{{background:transparent;padding:0;}}</style>\
         </head>\
         <body style=\"margin:0;padding:24px;background:#f4f5f7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1f2430;\">\
         <div style=\"max-width:900px;margin:0 auto;background:#ffffff;border-radius:14px;overflow:hidden;box-shadow:0 8px 24px rgba(15,23,42,.08);\">\
         <div style=\"padding:24px 28px;background:linear-gradient(135deg,#2f3b52,#4f5d78);color:#ffffff;\">\
         <div style=\"font-size:13px;opacity:.75;\">KeyWisp Agent Ops · AI 巡检报告</div>\
         <div style=\"font-size:22px;font-weight:700;margin-top:4px;\">{}</div>\
         <div style=\"margin-top:10px;font-size:13px;\">风险等级：<span style=\"display:inline-block;padding:2px 10px;border-radius:999px;background:{};color:#fff;\">{}</span></div>\
         </div>\
         <div style=\"padding:24px 28px;line-height:1.65;font-size:14px;\">{}</div>\
         </div></body></html>",
        host_label, risk_color, risk, body
    )
}

const BASELINE_SCRIPT: &str = r#"
echo "===BEGIN==="
echo "==SYSTEM=="
hostname 2>/dev/null || true
uname -a 2>/dev/null || true
cat /etc/os-release 2>/dev/null | head -n 8
uptime 2>/dev/null || true
echo "==RESOURCE=="
free -h 2>/dev/null || true
df -hP 2>/dev/null || true
ps -eo user,%cpu,%mem,args --sort=-%cpu 2>/dev/null | head -n 12
echo "==SERVICES=="
systemctl list-units --type=service --state=running --no-pager --no-legend 2>/dev/null | head -n 60
ss -tulnp 2>/dev/null | head -n 80
docker ps --format '{{.Names}} {{.Image}} {{.Status}} {{.Ports}}' 2>/dev/null | head -n 40
echo "==SECURITY=="
sshd -T 2>/dev/null | grep -E '^(permitrootlogin|passwordauthentication|pubkeyauthentication|permitemptypasswords|x11forwarding)'
for s in ufw firewalld fail2ban; do echo "$s=$(systemctl is-active "$s" 2>/dev/null || echo unknown)"; done
echo "==FAIL2BAN=="
fail2ban-client status 2>/dev/null || true
for f in /etc/fail2ban/jail.local /etc/fail2ban/jail.conf /etc/fail2ban/jail.d/*.conf /etc/fail2ban/jail.d/*.local; do
  [ -f "$f" ] || continue
  echo "--- $f ---"
  grep -Ei '^\s*(maxretry|bantime|findtime|enabled|backend|banaction|ignoreip)' "$f" 2>/dev/null | head -n 80
done
ufw status verbose 2>/dev/null | head -n 30
firewall-cmd --state 2>/dev/null || true
getent passwd 2>/dev/null | awk -F: '$7 ~ /sh$/ {print $1":"$3":"$7}' | head -n 120
grep -RhE '^(root|%sudo|%wheel)[[:space:]]' /etc/sudoers /etc/sudoers.d 2>/dev/null | head -n 80
echo "==MALWARE=="
ps -eo pid,user,%cpu,%mem,args 2>/dev/null | grep -Ei 'xmrig|kdevtmpfsi|kinsing|minergate|cgminer|bfgminer|ethminer|t-rex|phoenixminer|nbminer|gminer|/tmp/|/dev/shm/' | grep -v grep | head -n 80
ss -tunp 2>/dev/null | head -n 120 || true
for c in /var/spool/cron/crontabs/* /var/spool/cron/*; do
  [ -f "$c" ] || continue
  echo "--- $c ---"
  head -n 60 "$c" 2>/dev/null
done
crontab -l 2>/dev/null | head -n 80 || true
for f in /etc/crontab /etc/cron.d/* /etc/cron.daily/* /etc/cron.hourly/* /etc/cron.weekly/* /etc/cron.monthly/*; do
  [ -f "$f" ] || continue
  echo "--- $f ---"
  head -n 40 "$f" 2>/dev/null
done
systemctl list-timers --all --no-pager --no-legend 2>/dev/null | head -n 80
echo "-- tmp-shm-executables --"
find /tmp /dev/shm /var/tmp -maxdepth 2 -type f \( -perm -u+x -o -perm -g+x -o -perm -o+x \) -ls 2>/dev/null | head -n 80
echo "-- recent-authorized-keys --"
find /root /home -maxdepth 4 -type f -name 'authorized_keys' -mtime -14 -ls 2>/dev/null | head -n 60
echo "==EVENTS=="
last -20 2>/dev/null || true
lastb -20 2>/dev/null || true
grep -iE 'failed|invalid|authentication failure|sudo' /var/log/auth.log /var/log/secure 2>/dev/null | tail -n 100
echo "===END==="
"#;
