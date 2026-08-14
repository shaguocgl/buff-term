use crate::credentials;
use crate::db::Db;
use crate::models::{AiProvider, AuditLog, Host, PermissionMode};
use crate::russh::RusshManager;
use crate::safety::{is_dangerous, normalize_tool, sanitize};
use crate::session::SessionManager;
use crate::util::{extract_error, format_exec_output, now, shq, truncate};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

#[derive(Default)]
pub struct AgentManager {
    controls: Mutex<HashMap<u32, mpsc::Sender<Control>>>,
    histories: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    generations: Mutex<HashMap<String, u64>>,
}

pub enum Control {
    Approve {
        tool_call_id: String,
        allow: bool,
    },
    Cancel,
}

impl AgentManager {
    fn set_control(&self, id: u32, tx: mpsc::Sender<Control>) {
        self.controls.lock().unwrap().insert(id, tx);
    }

    fn clear_control(&self, id: u32) {
        self.controls.lock().unwrap().remove(&id);
    }

    fn history(&self, host_id: &str) -> Vec<serde_json::Value> {
        self.histories
            .lock()
            .unwrap()
            .get(host_id)
            .cloned()
            .unwrap_or_default()
    }

    fn save_history(&self, host_id: &str, history: Vec<serde_json::Value>) {
        self.histories
            .lock()
            .unwrap()
            .insert(host_id.to_string(), history);
    }

    pub(crate) fn clear_history(&self, host_id: &str) {
        self.histories.lock().unwrap().remove(host_id);
    }

    /// 当前主机的历史代数：每次 reset 递增，用于让正在运行的循环在结束时放弃写回旧历史。
    fn generation(&self, host_id: &str) -> u64 {
        *self.generations.lock().unwrap().get(host_id).unwrap_or(&0)
    }

    fn bump_generation(&self, host_id: &str) {
        *self.generations.lock().unwrap().entry(host_id.to_string()).or_insert(0) += 1;
    }
}

#[derive(Clone, Serialize)]
pub struct AiStream {
    pub session_id: u32,
    pub delta: String,
}

#[derive(Clone, Serialize)]
pub struct AiTool {
    pub session_id: u32,
    pub tool_call_id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub state: String,
    pub output: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct AiDone {
    pub session_id: u32,
}

#[derive(Clone, Serialize)]
pub struct AiError {
    pub session_id: u32,
    pub message: String,
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    db: State<'_, Db>,
    sessions: State<'_, SessionManager>,
    agents: State<'_, AgentManager>,
    session_id: u32,
    message: String,
    permission_mode: PermissionMode,
) -> Result<(), String> {
    let host = sessions
        .host(session_id)
        .ok_or_else(|| "会话不存在或已断开".to_string())?;
    let (provider, model) = crate::ai::resolve_active_ai(&db)?;
    eprintln!("[agent] 使用模型: {}（{}）", model, provider.name);
    let api_key = credentials::get_api_key(&provider.id)
        .ok_or_else(|| "API Key 未找到，请在 AI 配置中检查".to_string())?;
    let danger_rules: Vec<String> = db
        .list_ai_rules()
        .map_err(|e| format!("读取智能审核规则失败: {e}"))?
        .into_iter()
        .map(|r| r.pattern)
        .collect();
    let russh = app.state::<RusshManager>();
    let (tx, rx) = mpsc::channel::<Control>(8);
    agents.set_control(session_id, tx);
    let generation = agents.generation(&host.id);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    let mut history = agents.history(&host.id);
    let system = system_prompt(&host, &provider, &model);
    if history.is_empty() {
        history.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    } else if let Some(first) = history.first_mut() {
        // 会话中途切换平台/模型时，同步刷新系统提示词中的身份描述
        if first.get("role").and_then(|r| r.as_str()) == Some("system") {
            first["content"] = serde_json::json!(system);
        }
    }
    history.push(serde_json::json!({"role": "user", "content": message}));

    let result = run_agent_loop(
        &app,
        &client,
        &url,
        &api_key,
        &model,
        &host,
        session_id,
        permission_mode,
        &danger_rules,
        &db,
        &russh,
        rx,
        &mut history,
    )
    .await;

    if agents.generation(&host.id) == generation {
        trim_history(&mut history, MAX_HISTORY_ROUNDS);
        agents.save_history(&host.id, history);
    }
    agents.clear_control(session_id);
    result
}

#[tauri::command]
pub fn agent_approve(
    agents: State<'_, AgentManager>,
    session_id: u32,
    tool_call_id: String,
    allow: bool,
) -> Result<(), String> {
    let tx = agents
        .controls
        .lock()
        .unwrap()
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "当前没有等待审批的工具调用".to_string())?;
    tx.try_send(Control::Approve {
        tool_call_id,
        allow,
    })
    .map_err(|_| "会话已结束".to_string())
}

