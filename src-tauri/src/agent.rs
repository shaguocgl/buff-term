mod context;
mod tools;
mod trend;

use crate::credentials;
use crate::db::Db;
use crate::models::{AuditLog, Host, PermissionMode, TaskPlan};
use crate::russh::RusshManager;
use crate::safety::{is_dangerous, is_write_operation, normalize_tool, sanitize};
use crate::session::SessionManager;
use crate::util::{extract_error, now, truncate, truncate_output};
use context::{build_context_view, CompressionStrategy, ContextUsage};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use tools::{execute_tool, infer_tool_name, parse_args, system_prompt, tools_schema};

#[derive(Default)]
pub struct AgentManager {
    controls: Mutex<HashMap<u32, mpsc::Sender<Control>>>,
    histories: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// 每台主机的结构化任务台账：跨压缩窗口保存目标/进度/失败尝试，永不裁剪。
    plans: Mutex<HashMap<String, TaskPlan>>,
    /// 不支持 stream_options.include_usage 的平台（按模型缓存），避免每次请求都重试。
    usage_unsupported: Mutex<HashSet<String>>,
    /// 每模型的估算校准系数（实测 / 本地估算 的 EMA），平台不返回 usage 时用于修正显示。
    calibration: Mutex<HashMap<String, f64>>,
    /// 会话级代数：按 session_id 索引（此前按 host_id，同主机多会话互相干扰，
    /// 一个会话 reset 会导致另一个会话的历史写回被丢弃）
    generations: Mutex<HashMap<u32, u64>>,
}

pub enum Control {
    Approve { tool_call_id: String, allow: bool },
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

    fn plan(&self, host_id: &str) -> Option<TaskPlan> {
        self.plans.lock().unwrap().get(host_id).cloned()
    }

    fn set_plan(&self, host_id: &str, plan: TaskPlan) {
        self.plans.lock().unwrap().insert(host_id.to_string(), plan);
    }

    fn clear_plan(&self, host_id: &str) {
        self.plans.lock().unwrap().remove(host_id);
    }

    fn usage_unsupported(&self, key: &str) -> bool {
        self.usage_unsupported.lock().unwrap().contains(key)
    }

    fn mark_usage_unsupported(&self, key: &str) {
        self.usage_unsupported
            .lock()
            .unwrap()
            .insert(key.to_string());
    }

    fn calibration_factor(&self, key: &str) -> f64 {
        *self.calibration.lock().unwrap().get(key).unwrap_or(&1.0)
    }

    /// 用平台实测 prompt_tokens 校准本地估算：EMA 收敛，限制在 0.25–4.0 之间，
    /// 避免个别异常值把系数拉飞。
    fn update_calibration(&self, key: &str, actual: usize, estimated: usize) {
        if estimated == 0 {
            return;
        }
        let ratio = (actual as f64 / estimated as f64).clamp(0.25, 4.0);
        let mut map = self.calibration.lock().unwrap();
        let entry = map.entry(key.to_string()).or_insert(1.0);
        *entry = (*entry * 0.7 + ratio * 0.3).clamp(0.25, 4.0);
    }

    /// 当前会话的历史代数：每次 reset 递增，用于让正在运行的循环在结束时放弃写回旧历史。
    fn generation(&self, session_id: u32) -> u64 {
        *self
            .generations
            .lock()
            .unwrap()
            .get(&session_id)
            .unwrap_or(&0)
    }

