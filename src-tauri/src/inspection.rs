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
    let db = app.state::<Arc<Db>>();
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
    db: State<'_, Arc<Db>>,
    id: String,
) -> Result<Option<InspectionReport>, String> {
    db.get_inspection_report(&id)
        .map_err(|e| format!("读取巡检报告失败: {e}"))
}

#[tauri::command]
pub fn list_inspection_reports(
    db: State<'_, Arc<Db>>,
    host_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<InspectionReport>, String> {
    db.list_inspection_reports(host_id.as_deref(), limit.unwrap_or(30))
        .map_err(|e| format!("读取巡检报告列表失败: {e}"))
}

#[tauri::command]
pub fn delete_inspection_report(db: State<'_, Arc<Db>>, id: String) -> Result<(), String> {
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
        let _ = app.state::<Arc<Db>>().update_inspection_report(&report);
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
        let _ = app.state::<Arc<Db>>().update_inspection_report(&report);
        let _ = app.emit(
            "inspection:error",
            InspectionError {
                report_id: report_id.clone(),
                message,
            },
        );
    } else {
        report.status = "success".to_string();
        let _ = app.state::<Arc<Db>>().update_inspection_report(&report);
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
    // 巡检开始时先采集一份轻量快照写入历史指标，与巡检报告互补。
    if let Ok(snap) = crate::monitor::collect_russh(host, &russh).await {
        let _ = crate::monitor::save_metric(&app.state::<Arc<Db>>(), &host.id, &snap, "inspection");
    }
    let baseline = russh
        .exec(host, BASELINE_SCRIPT, Duration::from_secs(50))
        .await?;
    let mut baseline_text = sanitize(&truncate_output(&baseline.text, 52000));

    // 注入历史趋势：查最近 7 天和 30 天的指标，拼成趋势摘要追加到基线数据
    let db = app.state::<Arc<Db>>();
    let now_ts = now();
    let trend_7d = build_trend_summary(&db, &host.id, now_ts, 168);
    let trend_30d = build_trend_summary(&db, &host.id, now_ts, 720);
    if !trend_7d.is_empty() || !trend_30d.is_empty() {
        baseline_text.push_str("\n\n=== 历史趋势 ===\n");
        if !trend_7d.is_empty() {
            baseline_text.push_str(&format!("【最近 7 天】\n{}\n", trend_7d));
        }
        if !trend_30d.is_empty() {
            baseline_text.push_str(&format!("【最近 30 天】\n{}\n", trend_30d));
        }
    }

    if cancelled(flag) {
        report.status = "cancelled".to_string();
        report.error = Some("用户取消".to_string());
        return Ok(());
    }

    let api_key = crate::credentials::get_api_key(&report.provider_id)
        .ok_or_else(|| "API Key 未找到，请在 AI 配置中检查".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    emit_progress(app, &report.id, "analyze", "AI 正在分析服务器状态");
    let inspection_ctx = AiInspectionCtx {
        app,
        client: &client,
        url: &url,
        api_key: &api_key,
        model: &report.model,
        host,
        report_id: &report.id,
        flag,
    };
    let markdown = run_ai_inspection(&inspection_ctx, &baseline_text).await?;
    if cancelled(flag) {
        report.status = "cancelled".to_string();
        report.error = Some("用户取消".to_string());
        return Ok(());
    }

    let (risk, clean_markdown) = extract_risk_level(&markdown);
    report.risk_level = risk;
    report.summary = summary_from_markdown(&clean_markdown);
    report.markdown = clean_markdown.clone();

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
        let subject = format!("[buffTerm 巡检] {} - {}", report.host_label, report.risk_level);
        if crate::alert::send_html_email(&settings, &subject, &report.html).await.is_ok() {
            report.email_sent = true;
        }
    }

    Ok(())
}

/// `run_ai_inspection` 所需的只读上下文（此前 9 个位置参数，收敛为具名字段结构体）。
/// `baseline`（本次巡检的采集数据）单独作为参数传入，因为它是每次调用变化的输入，而非上下文。
/// 字段全部是引用，因此整体可以 Copy，方便按值解构。
#[derive(Clone, Copy)]
struct AiInspectionCtx<'a> {
    app: &'a AppHandle,
    client: &'a reqwest::Client,
    url: &'a str,
    api_key: &'a str,
    model: &'a str,
    host: &'a Host,
    report_id: &'a str,
    flag: &'a Arc<AtomicBool>,
}