#[tauri::command]
pub fn agent_cancel(agents: State<'_, AgentManager>, session_id: u32) -> Result<(), String> {
    if let Some(tx) = agents.controls.lock().unwrap().get(&session_id).cloned() {
        let _ = tx.try_send(Control::Cancel);
    }
    Ok(())
}

#[tauri::command]
pub fn agent_reset(
    agents: State<'_, AgentManager>,
    session_id: u32,
    host_id: String,
) -> Result<(), String> {
    // 停止正在运行的 agent 循环（若有），并递增代数使旧循环在结束时放弃写回历史
    if let Some(tx) = agents.controls.lock().unwrap().get(&session_id).cloned() {
        let _ = tx.try_send(Control::Cancel);
    }
    agents.bump_generation(&host_id);
    agents.clear_history(&host_id);
    Ok(())
}

#[tauri::command]
pub fn get_history(
    agents: State<'_, AgentManager>,
    host_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(agents.history(&host_id))
}

async fn run_agent_loop(
    app: &AppHandle,
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    model: &str,
    host: &Host,
    session_id: u32,
    permission_mode: PermissionMode,
    danger_rules: &[String],
    db: &Db,
    russh: &RusshManager,
    mut rx: mpsc::Receiver<Control>,
    history: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    let mut iterations = 0;
    loop {
        iterations += 1;
        if iterations > 12 {
            let msg = "工具调用次数过多，已停止".to_string();
            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
            return Err(msg);
        }
        if let Ok(Control::Cancel) = rx.try_recv() {
            return Ok(());
        }

        let body = serde_json::json!({
            "model": model,
            "messages": history,
            "stream": true,
            "tools": tools_schema(),
        });
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                let msg = format!("请求 AI 平台失败: {e}");
                let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
                msg
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = extract_error(&text, status);
            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
            return Err(msg);
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut content = String::new();
        let mut tool_calls: HashMap<usize, ToolCallAcc> = HashMap::new();
        let mut done = false;

        while let Some(chunk) = stream.next().await {
            if let Ok(Control::Cancel) = rx.try_recv() {
                return Ok(());
            }
            let chunk = chunk.map_err(|e| format!("读取响应流失败: {e}"))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf = buf[pos + 1..].to_string();
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line[5..].trim();
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let value: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                apply_delta(
                    &value["choices"][0]["delta"],
                    &mut content,
                    &mut tool_calls,
                    app,
                    session_id,
                );
            }
            if done {
                break;
            }
        }
        // 处理流结束时缓冲区中残留的未换行数据，避免丢失最后一段内容
        if !buf.is_empty() {
            let data = buf.trim();
            if let Some(payload) = data.strip_prefix("data:") {
                let payload = payload.trim();
                if payload != "[DONE]" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                        apply_delta(
                            &value["choices"][0]["delta"],
                            &mut content,
                            &mut tool_calls,
                            app,
                            session_id,
                        );
                    }
                }
            }
        }

        // 模型漏填工具名时，从参数推断（command → exec_command，path → read_file）
        for (_, acc) in tool_calls.iter_mut() {
            if acc.name.trim().is_empty() {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&acc.args) {
                    if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
                        if !cmd.trim().is_empty() {
                            acc.name = "exec_command".to_string();
                        }
                    } else if args.get("path").and_then(|p| p.as_str()).is_some() {
                        acc.name = "read_file".to_string();
                    }
                }
            }
        }

        // 丢弃空壳工具调用：既没有工具名也没有任何有效参数（部分模型会输出空的占位调用），
        // 这类调用既无法推断也不能执行，直接忽略，避免回填“未知工具”错误让模型原地打转
        tool_calls.retain(|_, acc| {
            let has_name = !acc.name.trim().is_empty();
            let has_args = serde_json::from_str::<serde_json::Value>(&acc.args)
                .map(|v| v.as_object().map(|m| !m.is_empty()).unwrap_or(false))
                .unwrap_or(false);
            has_name || has_args
        });

        if tool_calls.is_empty() {
            if content.is_empty() {
                content = "（模型未返回内容）".to_string();
            }
            history.push(serde_json::json!({"role": "assistant", "content": content}));
            let _ = app.emit("ai:done", AiDone { session_id });
            return Ok(());
        }

        let mut calls_json = Vec::new();
        for (_, acc) in tool_calls.iter() {
            calls_json.push(serde_json::json!({
                "id": acc.id,
                "type": "function",
                "function": { "name": acc.name, "arguments": acc.args },
            }));
        }
        history.push(serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls_json,
        }));

        for (_, acc) in tool_calls {
            if let Ok(Control::Cancel) = rx.try_recv() {
                return Ok(());
            }
            let started = Instant::now();
            let args = parse_args(&acc.args);

            let need_approval = match permission_mode {
                PermissionMode::All => true,
                PermissionMode::None => false,
                PermissionMode::Smart => {
                    if acc.name != "exec_command" {
                        false
                    } else {
                        let marked = args
                            .get("requires_approval")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let command = args
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        let c = command.to_ascii_lowercase();
                        marked
                            || is_dangerous(command)
                            || danger_rules
                                .iter()
                                .any(|p| !p.trim().is_empty() && c.contains(&p.to_ascii_lowercase()))
                    }
                }
            };

            if need_approval {
                let _ = app.emit(
                    "ai:tool",
                    AiTool {
                        session_id,
                        tool_call_id: acc.id.clone(),
                        name: acc.name.clone(),
                        args: args.clone(),
                        state: "request".to_string(),
                        output: None,
                    },
                );

                let decision = loop {
                    match tokio::time::timeout(Duration::from_secs(600), rx.recv()).await {
                        Err(_) => {
                            let msg = "等待审批超时".to_string();
                            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
                            return Err(msg);
                        }
                        Ok(None) => {
                            let msg = "会话已结束".to_string();
                            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
                            return Err(msg);
                        }
                        Ok(Some(Control::Cancel)) => return Ok(()),
                        Ok(Some(Control::Approve { tool_call_id, allow }))
                            if tool_call_id == acc.id =>
                        {
                            break allow;
                        }
                        Ok(Some(Control::Approve { .. })) => continue,
                    }
                };

                if !decision {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode.as_str(),
                        "denied",
                        "denied",
                        None,
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "denied".to_string(),
                            output: None,
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": "用户拒绝执行该操作",
                    }));
                    continue;
                }
            }

            let approval_label = if need_approval { "approved" } else { "auto" };
            let _ = app.emit(
                "ai:tool",
                AiTool {
                    session_id,
                    tool_call_id: acc.id.clone(),
                    name: acc.name.clone(),
                    args: args.clone(),
                    state: "running".to_string(),
                    output: None,
                },
            );

            let result = execute_tool(russh, host, &acc.name, &args).await;
            match result {
                Ok(output) => {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode.as_str(),
                        approval_label,
                        "executed",
                        Some(output.clone()),
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "result".to_string(),
                            output: Some(output.clone()),
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": output,
                    }));
                }
                Err(err) => {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode.as_str(),
                        approval_label,
                        "error",
                        Some(err.clone()),
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "error".to_string(),
                            output: Some(err.clone()),
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": format!("执行失败: {err}"),
                    }));
                }
            }
        }
    }
}

