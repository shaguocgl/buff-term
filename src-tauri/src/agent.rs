mod approval;
mod context;
mod run;
mod stream;
mod tools;
mod trend;

use crate::credentials;
use crate::db::Db;
use crate::i18n::{is_en, t};
use crate::models::{Host, PermissionMode, TaskPlan};
use crate::russh::RusshManager;
use crate::session::SessionManager;
use context::{build_context_view, repair_dangling_tool_calls, ContextUsage};
use run::{AgentRun, RunCtx, RunOutcome};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use tools::{system_prompt, tools_schema};

/// 一台主机上正在运行的 agent 循环。
struct ActiveRun {
    /// 发起本轮对话的前端会话 id（审批/继续/停止命令按它路由控制消息）
    session_id: u32,
    /// 单调递增的登记令牌，RunLease 释放时据此区分新旧登记
    token: u64,
    /// 循环的控制通道，命令层 clone 后 try_send 控制消息
    control: mpsc::Sender<Control>,
}

/// 单台主机的对话实体：历史、台账、代数与活动循环同属一个实体，放在同一把锁下，
/// 保证 reset 与循环写回之间「比对代数 + 写入」原子完成。
#[derive(Default)]
struct HostConversation {
    /// canonical history：只追加不裁剪，发送视图由 context 模块按窗口生成
    history: Vec<serde_json::Value>,
    /// 结构化任务台账：跨压缩窗口保存目标/进度/失败尝试，永不裁剪
    plan: Option<TaskPlan>,
    /// 该主机的历史代数：每次 reset 递增，让正在收尾的循环写回时被拒
    generation: u64,
    /// 同一主机同一时刻只允许一个 agent 循环，否则两个会话各自读副本→整段写回会互相覆盖历史
    active: Option<ActiveRun>,
}

#[derive(Default)]
pub struct AgentManager {
    /// 按 host_id 索引的每主机对话实体（历史/台账/代数/活动循环同锁管理）
    conversations: Mutex<HashMap<String, HostConversation>>,
    /// 活动循环的登记令牌序列
    next_run_token: AtomicU64,
    /// 不支持 stream_options.include_usage 的平台（按模型缓存），避免每次请求都重试。
    usage_unsupported: Mutex<HashSet<String>>,
    /// 每模型的估算校准系数（实测 / 本地估算 的 EMA），平台不返回 usage 时用于修正显示。
    calibration: Mutex<HashMap<String, f64>>,
}

/// 同主机 agent 循环的独占租约：持有期间该主机拒绝新的循环登记；
/// reset 会让代数失效，之后经租约的写回（commit_history/set_plan）全部被拒。
/// Drop 时自动释放登记，覆盖所有提前返回路径（包括 `?`），
/// 避免忘记清理导致该主机永久无法再发起对话。
struct RunLease<'a> {
    /// 共享的 agent 状态（写回历史/台账、释放登记都经它）
    agents: &'a AgentManager,
    /// 本租约所属的主机 id
    host_id: String,
    /// 登记令牌：Drop 时据此区分新旧登记，防止误删后来者
    token: u64,
    /// 登记时看到的主机代数；reset 后代数递增，本租约的写回随之作废
    generation: u64,
}

pub enum Control {
    Approve { tool_call_id: String, allow: bool },
    /// 达到工具轮次上限后，用户选择是否继续执行下一批
    Continue { allow: bool },
    Cancel,
}

impl AgentManager {
    /// 查找某个会话当前登记的控制通道：一个会话只属于一台主机。
    fn control_sender(&self, session_id: u32) -> Option<mpsc::Sender<Control>> {
        self.conversations
            .lock()
            .unwrap()
            .values()
            .find_map(|conv| {
                conv.active
                    .as_ref()
                    .filter(|run| run.session_id == session_id)
                    .map(|run| run.control.clone())
            })
    }

    fn history(&self, host_id: &str) -> Vec<serde_json::Value> {
        self.conversations
            .lock()
            .unwrap()
            .get(host_id)
            .map(|conv| conv.history.clone())
            .unwrap_or_default()
    }

    fn plan(&self, host_id: &str) -> Option<TaskPlan> {
        self.conversations
            .lock()
            .unwrap()
            .get(host_id)
            .and_then(|conv| conv.plan.clone())
    }

    /// 注入系统提示/用量估算的台账文本块：空台账返回 None。
    fn plan_block(&self, host_id: &str) -> Option<String> {
        self.plan(host_id)
            .map(|p| p.render_block())
            .filter(|block| !block.trim().is_empty())
    }