async fn run_ai_inspection(ctx: &AiInspectionCtx<'_>, baseline: &str) -> Result<String, String> {
    let AiInspectionCtx { app, client, url, api_key, model, host, report_id, flag } = *ctx;
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
            .map_err(|e| {
                let msg = e.to_string();
                if e.is_timeout() {
                    format!("请求 AI 平台超时（180s），模型可能响应过慢或网络不畅: {msg}")
                } else if e.is_connect() {
                    format!("无法连接 AI 平台，请检查网络或 Base URL: {msg}")
                } else {
                    format!("请求 AI 平台失败: {msg}")
                }
            })?;
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
        "你是 buffTerm 的服务器巡检专家，负责对当前连接的服务器生成专业、可执行的中文巡检报告。\n\
         当前服务器：{}（{}@{}:{}）\n\
         你只能调用 inspect_exec 工具执行只读检查；不得执行任何写操作。\n\
         \n\
         ## 报告结构\n\
         报告应覆盖以下维度，根据采集结果突出异常项，无异常的模块简要说明即可，无需展开填充：\n\
         1. 趋势变化：若采集数据中包含\"=== 历史趋势 ===\"段落，基于变化速率做预判\
         （如\"按当前增速，X 天后磁盘满\"）。若指标平稳，简要说明\"无异常趋势\"即可。\n\
         2. 资源使用情况\n\
         3. 运行服务\n\
         4. 安全基线\n\
         5. 木马与挖矿风险\n\
         6. 登录与风险事件\n\
         \n\
         ## 分析要求\n\
         - 权限感知：采集数据开头的 uid 标识当前用户。若 uid 非 0（非 root），sshd -T、\n\
         fail2ban、/etc/sudoers、/var/log/auth.log 等段可能因权限不足为空，\n\
         应说明\"当前用户无权限读取，建议以 root 用户巡检\"，不要将空数据误判为\"未配置\"。\n\
         - 关联分析：注意跨模块的异常关联（如高 CPU 进程 + 异常对外连接 + 可疑 crontab 同时出现），\n\
         综合判断而非孤立描述各模块。\n\
         - 整改建议按风险和影响范围排序，优先给出可能导致服务中断或安全事件的项目。\n\
         - 整改建议必须严格基于采集结果中的实际配置与数值，禁止凭空猜测或套用模板：\n\
           若某项配置已满足推荐阈值，明确说明\"已满足，无需整改\"并引用实际数值；\n\
           若缺少某项配置数据，说明\"未采集到，建议人工确认\"，不要臆造当前值。\n\
         - 木马与挖矿判断只依据采集到的证据（高 CPU 进程、/tmp|/dev/shm 可疑可执行文件、\n\
         异常对外连接、cron/systemd timer、SSH 授权文件变动等），只报告可疑点和证据并建议人工确认，\n\
         不要仅凭进程名就断定已感染。\n\
         \n\
         最后给出总体风险等级（低/中/高），并在报告最末尾另起一行输出标记：\
         `[RISK:high]` 或 `[RISK:medium]` 或 `[RISK:low]`，\
         该标记用于程序解析，不会展示给用户。",
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

/// 从报告末尾的 [RISK:xxx] 标记提取风险等级，并返回剔除标记后的 markdown。
/// fallback：若未找到标记，退回到按行匹配"风险等级"关键词。
fn extract_risk_level(markdown: &str) -> (String, String) {
    // 优先匹配 [RISK:high] / [RISK:medium] / [RISK:low]
    for tag in ["[RISK:high]", "[RISK:medium]", "[RISK:low]"] {
        if let Some(pos) = markdown.rfind(tag) {
            let level = match tag {
                "[RISK:high]" => "high",
                "[RISK:medium]" => "medium",
                "[RISK:low]" => "low",
                _ => "unknown",
            };
            // 剔除标记行及其前后的空行
            let before = &markdown[..pos];
            let after = &markdown[pos + tag.len()..];
            let clean = format!("{}{}", before.trim_end_matches('\n'), after).trim().to_string();
            return (level.to_string(), clean);
        }
    }
    // fallback：按行匹配包含"风险等级"的行
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.contains("风险等级") || trimmed.contains("风险评级") {
            if trimmed.contains("高") || trimmed.contains("严重") || trimmed.contains("高危") {
                return ("high".to_string(), markdown.to_string());
            } else if trimmed.contains("中") || trimmed.contains("中等") {
                return ("medium".to_string(), markdown.to_string());
            } else if trimmed.contains("低") || trimmed.contains("正常") {
                return ("low".to_string(), markdown.to_string());
            }
        }
    }
    ("unknown".to_string(), markdown.to_string())
}

