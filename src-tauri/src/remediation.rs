use crate::credentials;
use crate::db::Db;
use crate::models::{AuditLog, Host, Remediation, RemediationStep, RemediationStepInput};
use crate::russh::RusshManager;
use crate::safety::{is_dangerous, sanitize};
use crate::util::{extract_error, now, truncate, truncate_output};
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
pub struct RemediationManager {
    flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl RemediationManager {
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
pub struct RemediationProgress {
    pub remediation_id: String,
    pub phase: String,
    pub message: String,
    pub step_index: Option<usize>,
    pub total: Option<usize>,
}

#[derive(Clone, Serialize)]
pub struct RemediationDone {
    pub remediation_id: String,
    pub status: String,
}

#[derive(Clone, Serialize)]
pub struct RemediationError {
    pub remediation_id: String,
    pub message: String,
}

#[tauri::command]
pub fn start_remediation_planning(
    app: AppHandle,
    state: State<'_, RemediationManager>,
    report_id: String,
    intervention: String,
) -> Result<String, String> {
    let db = app.state::<Db>();
    let report = db
        .get_inspection_report(&report_id)
        .map_err(|e| format!("读取巡检报告失败: {e}"))?
        .ok_or_else(|| "巡检报告不存在".to_string())?;
    if report.status != "success" {
        return Err("只有成功的巡检报告才能发起整改".to_string());
    }
    let host = db
        .get_host(&report.host_id)
        .map_err(|e| format!("读取主机失败: {e}"))?
        .ok_or_else(|| "巡检报告对应的主机不存在".to_string())?;
    let (provider, model) = crate::ai::resolve_active_ai(&db)?;

    let remediation = Remediation {
        id: uuid::Uuid::new_v4().to_string(),
        report_id: report_id.clone(),
        host_id: host.id.clone(),
        host_label: report.host_label.clone(),
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        model: model.clone(),
        intervention: intervention.trim().to_string(),
        plan_markdown: String::new(),
        steps: Vec::new(),
        status: "planning".to_string(),
        error: None,
        created_at: now(),
        started_at: Some(now()),
        finished_at: None,
        duration_ms: None,
    };
    db.upsert_remediation(&remediation)
        .map_err(|e| format!("创建整改任务失败: {e}"))?;

    let remediation_id = remediation.id.clone();
    let flag = state.register(remediation_id.clone());
    let app_for_task = app.clone();
    let host_for_task = host.clone();
    let report_markdown = report.markdown.clone();
    let provider_id = provider.id.clone();
    let base_url = provider.base_url.clone();
    tauri::async_runtime::spawn(async move {
        run_planning(
            app_for_task,
            remediation,
            host_for_task,
            report_markdown,
            provider_id,
            base_url,
            model,
            flag,
        )
        .await;
    });
    Ok(remediation_id)
}

#[tauri::command]
pub fn get_remediation(
    db: State<'_, Db>,
    report_id: String,
) -> Result<Option<Remediation>, String> {
    db.get_remediation_by_report(&report_id)
        .map_err(|e| format!("读取整改记录失败: {e}"))
}

#[tauri::command]
pub async fn execute_remediation(
    app: AppHandle,
    state: State<'_, RemediationManager>,
    remediation_id: String,
    steps: Vec<RemediationStepInput>,
) -> Result<(), String> {
    let db = app.state::<Db>();
    let mut remediation = db
        .get_remediation(&remediation_id)
        .map_err(|e| format!("读取整改记录失败: {e}"))?
        .ok_or_else(|| "整改记录不存在".to_string())?;
    if remediation.status != "plan_ready" {
        return Err("当前整改记录不可执行".to_string());
    }

    let full_steps = normalize_steps(steps)?;
    if full_steps.is_empty() {
        return Err("没有可执行的整改步骤".to_string());
    }
    let host = db
        .get_host(&remediation.host_id)
        .map_err(|e| format!("读取主机失败: {e}"))?
        .ok_or_else(|| "整改记录对应的主机不存在".to_string())?;

    remediation.steps = full_steps;
    remediation.status = "executing".to_string();
    remediation.error = None;
    remediation.started_at = Some(now());
    remediation.finished_at = None;
    remediation.duration_ms = None;
    db.update_remediation(&remediation)
        .map_err(|e| format!("更新整改记录失败: {e}"))?;

    let flag = state.register(remediation_id.clone());
    let app_for_task = app.clone();
    let host_for_task = host.clone();
    tauri::async_runtime::spawn(async move {
        run_execution(app_for_task, remediation, host_for_task, flag).await;
    });
    Ok(())
}

#[tauri::command]
pub fn cancel_remediation(
    state: State<'_, RemediationManager>,
    remediation_id: String,
) -> Result<(), String> {
    if state.cancel(&remediation_id) {
        Ok(())
    } else {
        Err("整改任务不存在或已完成".to_string())
    }
}

#[tauri::command]
pub async fn retry_remediation(
    app: AppHandle,
    state: State<'_, RemediationManager>,
    remediation_id: String,
) -> Result<(), String> {
    let db = app.state::<Db>();
    let mut remediation = db
        .get_remediation(&remediation_id)
        .map_err(|e| format!("读取整改记录失败: {e}"))?
        .ok_or_else(|| "整改记录不存在".to_string())?;
    if !matches!(
        remediation.status.as_str(),
        "failed" | "cancelled" | "success"
    ) {
        return Err("当前整改记录不可重新执行".to_string());
    }
    let host = db
        .get_host(&remediation.host_id)
        .map_err(|e| format!("读取主机失败: {e}"))?
        .ok_or_else(|| "整改记录对应的主机不存在".to_string())?;

    for step in &mut remediation.steps {
        step.status = "pending".to_string();
        step.output = None;
    }
    remediation.status = "executing".to_string();
    remediation.error = None;
    remediation.started_at = Some(now());
    remediation.finished_at = None;
    remediation.duration_ms = None;
    db.update_remediation(&remediation)
        .map_err(|e| format!("更新整改记录失败: {e}"))?;

    let flag = state.register(remediation_id.clone());
    let app_for_task = app.clone();
    let host_for_task = host.clone();
    tauri::async_runtime::spawn(async move {
        run_execution(app_for_task, remediation, host_for_task, flag).await;
    });
    Ok(())
}

async fn run_planning(
    app: AppHandle,
    mut remediation: Remediation,
    host: Host,
    report_markdown: String,
    provider_id: String,
    base_url: String,
    model: String,
    flag: Arc<AtomicBool>,
) {
    let remediation_id = remediation.id.clone();

    if cancelled(&flag) {
        remediation.status = "cancelled".to_string();
        remediation.error = Some("用户取消".to_string());
        let _ = app.state::<Db>().update_remediation(&remediation);
        let _ = app.emit(
            "remediation:done",
            RemediationDone {
                remediation_id: remediation_id.clone(),
                status: "cancelled".to_string(),
            },
        );
        let _ = app.state::<RemediationManager>().unregister(&remediation_id);
        return;
    }

    let result = plan_remediation(
        &app,
        &mut remediation,
        &host,
        &report_markdown,
        &provider_id,
        &base_url,
        &model,
        &flag,
    )
    .await;

    if cancelled(&flag) {
        remediation.status = "cancelled".to_string();
        remediation.error = Some("用户取消".to_string());
        let _ = app.state::<Db>().update_remediation(&remediation);
        let _ = app.emit(
            "remediation:done",
            RemediationDone {
                remediation_id: remediation_id.clone(),
                status: "cancelled".to_string(),
            },
        );
        let _ = app.state::<RemediationManager>().unregister(&remediation_id);
        return;
    }

    match result {
        Ok(()) => {
            remediation.status = "plan_ready".to_string();
            remediation.error = None;
            let _ = app.state::<Db>().update_remediation(&remediation);
            let _ = app.emit(
                "remediation:done",
                RemediationDone {
                    remediation_id: remediation_id.clone(),
                    status: "plan_ready".to_string(),
                },
            );
        }
        Err(message) => {
            remediation.status = "failed".to_string();
            remediation.error = Some(message.clone());
            let _ = app.state::<Db>().update_remediation(&remediation);
            let _ = app.emit(
                "remediation:error",
                RemediationError {
                    remediation_id: remediation_id.clone(),
                    message,
                },
            );
        }
    }
    let _ = app.state::<RemediationManager>().unregister(&remediation_id);
}

async fn plan_remediation(
    app: &AppHandle,
    remediation: &mut Remediation,
    host: &Host,
    report_markdown: &str,
    provider_id: &str,
    base_url: &str,
    model: &str,
    flag: &Arc<AtomicBool>,
) -> Result<(), String> {
    emit_progress(
        app,
        &remediation.id,
        "planning",
        "AI 正在生成整改步骤",
        None,
        None,
    );

    let api_key = credentials::get_api_key(provider_id)
        .ok_or_else(|| "API Key 未找到，请在 AI 配置中检查".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let intervention = if remediation.intervention.trim().is_empty() {
        "（无）".to_string()
    } else {
        remediation.intervention.clone()
    };
    let messages = vec![
        serde_json::json!({"role": "system", "content": remediation_system_prompt(host)}),
        serde_json::json!({
            "role": "user",
            "content": format!(
                "请基于下面这份巡检报告，结合用户的整改干预意见，生成可执行的整改步骤。\n\n\
                 巡检报告：\n{}\n\n\
                 用户整改干预意见：\n{}\n\n\
                 只输出 JSON。",
                truncate(report_markdown, 24000),
                intervention
            )
        }),
    ];

    if cancelled(flag) {
        return Ok(());
    }
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": false,
    });
    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
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
    let content = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if content.trim().is_empty() {
        return Err("AI 未返回整改步骤".to_string());
    }

    let plan = extract_json_object(&content)?;
    let plan: AiPlan = serde_json::from_value(plan)
        .map_err(|e| format!("整改步骤 JSON 结构无效: {e}"))?;
    if plan.steps.is_empty() {
        return Err("AI 未返回任何整改步骤".to_string());
    }

    let mut steps = Vec::new();
    for step in plan.steps {
        let command = step.command.trim().to_string();
        if command.is_empty() {
            return Err("整改步骤包含空命令".to_string());
        }
        if command.chars().count() > 2000 {
            return Err("整改步骤命令过长".to_string());
        }
        let description = step
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| command.clone());
        let timeout_secs = step.timeout_secs.unwrap_or(60).clamp(5, 600);
        steps.push(RemediationStep {
            id: uuid::Uuid::new_v4().to_string(),
            description,
            command: command.clone(),
            timeout_secs,
            dangerous: is_dangerous(&command),
            status: "pending".to_string(),
            output: None,
        });
    }