    /// 重置一台主机的对话：递增代数作废正在收尾的写回、清空历史与台账，
    /// 并取消正在运行的循环（发起 reset 的标签页未必是正在跑循环的那个）。
    /// 会话条目本身保留——代数必须保持单调递增，否则正在收尾的旧循环可能
    /// 把重建后的 0 代条目当成同代，把旧历史写进新会话。
    pub(crate) fn reset_host(&self, host_id: &str) {
        let sender = {
            let mut conversations = self.conversations.lock().unwrap();
            let Some(conv) = conversations.get_mut(host_id) else {
                return;
            };
            conv.generation += 1;
            conv.history.clear();
            conv.plan = None;
            conv.active.as_ref().map(|run| run.control.clone())
        };
        // 锁外发送：try_send 不阻塞，但保持「锁内只改状态」的惯例
        if let Some(tx) = sender {
            let _ = tx.try_send(Control::Cancel);
        }
    }

    /// 尝试登记该主机的活动 agent 循环；同主机已有循环在跑时返回 None。
    /// 成功时返回独占租约（Drop 自动释放登记）与控制通道接收端。
    fn try_acquire_run(
        &self,
        host_id: &str,
        session_id: u32,
    ) -> Option<(RunLease<'_>, mpsc::Receiver<Control>)> {
        let mut conversations = self.conversations.lock().unwrap();
        let conv = conversations.entry(host_id.to_string()).or_default();
        if conv.active.is_some() {
            return None;
        }
        let (tx, rx) = mpsc::channel(8);
        let token = self.next_run_token.fetch_add(1, Ordering::Relaxed);
        conv.active = Some(ActiveRun {
            session_id,
            token,
            control: tx,
        });
        Some((
            RunLease {
                agents: self,
                host_id: host_id.to_string(),
                token,
                generation: conv.generation,
            },
            rx,
        ))
    }