fn summary_from_markdown(markdown: &str) -> String {
    let text = markdown.trim();
    truncate(text, 300)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_risk_level_reads_tag_and_strips_it() {
        let md = "## 巡检报告\n一切正常。\n\n[RISK:low]";
        let (level, clean) = extract_risk_level(md);
        assert_eq!(level, "low");
        assert!(!clean.contains("[RISK:low]"));
        assert!(clean.contains("一切正常"));
    }

    #[test]
    fn extract_risk_level_ignores_keyword_in_body_when_tag_present() {
        // 正文里出现"高风险"这个词，但结尾标记是 medium，应以标记为准（回归此前的误判 bug）
        let md = "整改建议：P0 项属于高风险，需优先处理。\n\n[RISK:medium]";
        let (level, _) = extract_risk_level(md);
        assert_eq!(level, "medium");
    }

    #[test]
    fn extract_risk_level_falls_back_to_line_match_without_tag() {
        let md = "8. 总体风险等级：中\n判定依据：略";
        let (level, clean) = extract_risk_level(md);
        assert_eq!(level, "medium");
        // fallback 分支不剔除任何内容
        assert_eq!(clean, md);
    }

    #[test]
    fn extract_risk_level_unknown_when_no_signal() {
        let md = "这是一份没有明确结论的报告。";
        let (level, _) = extract_risk_level(md);
        assert_eq!(level, "unknown");
    }

    #[test]
    fn extract_risk_level_high_tag() {
        let md = "详细分析...\n[RISK:high]";
        let (level, clean) = extract_risk_level(md);
        assert_eq!(level, "high");
        assert!(!clean.contains("RISK"));
    }
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
         <div style=\"font-size:13px;opacity:.75;\">buffTerm · AI 巡检报告</div>\
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
echo "uid=$(id -u 2>/dev/null || echo unknown)"
hostname 2>/dev/null || true
uname -a 2>/dev/null || true
cat /etc/os-release 2>/dev/null | head -n 8
uptime 2>/dev/null || true
echo "==RESOURCE=="
free -h 2>/dev/null || free 2>/dev/null || true
df -hP 2>/dev/null || true
ps -eo user,%cpu,%mem,args --sort=-%cpu 2>/dev/null | head -n 12 || ps aux 2>/dev/null | head -n 12 || true
echo "==NETWORK=="
ip addr 2>/dev/null | head -n 80 || ifconfig -a 2>/dev/null | head -n 80 || true
ip route 2>/dev/null | head -n 40 || route -n 2>/dev/null | head -n 40 || true
echo "==SERVICES=="
if command -v systemctl >/dev/null 2>&1; then
  systemctl list-units --type=service --state=running --no-pager --no-legend 2>/dev/null | head -n 60
else
  service --status-all 2>/dev/null | head -n 60 || true
fi
ss -tulnp 2>/dev/null | head -n 80 || netstat -tulnp 2>/dev/null | head -n 80 || true
docker ps --format '{{.Names}} {{.Image}} {{.Status}} {{.Ports}}' 2>/dev/null | head -n 40 || true
podman ps --format '{{.Names}} {{.Image}} {{.Status}} {{.Ports}}' 2>/dev/null | head -n 40 || true
echo "==SECURITY=="
sshd -T 2>/dev/null | grep -E '^(permitrootlogin|passwordauthentication|pubkeyauthentication|permitemptypasswords|x11forwarding)'
for s in ufw firewalld fail2ban; do
  if command -v systemctl >/dev/null 2>&1; then
    echo "$s=$(systemctl is-active "$s" 2>/dev/null || echo unknown)"
  else
    echo "$s=$(service "$s" status 2>/dev/null | head -n 1 || echo unknown)"
  fi
done
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
echo "==UPDATES=="
apt list --upgradable 2>/dev/null | head -n 40 || true
yum check-update --quiet 2>/dev/null | head -n 40 || true
dnf check-update --quiet 2>/dev/null | head -n 40 || true
echo "==MALWARE=="
ps -eo pid,user,%cpu,%mem,args 2>/dev/null | grep -Ei 'xmrig|kdevtmpfsi|kinsing|minergate|cgminer|bfgminer|ethminer|t-rex|phoenixminer|nbminer|gminer|/tmp/|/dev/shm/' | grep -v grep | head -n 80 || ps aux 2>/dev/null | grep -Ei 'xmrig|kdevtmpfsi|kinsing|minergate|cgminer|bfgminer|ethminer|t-rex|phoenixminer|nbminer|gminer|/tmp/|/dev/shm/' | grep -v grep | head -n 80 || true
ss -tunp 2>/dev/null | head -n 120 || netstat -tunp 2>/dev/null | head -n 120 || true
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
if command -v systemctl >/dev/null 2>&1; then
  systemctl list-timers --all --no-pager --no-legend 2>/dev/null | head -n 80
else
  echo "(systemd timers 不可用，非 systemd 系统)"
fi
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

/// 构建历史趋势摘要文本（精简版，供巡检 prompt 注入）。
/// 查询最近 window_hours 的指标，对 cpu/mem/load/disk 各给一行摘要。
fn build_trend_summary(db: &Db, host_id: &str, now_ts: u64, window_h: u64) -> String {
    let since = now_ts.saturating_sub(window_h * 3600);
    let rows = match db.list_metrics(host_id, since, 2000) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    if rows.len() < 3 {
        return String::new();
    }

    let mut out = String::new();

    // CPU
    let cpu_points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.cpu_percent)).collect();
    if let Some(s) = scalar_summary("CPU", "%", &cpu_points) {
        out.push_str(&s);
    }

    // 内存
    let mem_points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.mem_percent)).collect();
    if let Some(s) = scalar_summary("内存", "%", &mem_points) {
        out.push_str(&s);
    }

    // 负载
    let load_points: Vec<(f64, f64)> = rows.iter().map(|r| (r.ts as f64, r.load1)).collect();
    if let Some(s) = scalar_summary("负载", "", &load_points) {
        out.push_str(&s);
    }

    // 磁盘（按挂载点）
    let mut mounts: Vec<String> = Vec::new();
    for r in &rows {
        for d in &r.disks {
            if !mounts.contains(&d.mount) {
                mounts.push(d.mount.clone());
            }
        }
    }
    mounts.sort();
    for mount in &mounts {
        let points: Vec<(f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                r.disks.iter().find(|d| d.mount == *mount).map(|d| (r.ts as f64, d.percent))
            })
            .collect();
        if points.len() < 3 {
            continue;
        }
        if let Some(s) = scalar_summary(&format!("磁盘 {}", mount), "%", &points) {
            out.push_str(&s);
        }
    }

    out
}

