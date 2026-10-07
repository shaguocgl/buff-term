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
use context::{
    build_context_view, repair_dangling_tool_calls, CompressionStrategy, ContextUsage,
};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tools::{execute_tool, infer_tool_name, parse_args, system_prompt, tools_schema};

struct ControlSlot {
    token: u64,
    tx: mpsc::Sender<Control>,
}

#[derive(Default)]
pub struct AgentManager {
    controls: Mutex<HashMap<u32, ControlSlot>>,
    next_control_token: AtomicU64,
    histories: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// 每台主机的结构化任务台账：跨压缩窗口保存目标/进度/失败尝试，永不裁剪。
    plans: Mutex<HashMap<String, TaskPlan>>,
    /// 不支持 stream_options.include_usage 的平台（按模型缓存），避免每次请求都重试。
    usage_unsupported: Mutex<HashSet<String>>,
    /// 每模型的估算校准系数（实测 / 本地估算 的 EMA），平台不返回 usage 时用于修正显示。
    calibration: Mutex<HashMap<String, f64>>,
    /// 主机级代数：按 host_id 索引，与 histories/plans 的口径一致。
    /// reset 一台主机即作废其历史，该主机上正在运行的循环结束时据此放弃写回旧历史。
    generations: Mutex<HashMap<String, u64>>,
    /// 同一主机同一时刻只允许一个 agent 循环，否则两个会话各自读副本→整段写回会互相覆盖历史
    active: Mutex<HashMap<String, u32>>,
}

/// 同主机 agent 循环的独占租约：agent_chat 结束时自动释放登记，
/// 覆盖所有提前返回路径（包括 `?`），避免忘记清理导致该主机永久无法再发起对话。
struct ActiveGuard<'a> {
    agents: &'a AgentManager,
    host_id: String,
    session_id: u32,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        // 仅当登记仍指向本会话时才移除，防止误删后来者（理论上同主机已被独占，这里是防御性判断）
        let mut active = self.agents.active.lock().unwrap();
        if active.get(&self.host_id) == Some(&self.session_id) {
            active.remove(&self.host_id);
        }
    }
}

pub enum Control {
    Approve { tool_call_id: String, allow: bool },
    /// 达到工具轮次上限后，用户选择是否继续执行下一批
    Continue { allow: bool },
    Cancel,
}

impl AgentManager {
    fn set_control(&self, id: u32, tx: mpsc::Sender<Control>) -> u64 {
        let token = self.next_control_token.fetch_add(1, Ordering::Relaxed);
        self.controls
            .lock()
            .unwrap()
            .insert(id, ControlSlot { token, tx });
        token
    }

    fn clear_control(&self, id: u32, token: u64) {
        let mut controls = self.controls.lock().unwrap();
        if controls.get(&id).is_some_and(|slot| slot.token == token) {
            controls.remove(&id);
        }
    }

    fn control_sender(&self, id: u32) -> Option<mpsc::Sender<Control>> {
        self.controls
            .lock()
            .unwrap()
            .get(&id)
            .map(|slot| slot.tx.clone())
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

    /// 该主机的历史代数：每次 reset 递增，用于让正在运行的循环在结束时放弃写回旧历史。
    fn generation(&self, host_id: &str) -> u64 {
        *self
            .generations
            .lock()
            .unwrap()
            .get(host_id)
            .unwrap_or(&0)
    }

    fn bump_generation(&self, host_id: &str) {
        *self
            .generations
            .lock()
            .unwrap()
            .entry(host_id.to_string())
            .or_insert(0) += 1;
    }

    /// 尝试登记该主机的活动 agent 循环；同主机已有循环在跑时返回 None。
    fn try_acquire_active(&self, host_id: &str, session_id: u32) -> Option<ActiveGuard<'_>> {
        let mut active = self.active.lock().unwrap();
        if active.contains_key(host_id) {
            return None;
        }
        active.insert(host_id.to_string(), session_id);
        Some(ActiveGuard {
            agents: self,
            host_id: host_id.to_string(),
            session_id,
        })
    }