    /// 等待上一个循环释放（停止/清空后旧循环退出需要少许时间），
    /// 超时才视为真正冲突（另一标签页在用）。
    async fn acquire_run(
        &self,
        host_id: &str,
        session_id: u32,
    ) -> Option<(RunLease<'_>, mpsc::Receiver<Control>)> {
        for _ in 0..ACTIVE_WAIT_SECS * 10 {
            if let Some(acquired) = self.try_acquire_run(host_id, session_id) {
                return Some(acquired);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.try_acquire_run(host_id, session_id)
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
}

impl<'a> RunLease<'a> {
    /// 仅当会话条目仍存在且代数未被 reset 作废时，在锁内执行 `f`。
    /// 「比对代数 + 写入」在同一把锁内完成，reset 无法插进中间。
    fn with_current<R>(&self, f: impl FnOnce(&mut HostConversation) -> R) -> Option<R> {
        let mut conversations = self.agents.conversations.lock().unwrap();
        let conv = conversations.get_mut(&self.host_id)?;
        if conv.generation != self.generation {
            return None;
        }
        Some(f(conv))
    }

    /// 循环结束时把历史写回该主机；返回 false 表示期间发生过 reset，写回被丢弃。
    fn commit_history(&self, history: Vec<serde_json::Value>) -> bool {
        self.with_current(|conv| conv.history = history).is_some()
    }

    /// 更新该主机的任务台账；返回 false 表示期间发生过 reset，更新被丢弃。
    fn set_plan(&self, plan: TaskPlan) -> bool {
        self.with_current(|conv| conv.plan = Some(plan)).is_some()
    }
}

impl Drop for RunLease<'_> {
    fn drop(&mut self) {
        // 仅当登记仍指向本轮（token 相同）时才清除，防止误删后来者
        // （理论上同主机已被独占，这里是防御性判断）
        let mut conversations = self.agents.conversations.lock().unwrap();
        if let Some(conv) = conversations.get_mut(&self.host_id) {
            if conv.active.as_ref().is_some_and(|run| run.token == self.token) {
                conv.active = None;
            }
        }
    }
}

/// `ai:tool` 事件的工具状态（小写序列化，与前端 ToolState 类型一致）。
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    /// 等待用户审批
    Request,
    /// 被拒绝（用户拒绝 / 等待超时 / 用户中断）
    Denied,
    /// 正在执行
    Running,
    /// 执行完成，output 携带结果
    Result,
    /// 执行或前置校验出错，output 携带错误文本
    Error,
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
    pub state: ToolState,
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

/// `chat` 的请求参数聚合体（从 Tauri 命令参数整理而来）。
struct ChatRequest {
    /// 发起对话的前端会话 id（会话→主机解析与事件路由）
    session_id: u32,
    /// 用户消息文本
    message: String,
    /// 审批权限模式
    permission_mode: PermissionMode,
    /// 随消息附带的图片（data URL，前端已压缩；可为空）
    images: Vec<String>,
}

/// `images`：随消息附带的图片（data URL，如 `data:image/jpeg;base64,...`），前端已压缩。
/// 仅当当前模型在 AI 配置中勾选「支持图片」时才允许非空。
#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    sessions: State<'_, SessionManager>,
    agents: State<'_, AgentManager>,
    session_id: u32,
    message: String,
    permission_mode: PermissionMode,
    images: Option<Vec<String>>,
) -> Result<(), String> {
    let request = ChatRequest {
        session_id,
        message,
        permission_mode,
        images: images.unwrap_or_default(),
    };
    let outcome = chat(&app, &db, &sessions, &agents, request).await;
    // 终态事件统一在这里下发一次：历史已提交、租约已释放之后。
    // 用户中断不发 ai:done：前端点「停止」时已自行复位，避免误伤紧随其后的新一轮。
    match &outcome {
        Ok(RunOutcome::Done) => {
            let _ = app.emit("ai:done", AiDone { session_id });
        }
        Ok(RunOutcome::Cancelled) => {}
        Err(message) => {
            let _ = app.emit(
                "ai:error",
                AiError {
                    session_id,
                    message: message.clone(),
                },
            );
        }
    }
    outcome.map(|_| ())
}

/// 一轮 AI 对话：准备请求上下文，执行 agent 循环，把历史经租约写回。
/// 返回循环终态；事件下发（ai:done/ai:error）由调用方统一处理。
async fn chat(
    app: &AppHandle,
    db: &Db,
    sessions: &SessionManager,
    agents: &AgentManager,
    req: ChatRequest,
) -> Result<RunOutcome, String> {
    let host = sessions
        .host(req.session_id)
        .ok_or_else(|| t("会话不存在或已断开", "Session does not exist or is disconnected").to_string())?;
    // 同主机只允许一个 agent 循环：历史按主机共享，两个会话各自读副本→整段写回会互相覆盖。
    // 租约在函数返回（含任何 ? 提前返回）时自动释放登记。
    let (lease, rx) = agents
        .acquire_run(&host.id, req.session_id)
        .await
        .ok_or_else(|| {
            t(
                "该主机已有正在进行的 AI 对话（可能在另一个标签页），请等其完成或停止后再发送",
                "This host already has an AI conversation in progress (possibly in another tab); wait for it to finish or stop it before sending",
            )
            .to_string()
        })?;
    let (provider, model_info) = crate::ai::resolve_active_ai_model(db)?;
    let model = model_info.model.clone();
    let context_window = model_info.context_window;
    validate_images(&req.images, model_info.supports_vision)?;
    eprintln!(
        "[agent] {}: {}（{}，{} {} tokens）",
        t("使用模型", "using model"),
        model,
        provider.name,
        t("窗口", "window"),
        context_window
    );
    let api_key = credentials::get_api_key(&provider.id)
        .ok_or_else(|| t("API Key 未找到，请在 AI 配置中检查", "API Key not found; check your AI settings").to_string())?;
    let danger_rules: Vec<String> = db
        .list_ai_rules()
        .map_err(|e| {
            format!(
                "{}: {e}",
                t("读取智能审核规则失败", "Failed to read smart approval rules")
            )
        })?
        .into_iter()
        .map(|r| r.pattern)
        .collect();

    // 不能用整体 timeout：流式回复的合法总时长可能远超 120s。
    // read_timeout 限定的是相邻数据块之间的最长空闲间隔，而不是整个响应的总时长。
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| {
            format!(
                "{}: {e}",
                t("HTTP 客户端初始化失败", "Failed to initialize the HTTP client")
            )
        })?;
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    spawn_metric_snapshot(app, &host);

    let mut history = agents.history(&host.id);
    // 上次对话被中断时历史里可能留下没有 tool 结果的 assistant.tool_calls，
    // 先补齐占位结果，否则 OpenAI 兼容平台会直接拒绝本次请求（400）
    repair_dangling_tool_calls(&mut history);
    let system = system_prompt(&host, &provider, &model, req.permission_mode);
    refresh_system_prompt(&mut history, system);
    // 有图片时 user.content 组为 OpenAI 多模态数组（text + image_url 块），
    // 否则保持纯文本不变，避免对不支持数组 content 的平台引入兼容问题。
    history.push(serde_json::json!({
        "role": "user",
        "content": user_content(&req.message, &req.images),
    }));