fn insert_audit(
    db: &Db,
    session_id: u32,
    host: &Host,
    acc: &ToolCallAcc,
    args: &serde_json::Value,
    permission_mode: &str,
    approval: &str,
    status: &str,
    result: Option<String>,
    duration_ms: u64,
) -> Result<(), String> {
    let summary = args
        .get("command")
        .and_then(|c| c.as_str())
        .map(String::from)
        .unwrap_or_else(|| serde_json::to_string(args).unwrap_or_default());
    let result = result.map(|r| truncate(&r, 300));
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: Some(session_id),
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: acc.name.clone(),
        summary: truncate(&summary, 500),
        permission_mode: permission_mode.to_string(),
        approval: approval.to_string(),
        status: status.to_string(),
        result,
        duration_ms: Some(duration_ms),
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作日志失败: {e}"))
}

fn apply_delta(
    delta: &serde_json::Value,
    content: &mut String,
    tool_calls: &mut HashMap<usize, ToolCallAcc>,
    app: &AppHandle,
    session_id: u32,
) {
    if let Some(t) = delta["content"].as_str() {
        content.push_str(t);
        let _ = app.emit(
            "ai:stream",
            AiStream {
                session_id,
                delta: t.to_string(),
            },
        );
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            let acc = tool_calls.entry(index).or_default();
            if let Some(id) = call["id"].as_str() {
                if acc.id.is_empty() {
                    acc.id = id.to_string();
                }
            }
            // 兼容两种流式格式：function 嵌套（OpenAI 风格）与顶层 name/arguments（部分模型）
            let name = call["function"]["name"]
                .as_str()
                .or_else(|| call["name"].as_str());
            if let Some(name) = name {
                acc.name = name.to_string();
            }
            let args = call["function"]["arguments"]
                .as_str()
                .or_else(|| call["arguments"].as_str());
            if let Some(args) = args {
                acc.args.push_str(args);
            }
        }
    }
}