    /// 等待上一个循环释放（停止/清空后旧循环退出需要少许时间），
    /// 超时才视为真正冲突（另一标签页在用）。
    async fn acquire_active(&self, host_id: &str, session_id: u32) -> Option<ActiveGuard<'_>> {
        for _ in 0..ACTIVE_WAIT_SECS * 10 {
            if let Some(guard) = self.try_acquire_active(host_id, session_id) {
                return Some(guard);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.try_acquire_active(host_id, session_id)
    }

    /// 该主机上正在运行的 agent 循环所属的 session_id（reset 时据此精确取消）。
    fn active_session(&self, host_id: &str) -> Option<u32> {
        self.active.lock().unwrap().get(host_id).copied()
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

/// 达到工具轮次上限时下发，前端据此弹出「继续 / 停止」确认卡片。
#[derive(Clone, Serialize)]
pub struct AiRoundsExhausted {
    pub session_id: u32,
    /// 本批已完成的模型往返轮次
    pub rounds_done: u32,
    /// 每次「继续」追加的轮次数
    pub batch: u32,
    /// 等待继续确认的秒数，超时按停止处理
    pub timeout_secs: u64,
}

/// 继续确认已处理（用户选择或超时），前端据此关闭确认卡片。
#[derive(Clone, Serialize)]
pub struct AiRoundsResolved {
    pub session_id: u32,
    pub continued: bool,
    /// 是否因等待超时而自动停止（仅在 `continued = false` 时有意义）
    pub timed_out: bool,
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

/// 中断/会话结束时为尚未回填结果的 tool_call 补一条 tool 消息，
/// 否则历史里会留下没有 tool 结果的 assistant.tool_calls，OpenAI 兼容平台会直接拒绝下一次请求（400）。
fn backfill_unanswered(history: &mut Vec<serde_json::Value>, calls: &[ToolCallAcc], note: &str) {
    for acc in calls {
        history.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": acc.id,
            "content": note,
        }));
    }
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
    // 同主机只允许一个 agent 循环：历史按主机共享，两个会话各自读副本→整段写回会互相覆盖。
    // guard 在函数返回（含任何 ? 提前返回）时自动释放登记。
    let _active_guard = agents
        .acquire_active(&host.id, session_id)
        .await
        .ok_or_else(|| {
            "该主机已有正在进行的 AI 对话（可能在另一个标签页），请等其完成或停止后再发送".to_string()
        })?;
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
    let control_token = agents.set_control(session_id, tx);
    let generation = agents.generation(&host.id);

    // 会话开始时静默采集一份主机快照写入历史指标，让趋势数据随对话自然累积。
    // 失败不影响对话流程。
    if let Ok(snap) = crate::monitor::collect_russh(&host, &russh).await {
        let _ = crate::monitor::save_metric(&db, &host.id, &snap, "agent");
    }

    // 不能用整体 timeout：流式回复的合法总时长可能远超 120s。
    // read_timeout 限定的是相邻数据块之间的最长空闲间隔，而不是整个响应的总时长。
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    let mut history = agents.history(&host.id);
    // 上次对话被中断时历史里可能留下没有 tool 结果的 assistant.tool_calls，
    // 先补齐占位结果，否则 OpenAI 兼容平台会直接拒绝本次请求（400）
    repair_dangling_tool_calls(&mut history);
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

    if agents.generation(&host.id) == generation {
        agents.save_history(&host.id, history);
    }
    agents.clear_control(session_id, control_token);
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
        .control_sender(session_id)
        .ok_or_else(|| "当前没有等待审批的工具调用".to_string())?;
    tx.try_send(Control::Approve {
        tool_call_id,
        allow,
    })
    .map_err(|_| "会话已结束".to_string())
}

/// 工具轮次达到上限后，用户确认是否继续执行下一批。
#[tauri::command]
pub fn agent_continue(
    agents: State<'_, AgentManager>,
    session_id: u32,
    allow: bool,
) -> Result<(), String> {
    let tx = agents
        .control_sender(session_id)
        .ok_or_else(|| "当前没有等待继续确认的会话".to_string())?;
    tx.try_send(Control::Continue { allow })
        .map_err(|_| "会话已结束".to_string())
}

#[tauri::command]
pub fn agent_cancel(agents: State<'_, AgentManager>, session_id: u32) -> Result<(), String> {
    if let Some(tx) = agents.control_sender(session_id) {
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
    // 停止正在运行的 agent 循环（若有）：取消该主机上实际登记的会话
    // （发起 reset 的标签页未必是正在跑循环的那个），并递增主机级代数
    // 使旧循环在结束时放弃写回历史
    let target = agents.active_session(&host_id).unwrap_or(session_id);
    if let Some(tx) = agents.control_sender(target) {
        let _ = tx.try_send(Control::Cancel);
    }
    agents.bump_generation(&host_id);
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
    // 校准系数同时参与预算判定与展示折算，保证「圆环显示」与「压缩时机」一致
    let factor = agents.calibration_factor(&model);
    let view = build_context_view(
        &history,
        plan_block.as_deref(),
        &tools,
        context_window,
        session_id,
        factor,
    )?;
    let mut usage = view.usage;
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

/// 达到轮次上限后的处理结果。
enum ContinueDecision {
    /// 用户选择继续，追加下一批轮次
    Continue,
    /// 用户选择停止或等待超时，结束本轮执行
    Stop,
}

/// 达到工具轮次上限后暂停，下发确认事件并等待用户选择（超时按停止处理）。
/// 期间收到 `Cancel` 或通道关闭同样按停止处理，保证「停止」按钮始终生效。
async fn wait_continue_decision(
    app: &AppHandle,
    session_id: u32,
    rx: &mut mpsc::Receiver<Control>,
    rounds_done: u32,
    batch: u32,
) -> ContinueDecision {
    let _ = app.emit(
        "ai:rounds-exhausted",
        AiRoundsExhausted {
            session_id,
            rounds_done,
            batch,
            timeout_secs: APPROVAL_TIMEOUT_SECS,
        },
    );
    let mut timed_out = false;
    let allow = loop {
        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx.recv()).await {
            Err(_) => {
                timed_out = true;
                break false;
            }
            Ok(None) | Ok(Some(Control::Cancel)) => break false,
            Ok(Some(Control::Continue { allow })) => break allow,
            // 上一批遗留的审批回执，忽略
            Ok(Some(Control::Approve { .. })) => continue,
        }
    };
    let _ = app.emit(
        "ai:rounds-resolved",
        AiRoundsResolved {
            session_id,
            continued: allow,
            timed_out,
        },
    );
    if allow {
        ContinueDecision::Continue
    } else {
        // 主动停止属于正常结束（而非报错），原因由前端确认卡片展示
        let _ = app.emit("ai:done", AiDone { session_id });
        ContinueDecision::Stop
    }
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
    // 单批模型往返轮次上限（可在 AI 配置中调整）。达到上限不直接中断会话，
    // 而是暂停并询问是否继续下一批，避免复杂多步运维任务被硬性截断。
    let batch = db
        .get_ai_max_tool_rounds()
        .unwrap_or_else(|_| crate::models::default_max_tool_rounds())
        .max(1);
    let mut round_limit = batch;
    loop {
        if iterations >= round_limit {
            match wait_continue_decision(app, session_id, &mut rx, iterations, batch).await {
                ContinueDecision::Continue => round_limit += batch,
                ContinueDecision::Stop => return Ok(()),
            }
        }
        iterations += 1;
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
        // 循环内每次请求前都会赋值，循环退出时必然已初始化
        let mut last_raw_estimate: usize;
        // 发送前构建视图；平台报上下文超限时按 50% 窗口重试一次。
        let resp = loop {
            // 校准系数同时参与预算判定与展示折算，保证「圆环显示」与「压缩时机」一致
            let factor = agents.calibration_factor(&calibration_key);
            let view = match build_context_view(
                history,
                plan_block.as_deref(),
                &tools,
                effective_window,
                session_id,
                factor,
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
            // send 阶段也要响应「停止」：连接/上传/等首字节期间可能阻塞较久，
            // 否则 Cancel 要等响应头到达才被察觉。此时历史中还没有本轮的
            // assistant.tool_calls（流读完后才写入），直接返回不会留下悬挂调用。
            let send = client
                .post(url)
                .bearer_auth(api_key)
                .json(&body)
                .send();
            tokio::pin!(send);
            let resp = loop {
                tokio::select! {
                    r = &mut send => break r,
                    msg = rx.recv() => match msg {
                        Some(Control::Cancel) | None => return Ok(()),
                        // 过期的审批/继续回执，忽略
                        Some(Control::Approve { .. }) | Some(Control::Continue { .. }) => {}
                    },
                }
            };
            let resp = resp.map_err(|e| {
                    let detail = e.to_string();
                    let msg = if e.is_timeout() {
                        format!("等待 AI 平台响应超时（120 秒内无数据），模型可能响应过慢或网络不畅: {detail}")
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
        // BTreeMap 按 tool_calls index 排序，保证执行顺序与模型声明的顺序一致且确定
        let mut tool_calls: BTreeMap<usize, ToolCallAcc> = BTreeMap::new();
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

        // 部分模型不下发 tool_call 的 id：用其流内序号补一个本地 id，
        // 保证审批回执匹配与 tool/tool_call_id 配对消歧
        for (index, acc) in tool_calls.iter_mut() {
            if acc.id.is_empty() {
                acc.id = format!("call_{index}");
            }
        }

        if tool_calls.is_empty() {
            if content.is_empty() {
                content = "（模型未返回内容）".to_string();
            }
            history.push(serde_json::json!({"role": "assistant", "content": content}));
            let _ = app.emit("ai:done", AiDone { session_id });
            return Ok(());
        }

        let calls: Vec<ToolCallAcc> = tool_calls.into_values().collect();
        let mut calls_json = Vec::new();
        for acc in &calls {
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

        // 不变量：每次迭代推进前都必须为 calls[i] 恰好补一条 tool 消息，
        // 否则历史里会留下没有结果的 tool_call，下一次请求会被平台拒绝（400）。
        // 因此本循环内的所有提前返回都要先 backfill 剩余调用。
        for (i, acc) in calls.iter().enumerate() {
            if let Ok(Control::Cancel) = rx.try_recv() {
                backfill_unanswered(history, &calls[i..], "用户已中断，该工具未执行");
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
                    acc,
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
                    acc,
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
                            // 当前调用的审批卡片还停在 request 态（按钮可点），
                            // 补一条 denied 让前端关闭它；其余未展示的调用无需发事件
                            emit_tool_state(
                                app,
                                session_id,
                                acc,
                                &args,
                                "denied",
                                None,
                                Some("会话已结束".to_string()),
                                None,
                            );
                            backfill_unanswered(
                                history,
                                &calls[i..],
                                "会话已结束，该工具未执行",
                            );
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
                        Ok(Some(Control::Cancel)) => {
                            // 同上：先关闭当前调用的审批卡片，再回填剩余调用的结果
                            emit_tool_state(
                                app,
                                session_id,
                                acc,
                                &args,
                                "denied",
                                None,
                                Some("用户已中断".to_string()),
                                None,
                            );
                            backfill_unanswered(
                                history,
                                &calls[i..],
                                "用户已中断，该工具未执行",
                            );
                            return Ok(());
                        }
                        Ok(Some(Control::Approve {
                            tool_call_id,
                            allow,
                        })) if tool_call_id == acc.id => {
                            break allow;
                        }
                        Ok(Some(Control::Approve { .. })) => continue,
                        // 继续确认只在轮次边界消费，这里收到说明已过期，忽略
                        Ok(Some(Control::Continue { .. })) => continue,
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
                    emit_tool_state(app, session_id, acc, &args, "denied", None, note, None);
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": if timed_out { "等待审批超时，已按拒绝处理" } else { "用户拒绝执行该操作" },
                    }));
                    continue;
                }
            }

            let approval_label = if need_approval { "approved" } else { "auto" };
            emit_tool_state(app, session_id, acc, &args, "running", None, None, None);

            let cancel = CancellationToken::new();
            let tool = execute_tool(db, russh, host, normalized_name, &args, &cancel);
            tokio::pin!(tool);
            let result = loop {
                tokio::select! {
                    r = &mut tool => break r,
                    msg = rx.recv() => match msg {
                        Some(Control::Cancel) | None => {
                            // 取消不是简单丢弃 future：先通知工具层（SSH 通道会向远端进程
                            // 发 SIGTERM），给它有界的清理时间，再回填未执行的工具结果
                            cancel.cancel();
                            let _ =
                                tokio::time::timeout(Duration::from_secs(5), &mut tool).await;
                            emit_tool_state(
                                app,
                                session_id,
                                acc,
                                &args,
                                "error",
                                Some("已中断".into()),
                                None,
                                None,
                            );
                            backfill_unanswered(history, &calls[i..], "用户已中断执行");
                            return Ok(());
                        }
                        Some(Control::Approve { .. }) | Some(Control::Continue { .. }) => {}
                    },
                }
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
                        acc,
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
                        acc,
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
        source: "agent".to_string(),
        summary,
        permission_mode: entry.permission_mode.to_string(),
        approval: entry.approval.to_string(),
        status: entry.status.to_string(),
        result,
        duration_ms: Some(entry.duration_ms),
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作审计失败: {e}"))
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
    tool_calls: &mut BTreeMap<usize, ToolCallAcc>,
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

/// 同主机新会话等待上一个 agent 循环释放登记的上限（秒）
const ACTIVE_WAIT_SECS: u64 = 10;

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
    fn control_slot_token_prevents_stale_clear() {
        let agents = AgentManager::default();
        let (old_tx, _old_rx) = mpsc::channel::<Control>(1);
        let (new_tx, _new_rx) = mpsc::channel::<Control>(1);
        let old_token = agents.set_control(7, old_tx);
        let new_token = agents.set_control(7, new_tx);
        assert_ne!(old_token, new_token);
        agents.clear_control(7, old_token);
        assert!(agents.control_sender(7).is_some());
        agents.clear_control(7, new_token);
        assert!(agents.control_sender(7).is_none());
    }

    #[test]
    fn backfill_unanswered_pushes_one_tool_message_per_call() {
        let mut history = vec![serde_json::json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
            ],
        })];
        let calls = vec![
            ToolCallAcc {
                id: "c1".to_string(),
                ..Default::default()
            },
            ToolCallAcc {
                id: "c2".to_string(),
                ..Default::default()
            },
        ];
        backfill_unanswered(&mut history, &calls, "用户已中断，该工具未执行");
        assert_eq!(history.len(), 3);
        assert_eq!(history[1]["role"], "tool");
        assert_eq!(history[1]["tool_call_id"], "c1");
        assert_eq!(history[1]["content"], "用户已中断，该工具未执行");
        assert_eq!(history[2]["role"], "tool");
        assert_eq!(history[2]["tool_call_id"], "c2");
    }

    #[test]
    fn active_guard_blocks_second_session_on_same_host() {
        let agents = AgentManager::default();
        let guard = agents
            .try_acquire_active("h1", 1)
            .expect("首次登记应成功");
        assert!(
            agents.try_acquire_active("h1", 2).is_none(),
            "同主机第二个会话应被拒绝"
        );
        assert_eq!(agents.active_session("h1"), Some(1));
        // 不同主机互不影响
        assert!(agents.try_acquire_active("h2", 3).is_some());
        drop(guard);
        assert!(
            agents.try_acquire_active("h1", 2).is_some(),
            "释放后应能重新登记"
        );
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