    let russh = app.state::<RusshManager>();
    let mut run = AgentRun::new(
        RunCtx {
            app,
            client: &client,
            url: &url,
            api_key: &api_key,
            model: &model,
            context_window,
            host: &host,
            session_id: req.session_id,
            permission_mode: req.permission_mode,
            danger_rules: &danger_rules,
            db,
            russh: &russh,
            agents,
            lease: &lease,
        },
        rx,
        history,
    );
    let outcome = run.run().await;
    lease.commit_history(run.into_history());
    outcome
}

/// 图片附件校验：模型需开启「支持图片」；data URL 形式 + 条数/体积上限，
/// 防止异常大附件直接把上下文打满或触发平台体积限制。
fn validate_images(images: &[String], supports_vision: bool) -> Result<(), String> {
    if images.is_empty() {
        return Ok(());
    }
    if !supports_vision {
        return Err(t(
            "当前模型未开启「支持图片」，请在 AI 配置中勾选该模型的图片能力后再试",
            "Image support is not enabled for the current model; enable it for this model in AI settings and try again",
        )
        .to_string());
    }
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(if is_en() {
            format!("At most {MAX_IMAGES_PER_MESSAGE} images per message")
        } else {
            format!("一次最多附带 {MAX_IMAGES_PER_MESSAGE} 张图片")
        });
    }
    if images.iter().any(|i| !i.starts_with("data:image/")) {
        return Err(t(
            "图片格式无效：仅支持 data:image/* 形式",
            "Invalid image format: only data:image/* is supported",
        )
        .to_string());
    }
    if images.iter().any(|i| i.len() > MAX_IMAGE_DATA_URL_CHARS) {
        return Err(t(
            "单张图片过大（超过约 5MB），请压缩后再发送",
            "An image is too large (over ~5MB); compress it before sending",
        )
        .to_string());
    }
    Ok(())
}

/// 构造 user 消息的 content：有图片时组为 OpenAI 多模态数组（text + image_url 块），
/// 否则保持纯文本不变，避免对不支持数组 content 的平台引入兼容问题。
fn user_content(message: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::json!(message);
    }
    let mut parts: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
    if !message.trim().is_empty() {
        parts.push(serde_json::json!({"type": "text", "text": message}));
    }
    for url in images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {"url": url},
        }));
    }
    serde_json::json!(parts)
}

/// 把系统提示词写进历史：空历史直接推入；首条是 system 则替换内容
/// （会话中途切换平台/模型时，同步刷新系统提示词中的身份描述）。
fn refresh_system_prompt(history: &mut Vec<serde_json::Value>, system: String) {
    if history.is_empty() {
        history.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    } else if let Some(first) = history.first_mut() {
        if first.get("role").and_then(|r| r.as_str()) == Some("system") {
            first["content"] = serde_json::json!(system);
        }
    }
}

/// 后台静默采集一份主机快照写入历史指标，让趋势数据随对话自然累积。
/// 不阻塞首个模型请求，失败也不影响对话。
fn spawn_metric_snapshot(app: &AppHandle, host: &Host) {
    let app = app.clone();
    let host = host.clone();
    tauri::async_runtime::spawn(async move {
        let russh = app.state::<RusshManager>();
        let db = app.state::<Arc<Db>>();
        if let Ok(snap) = crate::monitor::collect_russh(&host, &russh).await {
            let _ = crate::monitor::save_metric(&db, &host.id, &snap, "agent");
        }
    });
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
        .ok_or_else(|| t("当前没有等待审批的工具调用", "No tool call is waiting for approval").to_string())?;
    tx.try_send(Control::Approve {
        tool_call_id,
        allow,
    })
    .map_err(|_| t("会话已结束", "The session has ended").to_string())
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
        .ok_or_else(|| {
            t(
                "当前没有等待继续确认的会话",
                "No session is waiting for a continue confirmation",
            )
            .to_string()
        })?;
    tx.try_send(Control::Continue { allow })
        .map_err(|_| t("会话已结束", "The session has ended").to_string())
}

#[tauri::command]
pub fn agent_cancel(agents: State<'_, AgentManager>, session_id: u32) -> Result<(), String> {
    if let Some(tx) = agents.control_sender(session_id) {
        let _ = tx.try_send(Control::Cancel);
    }
    Ok(())
}

#[tauri::command]
pub fn agent_reset(agents: State<'_, AgentManager>, host_id: String) -> Result<(), String> {
    // 取消该主机上正在运行的循环（发起 reset 的标签页未必是正在跑循环的那个），
    // 并递增主机级代数使旧循环在结束时放弃写回历史，同时清空历史与台账
    agents.reset_host(&host_id);
    Ok(())
}