/// 单个标量指标的精简摘要：最早→最新、斜率/天、外推。
fn scalar_summary(label: &str, unit: &str, points: &[(f64, f64)]) -> Option<String> {
    if points.len() < 3 {
        return None;
    }
    let first = points[0].1;
    let latest = points.last().unwrap().1;
    let (slope, _) = linear_slope_local(points);
    let slope_per_day = slope * 24.0;

    let mut line = format!("{}: {:.1}{} → {:.1}{}", label, first, unit, latest, unit);
    if slope_per_day.abs() < 0.01 {
        line.push_str("，平稳");
    } else if slope_per_day > 0.0 {
        line.push_str(&format!("，+{:.2}{}/天", slope_per_day, unit));
        if latest < 90.0 && label != "负载" {
            let days = (90.0 - latest) / slope_per_day;
            if days > 0.0 && days < 365.0 {
                line.push_str(&format!("，预计 {:.1} 天后到 90%⚠", days));
            }
        }
    } else {
        line.push_str(&format!("，{:.2}{}/天（下降）", slope_per_day, unit));
    }
    Some(format!("{}\n", line))
}

/// 本地线性回归（避免与 agent.rs 的 private 函数冲突）。
fn linear_slope_local(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    if n < 2.0 {
        return (0.0, 0.0);
    }
    let sum_x: f64 = points.iter().map(|p| p.0).sum();
    let sum_y: f64 = points.iter().map(|p| p.1).sum();
    let sum_xy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let sum_x2: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let denom = n * sum_x2 - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        return (0.0, sum_y / n);
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;
    (slope, intercept)
}