    remediation.plan_markdown = plan.summary.unwrap_or_else(|| "整改步骤".to_string());
    remediation.steps = steps;
    Ok(())
}

async fn run_execution(
    app: AppHandle,
    mut remediation: Remediation,
    host: Host,
    flag: Arc<AtomicBool>,
) {
    let remediation_id = remediation.id.clone();
    let started = Instant::now();
    let db = app.state::<Db>();
    let russh = app.state::<RusshManager>();
    let total = remediation.steps.len();

    for index in 0..remediation.steps.len() {
        if cancelled(&flag) {
            remediation.status = "cancelled".to_string();
            remediation.error = Some("用户取消".to_string());
            remediation.finished_at = Some(now());
            remediation.duration_ms = Some(started.elapsed().as_millis() as u64);
            let _ = db.update_remediation(&remediation);
            let _ = app.emit(
                "remediation:done",
                RemediationDone {
                    remediation_id: remediation_id.clone(),
                    status: "cancelled".to_string(),
                },
            );
            let _ = app.state::<RemediationManager>().unregister(&remediation_id);
            return;
        }

        remediation.steps[index].status = "running".to_string();
        remediation.steps[index].output = None;
        let _ = db.update_remediation(&remediation);
        let description = remediation.steps[index].description.clone();
        emit_progress(
            &app,
            &remediation_id,
            "step_start",
            &format!("执行步骤 {}：{}", index + 1, description),
            Some(index),
            Some(total),
        );

        let command = remediation.steps[index].command.clone();
        let timeout_secs = remediation.steps[index].timeout_secs;
        let step_started = Instant::now();
        let result = russh
            .exec(&host, &command, Duration::from_secs(timeout_secs))
            .await;
        let duration_ms = step_started.elapsed().as_millis() as u64;

        match result {
            Ok(out) => {
                let mut text = truncate_output(&sanitize(&out.text), 8000);
                if out.timed_out {
                    text.push_str("\n[命令执行超时，输出可能不完整]");
                }
                if let Some(code) = out.exit_code {
                    if code != 0 {
                        text.push_str(&format!("\n[退出码 {code}]"));
                    }
                }
                let success =
                    !out.timed_out && out.exit_code.map(|c| c == 0).unwrap_or(true);
                remediation.steps[index].output = Some(text.clone());
                if success {
                    remediation.steps[index].status = "success".to_string();
                    let _ = db.update_remediation(&remediation);
                    insert_remediation_audit(
                        &db,
                        &host,
                        &command,
                        "executed",
                        Some(text.clone()),
                        duration_ms,
                    );
                    emit_progress(
                        &app,
                        &remediation_id,
                        "step_success",
                        &format!("步骤 {} 完成", index + 1),
                        Some(index),
                        Some(total),
                    );
                } else {
                    remediation.steps[index].status = "error".to_string();
                    let _ = db.update_remediation(&remediation);
                    insert_remediation_audit(
                        &db,
                        &host,
                        &command,
                        "error",
                        Some(text.clone()),
                        duration_ms,
                    );
                    let message = format!("步骤 {} 执行失败", index + 1);
                    remediation.status = "failed".to_string();
                    remediation.error = Some(message.clone());
                    remediation.finished_at = Some(now());
                    remediation.duration_ms = Some(started.elapsed().as_millis() as u64);
                    let _ = db.update_remediation(&remediation);
                    notify_remediation_result(&app, &remediation).await;
                    emit_progress(
                        &app,
                        &remediation_id,
                        "step_error",
                        &message,
                        Some(index),
                        Some(total),
                    );
                    let _ = app.emit(
                        "remediation:error",
                        RemediationError {
                            remediation_id: remediation_id.clone(),
                            message,
                        },
                    );
                    let _ = app.state::<RemediationManager>().unregister(&remediation_id);
                    return;
                }
            }
            Err(err) => {
                remediation.steps[index].status = "error".to_string();
                remediation.steps[index].output = Some(err.clone());
                let _ = db.update_remediation(&remediation);
                insert_remediation_audit(
                    &db,
                    &host,
                    &command,
                    "error",
                    Some(err.clone()),
                    duration_ms,
                );
                let message = format!("步骤 {} 执行失败: {}", index + 1, err);
                remediation.status = "failed".to_string();
                remediation.error = Some(message.clone());
                remediation.finished_at = Some(now());
                remediation.duration_ms = Some(started.elapsed().as_millis() as u64);
                let _ = db.update_remediation(&remediation);
                notify_remediation_result(&app, &remediation).await;
                emit_progress(
                    &app,
                    &remediation_id,
                    "step_error",
                    &message,
                    Some(index),
                    Some(total),
                );
                let _ = app.emit(
                    "remediation:error",
                    RemediationError {
                        remediation_id: remediation_id.clone(),
                        message,
                    },
                );
                let _ = app.state::<RemediationManager>().unregister(&remediation_id);
                return;
            }
        }
    }

    remediation.status = "success".to_string();
    remediation.error = None;
    remediation.finished_at = Some(now());
    remediation.duration_ms = Some(started.elapsed().as_millis() as u64);
    let _ = db.update_remediation(&remediation);
    notify_remediation_result(&app, &remediation).await;
    let _ = app.emit(
        "remediation:done",
        RemediationDone {
            remediation_id: remediation_id.clone(),
            status: "success".to_string(),
        },
    );
    let _ = app.state::<RemediationManager>().unregister(&remediation_id);
}