/// 把多模态数组 content 拆成纯文本 + 图片列表，供前端恢复气泡展示。
/// 纯字符串 content 原样返回，不额外挂 images 字段。
fn history_entry_for_display(mut msg: serde_json::Value) -> serde_json::Value {
    let Some(parts) = msg["content"].as_array() else {
        return msg;
    };
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<serde_json::Value> = Vec::new();
    for part in parts {
        match part["type"].as_str() {
            Some("text") => {
                if let Some(t) = part["text"].as_str() {
                    texts.push(t.to_string());
                }
            }
            Some("image_url") => {
                if let Some(url) = part["image_url"]["url"].as_str() {
                    images.push(serde_json::json!(url));
                }
            }
            _ => {}
        }
    }
    msg["content"] = serde_json::json!(texts.join("\n\n"));
    if !images.is_empty() {
        msg["images"] = serde_json::json!(images);
    }
    msg
}

#[tauri::command]
pub fn get_history(
    agents: State<'_, AgentManager>,
    host_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    Ok(agents
        .history(&host_id)
        .into_iter()
        .map(history_entry_for_display)
        .collect())
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
    let plan_block = agents.plan_block(&host_id);
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
    Ok(view.usage.calibrated(factor))
}

/// 单个工具审批的等待上限（秒），超时按拒绝处理，不终止会话
const APPROVAL_TIMEOUT_SECS: u64 = 600;

/// 同主机新会话等待上一个 agent 循环释放登记的上限（秒）
const ACTIVE_WAIT_SECS: u64 = 10;

/// 单条用户消息最多附带的图片数。
const MAX_IMAGES_PER_MESSAGE: usize = 6;

/// 单张图片 data URL 的最大字符数（约 5MB 二进制 → 6.7MB base64）。
const MAX_IMAGE_DATA_URL_CHARS: usize = 7_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lease_is_exclusive_per_host() {
        let agents = AgentManager::default();
        let (lease, _rx) = agents
            .try_acquire_run("h1", 1)
            .expect("首次登记应成功");
        assert!(
            agents.try_acquire_run("h1", 2).is_none(),
            "同主机第二个会话应被拒绝"
        );
        // 不同主机互不影响
        assert!(agents.try_acquire_run("h2", 3).is_some());
        drop(lease);
        assert!(
            agents.try_acquire_run("h1", 2).is_some(),
            "释放后应能重新登记"
        );
    }

    #[test]
    fn control_sender_follows_run_lease() {
        let agents = AgentManager::default();
        let (lease, _rx) = agents.try_acquire_run("h1", 7).unwrap();
        assert!(agents.control_sender(7).is_some());
        assert!(agents.control_sender(8).is_none());
        drop(lease);
        assert!(agents.control_sender(7).is_none());
    }

    #[test]
    fn reset_host_cancels_run_and_voids_its_writes() {
        let agents = AgentManager::default();
        let (lease, mut rx) = agents.try_acquire_run("h1", 1).unwrap();
        assert!(lease.commit_history(vec![serde_json::json!({
            "role": "user",
            "content": "a"
        })]));
        assert_eq!(
            agents.history("h1"),
            vec![serde_json::json!({"role": "user", "content": "a"})]
        );
        agents.reset_host("h1");
        assert!(matches!(rx.try_recv(), Ok(Control::Cancel)));
        // reset 之后旧租约的写回全部被拒
        assert!(!lease.commit_history(vec![serde_json::json!({
            "role": "user",
            "content": "b"
        })]));
        assert!(agents.history("h1").is_empty());
        assert!(!lease.set_plan(TaskPlan {
            goal: "x".to_string(),
            ..Default::default()
        }));
        assert!(agents.plan("h1").is_none());
    }

    #[test]
    fn reset_host_clears_idle_conversation() {
        let agents = AgentManager::default();
        {
            let (lease, _rx) = agents.try_acquire_run("h1", 1).unwrap();
            assert!(lease.commit_history(vec![serde_json::json!({
                "role": "user",
                "content": "a"
            })]));
            assert!(lease.set_plan(TaskPlan {
                goal: "x".to_string(),
                ..Default::default()
            }));
        }
        agents.reset_host("h1");
        assert!(agents.history("h1").is_empty());
        assert!(agents.plan("h1").is_none());
        // 新一轮对话照常写回
        let (lease, _rx) = agents.try_acquire_run("h1", 2).unwrap();
        assert!(lease.commit_history(vec![serde_json::json!({
            "role": "user",
            "content": "b"
        })]));
        assert_eq!(agents.history("h1").len(), 1);
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
