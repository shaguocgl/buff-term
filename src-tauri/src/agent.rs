mod tools;
mod trend;

use crate::credentials;
use crate::db::Db;
use crate::models::{AuditLog, Host, PermissionMode};
use crate::russh::RusshManager;
use crate::safety::is_dangerous;
use crate::session::SessionManager;
use crate::util::{extract_error, now, truncate};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use tools::{execute_tool, infer_tool_name, parse_args, system_prompt, tools_schema};

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

/// 统一发出 `ai:tool` 事件，避免 request/running/result/error/denied 五种状态各自重复构造事件体。
fn emit_tool_state(
    app: &AppHandle,
    session_id: u32,
    call: &ToolCallAcc,
    args: &serde_json::Value,
    state: &str,
    output: Option<String>,
) {
    let _ = app.emit(
        "ai:tool",
        AiTool {
            session_id,
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            args: args.clone(),
            state: state.to_string(),
            output,
        },
    );
}

#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
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

    // 会话开始时静默采集一份主机快照写入历史指标，让趋势数据随对话自然累积。
    // 失败不影响对话流程。
    if let Ok(snap) = crate::monitor::collect_russh(&host, &russh).await {
        let _ = crate::monitor::save_metric(&db, &host.id, &snap, "agent");
    }

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

    let loop_ctx = AgentLoopCtx {
        app: &app,
        client: &client,
        url: &url,
        api_key: &api_key,
        model: &model,
        host: &host,
        session_id,
        permission_mode,
        danger_rules: &danger_rules,
        db: &db,
        russh: &russh,
    };
    let result = run_agent_loop(&loop_ctx, rx, &mut history).await;

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

/// `run_agent_loop` 所需的只读上下文，聚合以避免函数参数过多（此前 13 个位置参数，
/// 顺序稍有出错编译器也无法察觉）。`rx`/`history` 会被消费/可变借用，单独作为参数传入。
/// 字段全部是引用或已实现 Copy 的类型，因此整体可以 Copy，方便按值解构。
#[derive(Clone, Copy)]
struct AgentLoopCtx<'a> {
    app: &'a AppHandle,
    client: &'a reqwest::Client,
    url: &'a str,
    api_key: &'a str,
    model: &'a str,
    host: &'a Host,
    session_id: u32,
    permission_mode: PermissionMode,
    danger_rules: &'a [String],
    db: &'a Db,
    russh: &'a RusshManager,
}