fn insert_remediation_audit(
    db: &Db,
    host: &Host,
    command: &str,
    status: &str,
    result: Option<String>,
    duration_ms: u64,
) {
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: None,
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: "remediation".to_string(),
        summary: truncate(command, 500),
        permission_mode: "remediation".to_string(),
        approval: "confirmed".to_string(),
        status: status.to_string(),
        result: result.map(|r| truncate(&r, 300)),
        duration_ms: Some(duration_ms),
    };
    let _ = db.insert_audit_log(&log);
}

async fn notify_remediation_result(app: &AppHandle, remediation: &Remediation) {
    let db = app.state::<Db>();
    let settings = match db.get_alert_settings() {
        Ok(settings) => settings,
        Err(_) => return,
    };
    if settings.smtp_host.as_deref().map(|s| !s.trim().is_empty()) != Some(true)
        || settings.smtp_to.as_deref().map(|s| !s.trim().is_empty()) != Some(true)
    {
        return;
    }
    let status_label = remediation_status_label(&remediation.status);
    let subject = format!(
        "[KeyWisp 整改] {} - {}",
        remediation.host_label, status_label
    );
    let html = build_remediation_email_html(remediation, status_label);
    let _ = crate::alert::send_html_email(&settings, &subject, &html).await;
}

fn remediation_status_label(status: &str) -> &str {
    match status {
        "success" => "整改完成",
        "failed" => "整改失败",
        "cancelled" => "已取消",
        "executing" => "执行中",
        "plan_ready" => "待执行",
        "planning" => "生成中",
        _ => "未知状态",
    }
}