    fn bump_generation(&self, session_id: u32) {
        *self
            .generations
            .lock()
            .unwrap()
            .entry(session_id)
            .or_insert(0) += 1;
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
    /// request/denied 态附带：审批原因（命中规则 / 内置危险 / 模型标记 / 全部审核）
    pub reason: Option<String>,
    /// request 态附带：审批超时秒数，超时按拒绝处理
    pub timeout_secs: Option<u64>,
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

#[derive(Clone, Serialize)]
pub struct AiPlan {
    pub session_id: u32,
    pub plan: TaskPlan,
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

/// 统一发出 `ai:tool` 事件，避免 request/running/result/error/denied 五种状态各自重复构造事件体。
/// reason / timeout_secs 仅审批相关状态（request/denied）携带，其余状态传 None。
fn emit_tool_state(
    app: &AppHandle,
    session_id: u32,
    call: &ToolCallAcc,
    args: &serde_json::Value,
    state: &str,
    output: Option<String>,
    reason: Option<String>,
    timeout_secs: Option<u64>,
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
            reason,
            timeout_secs,
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
    let (provider, model_info) = crate::ai::resolve_active_ai_model(&db)?;
    let model = model_info.model.clone();
    let context_window = model_info.context_window;
    eprintln!(
        "[agent] 使用模型: {}（{}，窗口 {} tokens）",
        model, provider.name, context_window
    );
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
    let generation = agents.generation(session_id);

    // 会话开始时静默采集一份主机快照写入历史指标，让趋势数据随对话自然累积。
    // 失败不影响对话流程。
    if let Ok(snap) = crate::monitor::collect_russh(&host, &russh).await {
        let _ = crate::monitor::save_metric(&db, &host.id, &snap, "agent");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    let mut history = agents.history(&host.id);
    let system = system_prompt(&host, &provider, &model, permission_mode);
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
        context_window,
        host: &host,
        session_id,
        permission_mode,
        danger_rules: &danger_rules,
        db: &db,
        russh: &russh,
        agents: agents.inner(),
    };
    let result = run_agent_loop(&loop_ctx, rx, &mut history).await;

    if agents.generation(session_id) == generation {
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
    // 停止正在运行的 agent 循环（若有），并递增该会话的代数使旧循环在结束时放弃写回历史
    if let Some(tx) = agents.controls.lock().unwrap().get(&session_id).cloned() {
        let _ = tx.try_send(Control::Cancel);
    }
    agents.bump_generation(session_id);
    agents.clear_history(&host_id);
    agents.clear_plan(&host_id);
    Ok(())
}

#[tauri::command]
pub fn get_history(
    agents: State<'_, AgentManager>,
    host_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(agents.history(&host_id))
}

#[tauri::command]
pub fn get_task_plan(
    agents: State<'_, AgentManager>,
    host_id: String,
) -> Result<Option<TaskPlan>, String> {
    Ok(agents.plan(&host_id))
}

/// 不发送模型请求，仅按当前完整历史 + 指定模型窗口估算发送视图用量。
/// 用于切换模型/打开对话时立即刷新进度条，避免显示上一个模型的旧数据。
#[tauri::command]
pub fn get_context_usage(
    agents: State<'_, AgentManager>,
    host_id: String,
    session_id: u32,
    context_window: u32,
    model: String,
) -> Result<ContextUsage, String> {
    let history = agents.history(&host_id);
    let plan_block = agents
        .plan(&host_id)
        .map(|p| p.render_block())
        .filter(|block| !block.trim().is_empty());
    let tools = tools_schema();
    let view = build_context_view(
        &history,
        plan_block.as_deref(),
        &tools,
        context_window,
        session_id,
    )?;
    let mut usage = view.usage;
    let factor = agents.calibration_factor(&model);
    if factor != 1.0 {
        usage.used_tokens = ((usage.used_tokens as f64) * factor).round() as usize;
        usage.history_tokens = ((usage.history_tokens as f64) * factor).round() as usize;
        usage.calibrated = true;
    }
    Ok(usage)
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
    context_window: u32,
    host: &'a Host,
    session_id: u32,
    permission_mode: PermissionMode,
    danger_rules: &'a [String],
    db: &'a Db,
    russh: &'a RusshManager,
    agents: &'a AgentManager,
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
        context_window,
        host,
        session_id,
        permission_mode,
        danger_rules,
        db,
        russh,
        agents,
    } = *ctx;
    let mut iterations = 0;
    // 迭代次数上限按权限模式动态调整：none 模式无需逐条审批，复杂多步运维任务
    // 可能需要更多工具调用；smart/all 模式下每条危险命令都需审批，上限保持保守。
    let max_iterations = match permission_mode {
        PermissionMode::None => 30,
        PermissionMode::Smart | PermissionMode::All => 12,
    };
    loop {
        iterations += 1;
        if iterations > max_iterations {
            let msg = format!("工具调用次数过多（上限 {max_iterations}），已停止");
            let _ = app.emit(
                "ai:error",
                AiError {
                    session_id,
                    message: msg.clone(),
                },
            );
            return Err(msg);
        }
        if let Ok(Control::Cancel) = rx.try_recv() {
            return Ok(());
        }

        let tools = tools_schema();
        let plan_block = agents
            .plan(&host.id)
            .map(|p| p.render_block())
            .filter(|block| !block.trim().is_empty());
        let calibration_key = model.to_string();
        let mut effective_window = context_window;
        let mut reactive_retried = false;
        // 首次尝试携带 stream_options.include_usage；平台拒绝时去掉并记住，后续请求不再带。
        let mut include_usage = !agents.usage_unsupported(&calibration_key);
        let mut usage_param_retried = false;
        let mut last_usage: ContextUsage;
        let mut last_raw_estimate = 0usize;
        // 发送前构建视图；平台报上下文超限时按 50% 窗口重试一次。
        let resp = loop {
            let view = match build_context_view(
                history,
                plan_block.as_deref(),
                &tools,
                effective_window,
                session_id,
            ) {
                Ok(view) => view,
                Err(msg) => {
                    let _ = app.emit(
                        "ai:error",
                        AiError {
                            session_id,
                            message: msg.clone(),
                        },
                    );
                    return Err(msg);
                }
            };
            let mut usage = view.usage;
            if reactive_retried {
                usage.strategy = CompressionStrategy::ReactiveReduce;
                usage.warning = Some(format!(
                    "平台报告上下文超限，已按配置窗口的 50%（{} tokens）重试；若仍失败请调大该模型的 context_window",
                    effective_window
                ));
            }
            // 平台不返回 usage 时，用该模型的历史校准系数修正本地估算。
            let factor = agents.calibration_factor(&calibration_key);
            last_raw_estimate = usage.used_tokens;
            if factor != 1.0 {
                usage.used_tokens = ((usage.used_tokens as f64) * factor).round() as usize;
                usage.history_tokens =
                    ((usage.history_tokens as f64) * factor).round() as usize;
                usage.calibrated = true;
            }
            last_usage = usage;
            let mut body = serde_json::json!({
                "model": model,
                "messages": view.messages,
                "stream": true,
                "tools": &tools,
            });
            if include_usage {
                body["stream_options"] = serde_json::json!({"include_usage": true});
            }
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
                    let _ = app.emit(
                        "ai:error",
                        AiError {
                            session_id,
                            message: msg.clone(),
                        },
                    );
                    msg
                })?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                if include_usage
                    && !usage_param_retried
                    && is_unsupported_stream_options_error(&text)
                {
                    usage_param_retried = true;
                    include_usage = false;
                    agents.mark_usage_unsupported(&calibration_key);
                    continue;
                }
                if !reactive_retried && is_context_length_error(&text) {
                    reactive_retried = true;
                    effective_window = (effective_window / 2).max(1);
                    continue;
                }
                let msg = extract_error(&text, status);
                let _ = app.emit(
                    "ai:error",
                    AiError {
                        session_id,
                        message: msg.clone(),
                    },
                );
                return Err(msg);
            }
            break resp;
        };

        let mut stream = resp.bytes_stream();
        // 字节缓冲而非 String：SSE 的 UTF-8 多字节字符可能被切分到两个 chunk，
        // 逐 chunk 用 from_utf8_lossy 会破坏跨 chunk 的中文/emoji，改为先累积字节、
        // 在 \n 边界处解码完整行
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut tool_calls: HashMap<usize, ToolCallAcc> = HashMap::new();
        let mut done = false;
        let mut reported_prompt_tokens: Option<usize> = None;

        // select! 让「停止」能中断阻塞中的流式读取，而不是等当前 chunk 返回
        loop {
            tokio::select! {
                chunk = stream.next() => {
                    let Some(chunk) = chunk else { break };
                    let chunk = chunk.map_err(|e| format!("读取响应流失败: {e}"))?;
                    buf.extend_from_slice(&chunk);
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                        buf.drain(..pos + 1);
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
                        // 部分平台会在流末尾附带 usage；有就采信，用于把用量条从“估算”变成实测。
                        if let Some(u) = value.get("usage") {
                            reported_prompt_tokens = u["prompt_tokens"]
                                .as_u64()
                                .or_else(|| u["total_tokens"].as_u64())
                                .map(|v| v as usize)
                                .or(reported_prompt_tokens);
                        }
                        apply_delta(
                            &value["choices"][0]["delta"],
                            &mut content,
                            &mut tool_calls,
                            app,
                            session_id,
                        );
                    }
                }
                _ = rx.recv() => return Ok(()), // 用户点击停止
            }
            if done {
                break;
            }
        }
        // 处理流结束时缓冲区中残留的未换行数据，避免丢失最后一段内容
        if !buf.is_empty() {
            let data = String::from_utf8_lossy(&buf).trim().to_string();
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

        let mut usage = last_usage;
        if let Some(actual) = reported_prompt_tokens {
            agents.update_calibration(&calibration_key, actual, last_raw_estimate);
            usage.used_tokens = actual;
            usage.estimated = false;
            usage.calibrated = false;
        }
        let _ = app.emit("ai:context", usage);

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

            // 统一规范化工具名：审批判定与实际执行必须使用同一套规范名，
            // 否则模型用别名（exec/shell/run）或空名调用 exec_command 时可绕过审批门。
            let normalized_name = normalize_tool(infer_tool_name(&acc.name, &args));

            // 任务台账工具：只更新本地状态，不访问远程主机、不参与审批。
            if normalized_name == "update_task_plan" {
                let existing = agents.plan(&host.id).unwrap_or_default();
                let plan = parse_task_plan_patch(&args, existing);
                agents.set_plan(&host.id, plan.clone());
                let _ = app.emit("ai:plan", AiPlan { session_id, plan });
                emit_tool_state(
                    app,
                    session_id,
                    &acc,
                    &args,
                    "result",
                    Some("计划已更新".to_string()),
                    None,
                    None,
                );
                history.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": acc.id,
                    "content": "计划已更新",
                }));
                continue;
            }

            // 判定是否需要审批，并记录审批原因（随 request 事件下发，供审批卡片展示）
            let mut reason: Option<String> = None;
            let need_approval = match permission_mode {
                PermissionMode::All => {
                    reason = Some("全部审核：所有命令执行前都需要批准".to_string());
                    true
                }
                PermissionMode::None => false,
                PermissionMode::Smart => {
                    if normalized_name != "exec_command" {
                        false
                    } else {
                        let marked = args
                            .get("requires_approval")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        let c = command.to_ascii_lowercase();
                        if marked {
                            reason = Some("模型标记该命令需要审批".to_string());
                            true
                        } else if is_dangerous(command) {
                            reason = Some("命中内置危险命令规则".to_string());
                            true
                        } else if let Some(p) = danger_rules
                            .iter()
                            .find(|p| !p.trim().is_empty() && c.contains(&p.to_ascii_lowercase()))
                        {
                            reason = Some(format!("命中自定义规则：{p}"));
                            true
                        } else if is_write_operation(command) {
                            // 写操作（修改/删除/安装/网络写入等）同样需要批准，
                            // 与 MCP 只读模式的判定口径一致，避免静默放行文件修改
                            reason = Some("命令包含写操作，需要批准".to_string());
                            true
                        } else {
                            false
                        }
                    }
                }
            };

            if need_approval {
                emit_tool_state(
                    app,
                    session_id,
                    &acc,
                    &args,
                    "request",
                    None,
                    reason.clone(),
                    Some(APPROVAL_TIMEOUT_SECS),
                );

                let mut timed_out = false;
                let decision = loop {
                    match tokio::time::timeout(
                        Duration::from_secs(APPROVAL_TIMEOUT_SECS),
                        rx.recv(),
                    )
                    .await
                    {
                        Err(_) => {
                            // 审批超时：按拒绝处理，不终止整个会话
                            timed_out = true;
                            break false;
                        }
                        Ok(None) => {
                            let msg = "会话已结束".to_string();
                            let _ = app.emit(
                                "ai:error",
                                AiError {
                                    session_id,
                                    message: msg.clone(),
                                },
                            );
                            return Err(msg);
                        }
                        Ok(Some(Control::Cancel)) => return Ok(()),
                        Ok(Some(Control::Approve {
                            tool_call_id,
                            allow,
                        })) if tool_call_id == acc.id => {
                            break allow;
                        }
                        Ok(Some(Control::Approve { .. })) => continue,
                    }
                };

                if !decision {
                    let note = if timed_out {
                        Some("等待审批超时，已按拒绝处理".to_string())
                    } else {
                        reason.clone()
                    };
                    let _ = insert_audit(
                        db,
                        AuditEntry {
                            session_id,
                            host,
                            tool_name: normalized_name,
                            args: &args,
                            permission_mode: permission_mode.as_str(),
                            approval: "denied",
                            status: "denied",
                            result: note.clone(),
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                    emit_tool_state(app, session_id, &acc, &args, "denied", None, note, None);
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": if timed_out { "等待审批超时，已按拒绝处理" } else { "用户拒绝执行该操作" },
                    }));
                    continue;
                }
            }

            let approval_label = if need_approval { "approved" } else { "auto" };
            emit_tool_state(app, session_id, &acc, &args, "running", None, None, None);

            // select! 让「停止」能中断正在执行的工具：drop future 会关闭本地 SSH 通道；
            // 远端命令可能仍在运行（与超时语义一致），但本地立即停止等待
            let result = tokio::select! {
                r = execute_tool(db, russh, host, normalized_name, &args) => r,
                _ = rx.recv() => return Ok(()),
            };
            match result {
                Ok(output) => {
                    let _ = insert_audit(
                        db,
                        AuditEntry {
                            session_id,
                            host,
                            tool_name: normalized_name,
                            args: &args,
                            permission_mode: permission_mode.as_str(),
                            approval: approval_label,
                            status: "executed",
                            result: Some(output.clone()),
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                    emit_tool_state(
                        app,
                        session_id,
                        &acc,
                        &args,
                        "result",
                        Some(output.clone()),
                        None,
                        None,
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": truncate_output(&output, 8000),
                    }));
                }
                Err(err) => {
                    let _ = insert_audit(
                        db,
                        AuditEntry {
                            session_id,
                            host,
                            tool_name: normalized_name,
                            args: &args,
                            permission_mode: permission_mode.as_str(),
                            approval: approval_label,
                            status: "error",
                            result: Some(err.clone()),
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                    emit_tool_state(
                        app,
                        session_id,
                        &acc,
                        &args,
                        "error",
                        Some(err.clone()),
                        None,
                        None,
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": truncate_output(&format!("执行失败: {err}"), 8000),
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
    /// 规范化后的工具名（审批与执行共用）
    tool_name: &'a str,
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
    // 落库前统一脱敏：命令本身可能包含密码/Token（如 mysql -pSecret、curl -u user:pass），
    // 与终端拦截审计的 sanitize 口径保持一致
    let summary = truncate(&sanitize(&summary), 500);
    let result = entry.result.map(|r| truncate(&sanitize(&r), 300));
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: Some(entry.session_id),
        host_id: entry.host.id.clone(),
        host_label: format!("{} ({})", entry.host.name, entry.host.label_address()),
        tool_name: entry.tool_name.to_string(),
        summary,
        permission_mode: entry.permission_mode.to_string(),
        approval: entry.approval.to_string(),
        status: entry.status.to_string(),
        result,
        duration_ms: Some(entry.duration_ms),
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作日志失败: {e}"))
}

/// 解析 `update_task_plan` 的参数。采用“patch”语义：请求里出现的字段覆盖旧值，
/// 未出现的字段保留旧值，避免模型漏传某个列表时把已完成/待办整段清空。
fn parse_task_plan_patch(args: &serde_json::Value, existing: TaskPlan) -> TaskPlan {
    fn list(args: &serde_json::Value, key: &str, fallback: &[String]) -> Vec<String> {
        match args.get(key).and_then(|v| v.as_array()) {
            Some(items) => items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => fallback.to_vec(),
        }
    }
    fn text(args: &serde_json::Value, key: &str, fallback: &str) -> String {
        match args.get(key).and_then(|v| v.as_str()) {
            Some(s) => s.trim().to_string(),
            None => fallback.to_string(),
        }
    }
    TaskPlan {
        goal: text(args, "goal", &existing.goal),
        constraints: list(args, "constraints", &existing.constraints),
        completed: list(args, "completed", &existing.completed),
        pending: list(args, "pending", &existing.pending),
        failed: list(args, "failed", &existing.failed),
        current_step: text(args, "current_step", &existing.current_step),
        updated_at: now(),
    }
}

/// 判断平台错误是否为“上下文超限”，用于触发一次降窗重试。
fn is_context_length_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "context length",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "context_length_exceeded",
        "exceeds the maximum number of tokens",
        "reduce the length",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// 判断平台是否因为不认识 `stream_options` 字段而拒绝请求。
fn is_unsupported_stream_options_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("stream_options") {
        return true;
    }
    let mentions_param = lower.contains("parameter")
        || lower.contains("argument")
        || lower.contains("field");
    let unsupported = lower.contains("unknown")
        || lower.contains("unrecognized")
        || lower.contains("unsupported")
        || lower.contains("extra");
    mentions_param && unsupported
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

/// 单个工具审批的等待上限（秒），超时按拒绝处理，不终止会话
const APPROVAL_TIMEOUT_SECS: u64 = 600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_plan_patch_keeps_omitted_fields() {
        let existing = TaskPlan {
            goal: "部署 nginx".to_string(),
            completed: vec!["安装依赖".to_string()],
            pending: vec!["启动服务".to_string()],
            ..Default::default()
        };
        let args = serde_json::json!({
            "completed": ["安装依赖", "写配置"],
            "current_step": "启动服务"
        });
        let plan = parse_task_plan_patch(&args, existing);
        assert_eq!(plan.goal, "部署 nginx");
        assert_eq!(plan.completed, vec!["安装依赖", "写配置"]);
        assert_eq!(plan.pending, vec!["启动服务"]);
        assert_eq!(plan.current_step, "启动服务");
    }

    #[test]
    fn task_plan_patch_can_clear_a_list_explicitly() {
        let existing = TaskPlan {
            pending: vec!["旧待办".to_string()],
            ..Default::default()
        };
        let args = serde_json::json!({"pending": []});
        let plan = parse_task_plan_patch(&args, existing);
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn detects_context_length_errors() {
        assert!(is_context_length_error(
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(is_context_length_error("prompt is too long"));
        assert!(!is_context_length_error("invalid api key"));
    }

    #[test]
    fn detects_unsupported_stream_options_errors() {
        assert!(is_unsupported_stream_options_error(
            "Unrecognized request argument supplied: stream_options"
        ));
        assert!(is_unsupported_stream_options_error(
            "Unknown parameter: stream_options"
        ));
        assert!(is_unsupported_stream_options_error(
            "Extra fields not permitted"
        ));
        assert!(!is_unsupported_stream_options_error("invalid api key"));
    }

    #[test]
    fn calibration_ema_converges_and_clamps() {
        let agents = AgentManager::default();
        agents.update_calibration("m", 200, 100);
        assert!(agents.calibration_factor("m") > 1.0);
        for _ in 0..30 {
            agents.update_calibration("m", 200, 100);
        }
        assert!((agents.calibration_factor("m") - 2.0).abs() < 0.05);
        agents.update_calibration("m", 100_000, 1);
        assert!(agents.calibration_factor("m") <= 4.0);
    }
}