async fn run_agent_loop(
    ctx: &AgentLoopCtx<'_>,
    mut rx: mpsc::Receiver<Control>,
    history: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    let AgentLoopCtx {
        app,
        client,
        url,
        api_key,
        model,
        host,
        session_id,
        permission_mode,
        danger_rules,
        db,
        russh,
    } = *ctx;
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
                let detail = e.to_string();
                let msg = if e.is_timeout() {
                    format!("请求 AI 平台超时（120s），模型可能响应过慢或网络不畅: {detail}")
                } else if e.is_connect() {
                    format!("无法连接 AI 平台，请检查网络或 Base URL: {detail}")
                } else {
                    format!("请求 AI 平台失败: {detail}")
                };
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

        // 模型漏填工具名时，从参数推断（与 execute_tool 的兜底分支共用同一套推断逻辑，
        // 避免两处分别维护导致遗漏，例如此前 query_history 的 metric 参数未被覆盖）
        for (_, acc) in tool_calls.iter_mut() {
            if acc.name.trim().is_empty() {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&acc.args) {
                    let inferred = infer_tool_name("", &args);
                    if !inferred.is_empty() {
                        acc.name = inferred.to_string();
                    }
                }
            }
        }

        // 丢弃空壳工具调用：既没有工具名也没有任何参数信号（部分模型会输出完全空的占位调用），
        // 这类调用既无法推断也不能执行，直接忽略，避免回填“未知工具”错误让模型原地打转。
        // 注意：只要 args 是一段能解析出来的 JSON（哪怕是 "{}"），就说明模型确实发出过参数增量，
        // 不能仅因为对象内容为空就当成占位调用丢弃——resource_usage 等无参数工具的合法调用
        // 恰好就是空对象，之前的写法会把这类合法调用误杀，导致模型宣布意图后却静默中断。
        tool_calls.retain(|_, acc| {
            let has_name = !acc.name.trim().is_empty();
            let args_trimmed = acc.args.trim();
            let has_args_signal = !args_trimmed.is_empty()
                && serde_json::from_str::<serde_json::Value>(args_trimmed).is_ok();
            has_name || has_args_signal
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
                emit_tool_state(app, session_id, &acc, &args, "request", None);

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
                    let _ = insert_audit(db, AuditEntry {
                        session_id,
                        host,
                        acc: &acc,
                        args: &args,
                        permission_mode: permission_mode.as_str(),
                        approval: "denied",
                        status: "denied",
                        result: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    emit_tool_state(app, session_id, &acc, &args, "denied", None);
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": "用户拒绝执行该操作",
                    }));
                    continue;
                }
            }

            let approval_label = if need_approval { "approved" } else { "auto" };
            emit_tool_state(app, session_id, &acc, &args, "running", None);

            let result = execute_tool(db, russh, host, &acc.name, &args).await;
            match result {
                Ok(output) => {
                    let _ = insert_audit(db, AuditEntry {
                        session_id,
                        host,
                        acc: &acc,
                        args: &args,
                        permission_mode: permission_mode.as_str(),
                        approval: approval_label,
                        status: "executed",
                        result: Some(output.clone()),
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    emit_tool_state(app, session_id, &acc, &args, "result", Some(output.clone()));
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": output,
                    }));
                }
                Err(err) => {
                    let _ = insert_audit(db, AuditEntry {
                        session_id,
                        host,
                        acc: &acc,
                        args: &args,
                        permission_mode: permission_mode.as_str(),
                        approval: approval_label,
                        status: "error",
                        result: Some(err.clone()),
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                    emit_tool_state(app, session_id, &acc, &args, "error", Some(err.clone()));
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

/// `insert_audit` 的参数聚合体（此前 10 个位置参数，字段含义相近的 `&str` 挤在一起，
/// 顺序传错编译器也发现不了）。
struct AuditEntry<'a> {
    session_id: u32,
    host: &'a Host,
    acc: &'a ToolCallAcc,
    args: &'a serde_json::Value,
    permission_mode: &'a str,
    approval: &'a str,
    status: &'a str,
    result: Option<String>,
    duration_ms: u64,
}

fn insert_audit(db: &Db, entry: AuditEntry) -> Result<(), String> {
    let summary = entry
        .args
        .get("command")
        .and_then(|c| c.as_str())
        .map(String::from)
        .unwrap_or_else(|| serde_json::to_string(entry.args).unwrap_or_default());
    let result = entry.result.map(|r| truncate(&r, 300));
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: Some(entry.session_id),
        host_id: entry.host.id.clone(),
        host_label: format!("{} ({})", entry.host.name, entry.host.label_address()),
        tool_name: entry.acc.name.clone(),
        summary: truncate(&summary, 500),
        permission_mode: entry.permission_mode.to_string(),
        approval: entry.approval.to_string(),
        status: entry.status.to_string(),
        result,
        duration_ms: Some(entry.duration_ms),
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_history_keeps_system_prompt_and_recent_rounds() {
        let mut history = vec![serde_json::json!({"role": "system", "content": "sys"})];
        for i in 0..5 {
            history.push(serde_json::json!({"role": "user", "content": format!("q{i}")}));
            history.push(serde_json::json!({"role": "assistant", "content": format!("a{i}")}));
        }
        trim_history(&mut history, 2);
        // 保留 system + 最近 2 轮（每轮 user+assistant）
        assert_eq!(history[0]["role"], "system");
        let user_msgs: Vec<&str> = history
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(user_msgs, vec!["q3", "q4"]);
    }

    #[test]
    fn trim_history_noop_when_under_limit() {
        let mut history = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "q0"}),
        ];
        let before = history.clone();
        trim_history(&mut history, 20);
        assert_eq!(history, before);
    }
}