fn build_remediation_email_html(remediation: &Remediation, status_label: &str) -> String {
    let status_color = match remediation.status.as_str() {
        "success" => "#30a46c",
        "failed" => "#e5484d",
        "cancelled" => "#f5a623",
        _ => "#8b8d98",
    };
    let mut steps_html = String::new();
    for (index, step) in remediation.steps.iter().enumerate() {
        let step_status = match step.status.as_str() {
            "success" => "已完成",
            "error" => "失败",
            "running" => "执行中",
            _ => "待执行",
        };
        let output = step
            .output
            .as_deref()
            .map(|o| format!("<pre style=\"white-space:pre-wrap;background:#f4f5f7;padding:10px;border-radius:8px;font-size:12px;margin:8px 0 0;color:#333;\">{}</pre>", esc_html(&truncate(o, 800))))
            .unwrap_or_default();
        steps_html.push_str(&format!(
            "<div style=\"border:1px solid #e5e7eb;border-radius:10px;padding:12px 14px;margin-top:12px;\">\
             <div style=\"font-weight:650;margin-bottom:4px;\">步骤 {}：{}</div>\
             <div style=\"color:#6b7280;font-size:13px;margin-bottom:8px;\">{}</div>\
             <code style=\"font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12px;background:#f1f3f5;padding:3px 7px;border-radius:5px;word-break:break-all;\">{}</code>\
             {}</div>",
            index + 1,
            step_status,
            esc_html(&step.description),
            esc_html(&step.command),
            output
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><style>table{{border-collapse:collapse;width:100%;}}th,td{{border:1px solid #e5e7eb;padding:8px 10px;text-align:left;}}code{{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;}}</style></head>\
         <body style=\"margin:0;padding:24px;background:#f4f5f7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1f2430;\">\
         <div style=\"max-width:900px;margin:0 auto;background:#ffffff;border-radius:14px;overflow:hidden;box-shadow:0 8px 24px rgba(15,23,42,.08);\">\
         <div style=\"padding:24px 28px;background:linear-gradient(135deg,#2f3b52,#4f5d78);color:#ffffff;\">\
         <div style=\"font-size:13px;opacity:.75;\">KeyWisp Agent Ops · 一键整改结果</div>\
         <div style=\"font-size:22px;font-weight:700;margin-top:4px;\">{}</div>\
         <div style=\"margin-top:10px;font-size:13px;\">执行结果：<span style=\"display:inline-block;padding:2px 10px;border-radius:999px;background:{};color:#fff;\">{}</span></div>\
         </div>\
         <div style=\"padding:24px 28px;line-height:1.65;font-size:14px;\">{}{}</div>\
         </div></body></html>",
        esc_html(&remediation.host_label),
        status_color,
        status_label,
        if remediation.plan_markdown.trim().is_empty() {
            String::new()
        } else {
            format!("<p style=\"color:#374151;\">{}</p>", esc_html(&remediation.plan_markdown))
        },
        steps_html
    )
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn emit_progress(
    app: &AppHandle,
    remediation_id: &str,
    phase: &str,
    message: &str,
    step_index: Option<usize>,
    total: Option<usize>,
) {
    let _ = app.emit(
        "remediation:progress",
        RemediationProgress {
            remediation_id: remediation_id.to_string(),
            phase: phase.to_string(),
            message: message.to_string(),
            step_index,
            total,
        },
    );
}

fn cancelled(flag: &Arc<AtomicBool>) -> bool {
    flag.load(Ordering::SeqCst)
}

fn remediation_system_prompt(host: &Host) -> String {
    format!(
        "你是 KeyWisp Agent Ops 的服务器整改执行专家。\n\
         当前服务器：{}（{}@{}:{}）\n\
         你只负责基于巡检报告和用户意见生成可执行的整改步骤，不执行任何命令。\n\
         请只输出 JSON，结构为：{{\"summary\":\"整改说明\",\"steps\":[{{\"description\":\"步骤说明\",\"command\":\"要执行的 shell 命令\",\"timeout_secs\":60}}]}}。\n\
         步骤要具体、安全、按依赖顺序排列；避免破坏性操作，除非报告或用户明确要求。\n\
         仅针对巡检报告与用户意见中明确指出的真实差距生成步骤；若某项配置已经满足目标，不要生成重复或无效的整改步骤。",
        host.name,
        host.username,
        host.address,
        host.port
    )
}

fn extract_json_object(text: &str) -> Result<serde_json::Value, String> {
    let mut cleaned = text.trim().trim_start_matches("```").trim();
    cleaned = cleaned
        .strip_prefix("json")
        .map(|s| s.trim())
        .unwrap_or(cleaned);
    cleaned = cleaned.trim_end_matches("```").trim();
    let start = cleaned
        .find('{')
        .ok_or_else(|| "AI 未返回 JSON 对象".to_string())?;
    let end = cleaned
        .rfind('}')
        .ok_or_else(|| "AI 未返回完整的 JSON 对象".to_string())?;
    serde_json::from_str(&cleaned[start..=end])
        .map_err(|e| format!("解析整改步骤 JSON 失败: {e}"))
}

fn normalize_steps(input: Vec<RemediationStepInput>) -> Result<Vec<RemediationStep>, String> {
    let mut steps = Vec::new();
    for step in input {
        let command = step.command.trim().to_string();
        if command.is_empty() {
            continue;
        }
        if command.chars().count() > 2000 {
            return Err("整改步骤命令过长".to_string());
        }
        let description = if step.description.trim().is_empty() {
            command.clone()
        } else {
            step.description.trim().to_string()
        };
        let timeout_secs = step.timeout_secs.clamp(5, 600);
        steps.push(RemediationStep {
            id: uuid::Uuid::new_v4().to_string(),
            description,
            command: command.clone(),
            timeout_secs,
            dangerous: is_dangerous(&command),
            status: "pending".to_string(),
            output: None,
        });
    }
    Ok(steps)
}

#[derive(Deserialize)]
struct AiPlan {
    summary: Option<String>,
    steps: Vec<AiPlanStep>,
}

#[derive(Deserialize)]
struct AiPlanStep {
    description: Option<String>,
    command: String,
    timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_json_object() {
        let value = extract_json_object(r#"{"summary":"x","steps":[]}"#).unwrap();
        assert_eq!(value["summary"], "x");
    }

    #[test]
    fn extracts_fenced_json_object() {
        let value = extract_json_object(
            "```json\n{\"summary\":\"x\",\"steps\":[]}\n```",
        )
        .unwrap();
        assert_eq!(value["summary"], "x");
    }

    #[test]
    fn normalizes_steps_with_defaults() {
        let steps = normalize_steps(vec![
            RemediationStepInput {
                description: "  ".to_string(),
                command: "  systemctl restart nginx  ".to_string(),
                timeout_secs: 9999,
            },
            RemediationStepInput {
                description: "查看时间".to_string(),
                command: "date".to_string(),
                timeout_secs: 2,
            },
        ])
        .unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].description, "systemctl restart nginx");
        assert_eq!(steps[0].timeout_secs, 600);
        assert!(steps[0].dangerous);
        assert_eq!(steps[1].timeout_secs, 5);
        assert!(!steps[1].dangerous);
    }
}