async fn execute_tool(
    russh: &RusshManager,
    host: &Host,
    name: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    match name {
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| "缺少 command 参数".to_string())?;
            let timeout = args.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(30);
            let out = russh
                .exec(host, command, std::time::Duration::from_secs(timeout))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "缺少 path 参数".to_string())?;
            let out = russh
                .exec(host, &format!("cat {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let out = russh
                .exec(host, &format!("ls -lah {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "resource_usage" => {
            let script = "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8";
            let out = russh
                .exec(host, script, std::time::Duration::from_secs(25))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        _ => {
            let mut effective = name;
            // 模型漏填工具名时，根据参数推断（command → exec_command，path → read_file）
            if effective.trim().is_empty() {
                if args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    effective = "exec_command";
                } else if args.get("path").and_then(|p| p.as_str()).is_some() {
                    effective = "read_file";
                }
            }
            let normalized = normalize_tool(effective);
            if normalized != name {
                return Box::pin(execute_tool(russh, host, normalized, args)).await;
            }
            eprintln!("[agent] 未知工具调用: {name}，参数: {args}");
            Err(format!(
                "未知工具: {name}。可用工具：exec_command（执行命令）、read_file（读文件）、\
                 list_dir（列目录）、resource_usage（资源占用）。\
                 请改用这些工具重试。"
            ))
        }
    }
}

fn parse_args(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ "command": raw.trim() }),
    }
}

fn system_prompt(host: &Host, provider: &AiProvider, model: &str) -> String {
    format!(
        "你是 KeyWisp Agent，运行在用户本地的 SSH 管理工具中，帮助用户管理远程服务器。\n\
         当前由 {} 平台提供能力，当前配置的底层模型是 {}。\n\
         当前连接的服务器：{}（{}@{}:{}）\n\
         可用工具：exec_command（执行命令）、read_file（读文件）、list_dir（列目录）、resource_usage（资源占用）。\n\
         规则：\n\
         1. 所有 exec_command 都会经过用户批准，获批后才执行，请先说明意图。\n\
         2. 命令输出可能被截断，只基于已有信息回答，不要编造。\n\
         3. 遇到破坏性操作（删除、格式化、改权限、停服务等）时，明确提示风险并给出命令原文。\n\
         4. 使用中文回答，简洁、专业、有条理。\n\
         5. 身份说明：当用户询问“你是什么模型/你由谁开发”时，如实回答你由 {} 驱动、配置的模型为 {}，
            以及你是 KeyWisp Agent；不要声称自己是任何其他 AI 助手（如 ChatGPT、Claude、Gemini 等），
            也不要编造版本号或开发厂商信息。\n\
         6. 工具调用约定：工具名称必须是以下之一——exec_command、read_file、list_dir、resource_usage；
            每次工具调用都必须包含完整的 name 字段且不能为空，不要发明新工具名；参数放入 arguments（JSON 对象）。",
        provider.name,
        model,
        host.name,
        host.username,
        host.address,
        host.port,
        provider.name,
        model
    )
}

fn tools_schema() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "exec_command",
                "description": "在远程服务器上执行一条 shell 命令并返回输出。默认超时 30 秒。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout_secs": { "type": "number", "description": "超时秒数，默认 30" },
                        "requires_approval": { "type": "boolean", "description": "如果你认为该命令有危险（删除、格式化、修改系统状态、影响服务等），设为 true，便于用户安全策略处理" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取远程服务器上的文件内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "远程文件路径" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出远程服务器上的目录内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "目录路径，默认当前目录" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "resource_usage",
                "description": "查看服务器磁盘、内存、负载和 CPU/内存占用最高的进程",
                "parameters": { "type": "object", "properties": {} }
            }
        },
    ])
}

/// 每台主机最多保留的对话轮数（一轮 = 一次用户提问）。
const MAX_HISTORY_ROUNDS: usize = 20;

/// 裁剪对话历史，最多保留最近 `max_rounds` 轮（一轮 = 一次 user 消息及其后续 assistant/tool 消息），
/// 始终保留首条 system 提示词。
fn trim_history(history: &mut Vec<serde_json::Value>, max_rounds: usize) {
    if history.is_empty() {
        return;
    }
    let start = if history[0]["role"].as_str() == Some("system") {
        1
    } else {
        0
    };
    let user_indices: Vec<usize> = (start..history.len())
        .filter(|&i| history[i]["role"].as_str() == Some("user"))
        .collect();
    if user_indices.len() <= max_rounds {
        return;
    }
    let keep_from = user_indices[user_indices.len() - max_rounds];
    let mut kept = history.split_off(keep_from);
    history.truncate(start);
    history.append(&mut kept);
}
