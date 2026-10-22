//! Agent 循环：模型往返 → 工具调用 → 结果回填，直到模型给出最终回复、
//! 用户中断或出错。会话/租约状态在 agent.rs，工具定义在 tools.rs，
//! 审批判定在 approval.rs，流式解析在 stream.rs。

use super::approval::approval_reason_with_sensitive;
use super::archive;
use super::context::{build_context_view, CompressionStrategy};
use super::memory::{self, ContextMemory, SummaryCheckpoint};
use super::stream::{SseDecoder, SseEvent, StreamReply, ToolCallAcc};
use super::tools::{execute_tool, parse_args, tools_schema, unknown_tool_message, Tool};
use super::{
    AgentManager, AiPlan, AiRoundsExhausted, AiRoundsResolved, AiStream, AiTool, Control, RunLease,
    ToolState, APPROVAL_TIMEOUT_SECS,
};
use crate::db::Db;
use crate::i18n::{is_en, t};
use crate::models::{AuditLog, Host, PermissionMode, TaskPlan};
use crate::russh::RusshManager;
use crate::safety::{command_has_sensitive_path, sanitize};
use crate::util::{extract_error, now, truncate};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// `AgentRun` 所需的只读上下文，聚合以避免函数参数过多（此前 13 个位置参数，
/// 顺序稍有出错编译器也无法察觉）。字段全部是引用或已实现 Copy 的类型，
/// 因此整体可以 Copy，方便在 &mut self 方法里按值取出使用。
#[derive(Clone, Copy)]
pub(super) struct RunCtx<'a> {
    /// 事件下发用的 Tauri 应用句柄
    pub(super) app: &'a AppHandle,
    /// 复用的 HTTP 客户端（无整体超时，read_timeout 限相邻数据块空闲）
    pub(super) client: &'a reqwest::Client,
    /// chat/completions 端点
    pub(super) url: &'a str,
    /// 平台 API Key
    pub(super) api_key: &'a str,
    /// 底层模型名（同时用作校准/usage 缓存键）
    pub(super) model: &'a str,
    /// 模型上下文窗口（token）
    pub(super) context_window: u32,
    /// 目标主机
    pub(super) host: &'a Host,
    /// 发起对话的前端会话 id（事件路由）
    pub(super) session_id: u32,
    /// 审批权限模式
    pub(super) permission_mode: PermissionMode,
    /// 智能模式下的自定义危险规则
    pub(super) danger_rules: &'a [String],
    /// 本地数据库（轮次上限、审计、历史指标）
    pub(super) db: &'a Db,
    /// SSH 执行层
    pub(super) russh: &'a RusshManager,
    /// 跨命令共享的 agent 状态（历史/台账/校准等）
    pub(super) agents: &'a AgentManager,
    /// 本轮独占租约：写回历史/台账需经其校验代数
    pub(super) lease: &'a RunLease<'a>,
}

/// agent 循环的终态，决定 agent_chat 下发哪个收尾事件。
pub(super) enum RunOutcome {
    /// 模型给出最终回复，或在轮次上限处选择停止/等待超时 → agent_chat 发 ai:done
    Done,
    /// 用户中断：前端点「停止」时已自行复位，不发 ai:done，避免误伤紧随其后的新一轮
    Cancelled,
}

/// 达到轮次上限后的处理结果。
enum ContinueDecision {
    /// 用户选择继续，追加下一批轮次
    Continue,
    /// 用户选择停止或等待超时，正常结束本轮执行
    Stop,
    /// 用户中断（前端已自行复位，不发 ai:done）
    Cancelled,
}

/// 单次审批等待的结果。
enum ApprovalDecision {
    /// 用户批准
    Approved,
    /// 用户拒绝或等待超时（note 随 denied 事件与审计记录展示）
    Denied {
        /// 拒绝原因（审批原因或超时文案）
        note: Option<String>,
        /// 是否因等待超时按拒绝处理（决定回填给模型的文案）
        timed_out: bool,
    },
    /// 等待期间被用户中断
    Cancelled,
}

/// 一次工具调用处理完后的流向。
enum CallFlow {
    /// 已回填恰好一条 tool 消息，继续处理下一个调用
    Next,
    /// 本调用未回填，由调用方为本调用及其后所有调用回填 note
    Interrupted {
        /// 回填给模型的中断说明
        note: &'static str,
    },
    /// 本调用已自行回填（命令已执行完的归档取消路径），
    /// 由调用方为「其后」所有调用回填 note 并结束本轮
    InterruptedAfter {
        /// 回填给模型的中断说明
        note: &'static str,
    },
}

/// 工具调用向 `ai:tool` 事件映射的状态（附带各状态需要的数据）。
enum ToolEvent {
    /// 请求审批，附带审批原因（随卡片展示）
    Request {
        /// 审批原因（命中规则 / 内置危险 / 模型标记 / 全部审核）
        reason: String,
    },
    /// 被拒绝/超时/中断，附带展示原因
    Denied {
        /// 拒绝原因
        reason: Option<String>,
    },
    /// 开始执行
    Running,
    /// 执行成功，附带工具输出
    Result(String),
    /// 执行或前置校验失败，附带错误文本
    Error(String),
}

/// 带固定截止时间的控制消息等待结果。
enum Waited<T> {
    /// 等到一条匹配的消息
    Got(T),
    /// 截止时间已到（不匹配/过期的消息不会顺延截止）
    TimedOut,
    /// 用户中断或控制通道关闭
    Cancelled,
}

/// 一次工具调用的预处理结果：流式分片聚合后统一在这里解析参数与工具名，
/// 后续审批、执行、审计、历史写回都使用解析后的同一份值。
struct PreparedCall {
    /// 工具调用 id：模型漏发时按流内序号补的本地 id（call_{index}）
    id: String,
    /// 写入历史/展示：能识别时为规范名，否则模型原始名（可能为空）
    name: String,
    /// 原样写回历史的 arguments 原文
    raw_args: String,
    /// parse_args 结果：Err 时回填错误让模型重发
    args: Result<Value, String>,
    /// 参数可解析且可识别时的工具枚举
    tool: Option<Tool>,
}

impl PreparedCall {
    /// 工具卡片展示用兜底名（模型漏填 name 时前端不至于空白标题）
    fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            "unknown"
        } else {
            self.name.as_str()
        }
    }
}

/// 把流式聚合出的 tool_calls 整理成待执行列表（按 index 顺序）。
/// 丢弃空壳工具调用：既没有工具名也没有任何参数信号（部分模型会输出完全空的占位调用），
/// 这类调用既无法推断也不能执行，直接忽略，避免回填“未知工具”错误让模型原地打转。
/// 注意：只要 args 是一段能解析出来的 JSON（哪怕是 "{}"），就说明模型确实发出过参数增量，
/// 不能仅因为对象内容为空就当成占位调用丢弃——resource_usage 等无参数工具的合法调用
/// 恰好就是空对象，之前的写法会把这类合法调用误杀，导致模型宣布意图后却静默中断。
fn prepare_tool_calls(calls: BTreeMap<usize, ToolCallAcc>) -> Vec<PreparedCall> {
    let mut out = Vec::new();
    for (index, acc) in calls {
        let has_name = !acc.name.trim().is_empty();
        let args_trimmed = acc.args.trim();
        let has_args_signal =
            !args_trimmed.is_empty() && serde_json::from_str::<Value>(args_trimmed).is_ok();
        if !has_name && !has_args_signal {
            continue;
        }
        let args = parse_args(&acc.args);
        let tool = args
            .as_ref()
            .ok()
            .and_then(|args| Tool::resolve(&acc.name, args));
        // 部分模型不下发 tool_call 的 id：用其流内序号补一个本地 id，
        // 保证审批回执匹配与 tool/tool_call_id 配对消歧
        let id = if acc.id.is_empty() {
            format!("call_{index}")
        } else {
            acc.id
        };
        // 历史里记录规范名：可识别的别名/推断名统一成 canonical 名，
        // 与审批/执行用的是同一个解析结果
        let name = tool
            .map(|tool| tool.as_str().to_string())
            .unwrap_or(acc.name);
        out.push(PreparedCall {
            id,
            name,
            raw_args: acc.args,
            args,
            tool,
        });
    }
    out
}

/// 构造写回历史的 assistant.tool_calls 消息（content + 原始 tool_calls 声明）。
fn assistant_tool_calls_message(content: &str, calls: &[PreparedCall]) -> Value {
    let calls_json: Vec<Value> = calls
        .iter()
        .map(|call| {
            serde_json::json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.raw_args },
            })
        })
        .collect();
    serde_json::json!({
        "role": "assistant",
        "content": content,
        "tool_calls": calls_json,
    })
}

/// 追加一条 tool 结果消息。content 在这里逐字保留：长输出在写入前已由
/// store_tool_output 换成「有界预览 + 归档引用」，归档失败时完整原文留在
/// canonical 历史里（发送视图仍可按预算裁剪，不回读这条消息本体）。
fn push_tool_message(history: &mut Vec<Value>, tool_call_id: &str, content: &str) {
    history.push(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content,
    }));
}

/// 中断/会话结束时为尚未回填结果的 tool_call 补一条 tool 消息，
/// 否则历史里会留下没有 tool 结果的 assistant.tool_calls，OpenAI 兼容平台会直接拒绝下一次请求（400）。
fn backfill_unanswered(history: &mut Vec<Value>, calls: &[PreparedCall], note: &str) {
    for call in calls {
        push_tool_message(history, &call.id, note);
    }
}

/// 工具执行失败时回填给模型的 tool 消息文本。
fn failure_note(msg: &str) -> String {
    format!("{}: {msg}", t("执行失败", "Execution failed"))
}

/// 摘要不可用时的模型可见回退说明：改用确定性规则摘录，原文仍可回查。
fn summary_fallback_warning() -> String {
    t(
        "摘要不可用，正在使用规则摘录的上下文。原始证据仍可通过 recall_context 回查。",
        "Summary unavailable; rule-extracted context is being used. \
         Original evidence remains available through recall_context.",
    )
    .to_string()
}

/// 归档等待被中断时的说明：命令已真实执行完成，按归档失败口径
/// 把完整捕获留在 canonical 历史，不丢已产生的输出证据。
fn archive_interrupted_note() -> String {
    t(
        "归档已中断；完整捕获已保留在会话历史中。",
        "Archiving was interrupted; the complete capture is retained \
         in conversation history.",
    )
    .to_string()
}

/// 等待 `fut` 完成；期间收到 Cancel 或控制通道关闭时返回 None。
/// 过期的审批/继续回执直接忽略，避免静默终止本轮。
async fn until_cancelled<F: Future>(rx: &mut mpsc::Receiver<Control>, fut: F) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            msg = rx.recv() => match msg {
                Some(Control::Cancel) | None => return None,
                Some(Control::Approve { .. }) | Some(Control::Continue { .. }) => {}
            },
        }
    }
}

/// 等待一条匹配的控制消息，带固定超时：截止时间只算一次，
/// 过期/不匹配的消息不会重置倒计时，与前端审批卡片的倒计时口径一致。
/// Cancel 与通道关闭都视为中断（通道被租约持有，None 实际不可达，防御性处理）。
async fn wait_for_control<T>(
    rx: &mut mpsc::Receiver<Control>,
    timeout: Duration,
    mut pick: impl FnMut(Control) -> Option<T>,
) -> Waited<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return Waited::TimedOut,
            Ok(None) | Ok(Some(Control::Cancel)) => return Waited::Cancelled,
            Ok(Some(msg)) => {
                if let Some(v) = pick(msg) {
                    return Waited::Got(v);
                }
            }
        }
    }
}

/// 把 reqwest 的发送错误翻译成用户可读消息。
fn describe_request_error(e: &reqwest::Error) -> String {
    let detail = e.to_string();
    if e.is_timeout() {
        if is_en() {
            format!("Timed out waiting for the AI platform (no data within 120s); the model may be slow or the network unstable: {detail}")
        } else {
            format!("等待 AI 平台响应超时（120 秒内无数据），模型可能响应过慢或网络不畅: {detail}")
        }
    } else if e.is_connect() {
        format!(
            "{}: {detail}",
            t(
                "无法连接 AI 平台，请检查网络或 Base URL",
                "Cannot reach the AI platform; check your network or Base URL"
            )
        )
    } else {
        format!(
            "{}: {detail}",
            t("请求 AI 平台失败", "Request to the AI platform failed")
        )
    }
}

/// 平台报上下文超限并降窗重试后，随 ai:context 下发的提示文案。
fn reactive_reduce_warning(window: u32) -> String {
    if is_en() {
        format!("The platform reported a context overflow; retried with 50% of the configured window ({window} tokens). If it still fails, increase this model's context_window")
    } else {
        format!("平台报告上下文超限，已按配置窗口的 50%（{window} tokens）重试；若仍失败请调大该模型的 context_window")
    }
}

impl RunCtx<'_> {
    /// 统一发出 `ai:tool` 事件，避免 request/denied/running/result/error 五种状态各自重复构造事件体。
    /// reason / timeout_secs 仅审批相关状态（request/denied）携带，其余状态为 None。
    fn emit_tool(&self, call: &PreparedCall, args: &Value, event: ToolEvent) {
        let (state, output, reason, timeout_secs) = match event {
            ToolEvent::Request { reason } => (
                ToolState::Request,
                None,
                Some(reason),
                Some(APPROVAL_TIMEOUT_SECS),
            ),
            ToolEvent::Denied { reason } => (ToolState::Denied, None, reason, None),
            ToolEvent::Running => (ToolState::Running, None, None, None),
            // 事件 output 统一走展示投影：归档失败时写回历史的是完整捕获，
            // 但 IPC/前端气泡必须始终有界（canonical 不丢，界面不炸）
            ToolEvent::Result(output) => (
                ToolState::Result,
                Some(archive::display_text(&output)),
                None,
                None,
            ),
            ToolEvent::Error(output) => (
                ToolState::Error,
                Some(archive::display_text(&output)),
                None,
                None,
            ),
        };
        let _ = self.app.emit(
            "ai:tool",
            AiTool {
                session_id: self.session_id,
                tool_call_id: call.id.clone(),
                name: call.display_name().to_string(),
                args: args.clone(),
                state,
                output,
                reason,
                timeout_secs,
            },
        );
    }

    /// 下发一段正文增量（ai:stream）
    fn emit_stream(&self, delta: &str) {
        let _ = self.app.emit(
            "ai:stream",
            AiStream {
                session_id: self.session_id,
                delta: delta.to_string(),
            },
        );
    }

    /// 写一条工具调用审计；审计写入失败只静默忽略，不影响对话主流程。
    fn audit(
        &self,
        tool_name: &str,
        args: &Value,
        approval: &str,
        status: &str,
        result: Option<String>,
        started: Instant,
    ) {
        let _ = insert_audit(
            self.db,
            AuditEntry {
                session_id: self.session_id,
                host: self.host,
                tool_name,
                args,
                permission_mode: self.permission_mode.as_str(),
                approval,
                status,
                result,
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
}

/// 一轮对话的 agent 循环：持有控制通道接收端与本轮历史副本，
/// 结束时由调用方把 `into_history()` 的结果经租约校验代数后写回。
pub(super) struct AgentRun<'a> {
    /// 本轮的只读上下文（应用句柄、HTTP 客户端、模型、主机、租约等）
    ctx: RunCtx<'a>,
    /// 控制通道接收端：审批回执、继续确认与用户中断经此传入
    rx: mpsc::Receiver<Control>,
    /// 本轮历史副本（含中断回填），结束时经租约校验代数后写回
    history: Vec<Value>,
    /// 本轮压缩记忆工作副本（启动时克隆自会话；摘要尝试只改副本，
    /// 结束时与历史一起经租约原子写回，中途取消不产生半提交状态）
    memory: ContextMemory,
}

impl<'a> AgentRun<'a> {
    pub(super) fn new(
        ctx: RunCtx<'a>,
        rx: mpsc::Receiver<Control>,
        history: Vec<Value>,
        memory: ContextMemory,
    ) -> Self {
        AgentRun {
            ctx,
            rx,
            history,
            memory,
        }
    }

    /// 取出本轮累积的历史副本与压缩记忆（含未提交的中断回填），供调用方写回
    pub(super) fn into_state(self) -> (Vec<Value>, ContextMemory) {
        (self.history, self.memory)
    }

    /// 主循环：每轮一次模型请求 + 依次执行其工具调用，直到模型给出最终回复、
    /// 用户中断或请求出错。每个调用执行后必须恰好回填一条 tool 消息。
    pub(super) async fn run(&mut self) -> Result<RunOutcome, String> {
        // 单批模型往返轮次上限（可在 AI 配置中调整）。达到上限不直接中断会话，
        // 而是暂停并询问是否继续下一批，避免复杂多步运维任务被硬性截断。
        let batch = self
            .ctx
            .db
            .get_ai_max_tool_rounds()
            .unwrap_or_else(|_| crate::models::default_max_tool_rounds())
            .max(1);
        let mut round_limit = batch;
        let mut rounds = 0;
        loop {
            if rounds >= round_limit {
                match self.wait_continue_decision(rounds, batch).await {
                    ContinueDecision::Continue => round_limit += batch,
                    ContinueDecision::Stop => return Ok(RunOutcome::Done),
                    ContinueDecision::Cancelled => return Ok(RunOutcome::Cancelled),
                }
            }
            rounds += 1;
            if matches!(self.rx.try_recv(), Ok(Control::Cancel)) {
                return Ok(RunOutcome::Cancelled);
            }
            let Some(reply) = self.request_completion().await? else {
                return Ok(RunOutcome::Cancelled);
            };
            let calls = prepare_tool_calls(reply.tool_calls);
            if calls.is_empty() {
                let content = if reply.content.is_empty() {
                    t("（模型未返回内容）", "(the model returned no content)").to_string()
                } else {
                    reply.content
                };
                self.history
                    .push(serde_json::json!({"role": "assistant", "content": content}));
                return Ok(RunOutcome::Done);
            }
            self.history
                .push(assistant_tool_calls_message(&reply.content, &calls));
            // 不变量：每次迭代推进前都必须为 calls[i] 恰好补一条 tool 消息，
            // 否则历史里会留下没有结果的 tool_call，下一次请求会被平台拒绝（400）。
            // 因此本循环内的所有提前返回都要先 backfill 剩余调用。
            for (i, call) in calls.iter().enumerate() {
                let flow = if matches!(self.rx.try_recv(), Ok(Control::Cancel)) {
                    CallFlow::Interrupted {
                        note: t(
                            "用户已中断，该工具未执行",
                            "Interrupted by the user; this tool was not executed",
                        ),
                    }
                } else {
                    self.handle_tool_call(call).await
                };
                match flow {
                    CallFlow::Interrupted { note } => {
                        backfill_unanswered(&mut self.history, &calls[i..], note);
                        return Ok(RunOutcome::Cancelled);
                    }
                    CallFlow::InterruptedAfter { note } => {
                        backfill_unanswered(&mut self.history, &calls[i + 1..], note);
                        return Ok(RunOutcome::Cancelled);
                    }
                    CallFlow::Next => {}
                }
            }
        }
    }

    /// 达到工具轮次上限后暂停，下发确认事件并等待用户选择（超时按停止处理）。
    /// 期间收到 Cancel 或通道关闭按中断处理，保证「停止」按钮始终生效。
    async fn wait_continue_decision(&mut self, rounds_done: u32, batch: u32) -> ContinueDecision {
        let ctx = self.ctx;
        let _ = ctx.app.emit(
            "ai:rounds-exhausted",
            AiRoundsExhausted {
                session_id: ctx.session_id,
                rounds_done,
                batch,
                timeout_secs: APPROVAL_TIMEOUT_SECS,
            },
        );
        let (decision, timed_out) = match wait_for_control(
            &mut self.rx,
            Duration::from_secs(APPROVAL_TIMEOUT_SECS),
            |msg| match msg {
                Control::Continue { allow } => Some(allow),
                _ => None,
            },
        )
        .await
        {
            Waited::Got(true) => (ContinueDecision::Continue, false),
            Waited::Got(false) => (ContinueDecision::Stop, false),
            Waited::TimedOut => (ContinueDecision::Stop, true),
            Waited::Cancelled => (ContinueDecision::Cancelled, false),
        };
        let _ = ctx.app.emit(
            "ai:rounds-resolved",
            AiRoundsResolved {
                session_id: ctx.session_id,
                continued: matches!(decision, ContinueDecision::Continue),
                timed_out,
            },
        );
        decision
    }

    /// 发送一轮 chat/completions 请求并读取流式响应（含上下文超限降窗、
    /// stream_options 不支持两类一次性重试）。Ok(None) 表示发送/读取期间被中断。
    async fn request_completion(&mut self) -> Result<Option<StreamReply>, String> {
        let ctx = self.ctx;
        let tools = tools_schema();
        let plan_block = ctx.agents.plan_block(&ctx.host.id);
        let mut effective_window = ctx.context_window;
        let mut reactive_retried = false;
        // 首次尝试携带 stream_options.include_usage；平台拒绝时去掉并记住，后续请求不再带。
        let mut include_usage = !ctx.agents.usage_unsupported(ctx.model);
        let mut usage_param_retried = false;

        // 视图请求压缩新前缀时，本轮至多一次辅助摘要：成功安装检查点，
        // 失败/被跳过则记录 attempted + warning 走确定性摘录回退；
        // 已折叠边界总是推进到请求的切点（identical 前缀不会重复请求）。
        let factor = ctx.agents.calibration_factor(ctx.model);
        let requested = build_context_view(
            &self.history,
            plan_block.as_deref(),
            &tools,
            ctx.context_window,
            ctx.session_id,
            factor,
            Some(&self.memory),
        )?
        .compact_through;
        if let Some(cutoff) = requested {
            if memory::needs_summary(&self.memory, Some(cutoff))
                && self.attempt_summary(cutoff).await.is_none()
            {
                // 等待摘要期间被中断：不再发正常模型请求
                return Ok(None);
            }
            self.memory.compacted_through = self.memory.compacted_through.max(cutoff);
        }

        // 发送前构建视图；平台报上下文超限时按 50% 窗口重试一次。
        let (resp, mut usage, raw_estimate) = loop {
            // 校准系数同时参与预算判定与展示折算，保证「圆环显示」与「压缩时机」一致
            let factor = ctx.agents.calibration_factor(ctx.model);
            let view = build_context_view(
                &self.history,
                plan_block.as_deref(),
                &tools,
                effective_window,
                ctx.session_id,
                factor,
                Some(&self.memory),
            )?;
            let mut usage = view.usage;
            let raw_estimate = usage.used_tokens;
            if reactive_retried {
                usage.strategy = CompressionStrategy::ReactiveReduce;
                usage.warning = Some(reactive_reduce_warning(effective_window));
            }
            // 平台不返回 usage 时，用该模型的历史校准系数修正本地估算。
            let usage = usage.calibrated(factor);
            let mut body = serde_json::json!({
                "model": ctx.model,
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
            let send = ctx
                .client
                .post(ctx.url)
                .bearer_auth(ctx.api_key)
                .json(&body)
                .send();
            let Some(resp) = until_cancelled(&mut self.rx, send).await else {
                return Ok(None);
            };
            let resp = resp.map_err(|e| describe_request_error(&e))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                if include_usage
                    && !usage_param_retried
                    && is_unsupported_stream_options_error(&text)
                {
                    usage_param_retried = true;
                    include_usage = false;
                    ctx.agents.mark_usage_unsupported(ctx.model);
                    continue;
                }
                if !reactive_retried && is_context_length_error(&text) {
                    reactive_retried = true;
                    effective_window = (effective_window / 2).max(1);
                    continue;
                }
                return Err(extract_error(&text, status));
            }
            break (resp, usage, raw_estimate);
        };
        // 本轮实际发送的压缩前缀可能深于摘要请求的切点（应急削减/降窗重试）：
        // 折叠边界与「真正发送的视图」保持一致，不触发新的摘要请求。
        self.memory.compacted_through = self.memory.compacted_through.max(usage.compressed_rounds);

        let Some(reply) = self.read_stream(resp).await? else {
            return Ok(None);
        };
        if let Some(actual) = reply.reported_prompt_tokens {
            ctx.agents
                .update_calibration(ctx.model, actual, raw_estimate);
            usage.used_tokens = actual;
            usage.estimated = false;
            usage.calibrated = false;
        }
        let _ = ctx.app.emit("ai:context", usage);
        Ok(Some(reply))
    }

    /// 读取 SSE 流式响应；Ok(None) 表示读取中被用户中断。
    async fn read_stream(
        &mut self,
        resp: reqwest::Response,
    ) -> Result<Option<StreamReply>, String> {
        let ctx = self.ctx;
        let mut decoder = SseDecoder::default();
        let mut reply = StreamReply::default();
        let mut stream = resp.bytes_stream();
        // select! 让「停止」能中断阻塞中的流式读取，而不是等当前 chunk 返回
        loop {
            let Some(chunk) = until_cancelled(&mut self.rx, stream.next()).await else {
                return Ok(None); // 用户点击停止
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|e| {
                format!(
                    "{}: {e}",
                    t("读取响应流失败", "Failed to read the response stream")
                )
            })?;
            for event in decoder.feed(&chunk) {
                match event {
                    // [DONE] 之后的数据直接忽略，立即返回
                    SseEvent::Done => return Ok(Some(reply)),
                    SseEvent::Data(v) => {
                        if let Some(delta) = reply.apply(&v) {
                            ctx.emit_stream(delta);
                        }
                    }
                }
            }
            if decoder.overflowed() {
                return Err(t(
                    "AI 平台返回的流式数据单行超过 8MB，已中止以保护内存",
                    "A streaming line exceeded 8MB; aborted to protect memory",
                )
                .to_string());
            }
        }
        // 处理流结束时缓冲区中残留的未换行数据，避免丢失最后一段内容
        if let Some(SseEvent::Data(v)) = decoder.finish() {
            if let Some(delta) = reply.apply(&v) {
                ctx.emit_stream(delta);
            }
        }
        Ok(Some(reply))
    }

    /// 处理一次工具调用。返回 Next 表示已为本调用恰好回填一条 tool 消息；
    /// 返回 Interrupted 表示未回填，由调用方为它及其后所有调用统一回填。
    async fn handle_tool_call(&mut self, call: &PreparedCall) -> CallFlow {
        let ctx = self.ctx;
        let started = Instant::now();
        let args = match &call.args {
            Ok(args) => args,
            Err(msg) => {
                // 参数不可解析：不猜测、不执行，回填错误让模型重发，并留审计
                let raw = serde_json::json!({ "raw": truncate(&call.raw_args, 500) });
                ctx.audit(
                    call.display_name(),
                    &raw,
                    "auto",
                    "error",
                    Some(msg.clone()),
                    started,
                );
                ctx.emit_tool(call, &raw, ToolEvent::Error(msg.clone()));
                push_tool_message(&mut self.history, &call.id, &failure_note(msg));
                return CallFlow::Next;
            }
        };

        // 未知工具在审批前直接拒绝：审批、执行、审计用的是同一个解析结果，
        // 未识别的名字没有机会以「执行兜底」的形式混过去
        let Some(tool) = call.tool else {
            let msg = unknown_tool_message(call.display_name());
            log::warn!(
                "[agent] {}: {}, args: {args}",
                t("未知工具调用", "Unknown tool call"),
                call.display_name()
            );
            ctx.audit(
                call.display_name(),
                args,
                "auto",
                "error",
                Some(msg.clone()),
                started,
            );
            ctx.emit_tool(call, args, ToolEvent::Error(msg.clone()));
            push_tool_message(&mut self.history, &call.id, &failure_note(&msg));
            return CallFlow::Next;
        };

        // 任务台账工具：只更新本地状态，不访问远程主机、不参与审批。
        if tool == Tool::UpdateTaskPlan {
            let existing = ctx.agents.plan(&ctx.host.id).unwrap_or_default();
            let plan = parse_task_plan_patch(args, existing);
            // reset 作废本次写回时不发事件，避免陈旧台账盖掉刚清空的前端状态
            if ctx.lease.set_plan(plan.clone()) {
                let _ = ctx.app.emit(
                    "ai:plan",
                    AiPlan {
                        session_id: ctx.session_id,
                        plan,
                    },
                );
            }
            let plan_note = t("计划已更新", "Task plan updated");
            ctx.emit_tool(call, args, ToolEvent::Result(plan_note.to_string()));
            push_tool_message(&mut self.history, &call.id, plan_note);
            return CallFlow::Next;
        }

        // 上下文回查工具：只读本机历史/归档，不审批、不触达远程主机，
        // 必须在审批判定与 execute_tool 之前拦截（execute_tool 对它直接报错）。
        // 本地 IO 走了 spawn_blocking，但等待本身也要响应取消——磁盘/杀毒
        // 扫描卡住时不能让「停止」失效、让旧 run 一直占着租约。
        if tool == Tool::RecallContext {
            match until_cancelled(
                &mut self.rx,
                archive::recall_context(&self.history, &ctx.lease.archive, args),
            )
            .await
            {
                Some(Ok(text)) => {
                    ctx.emit_tool(call, args, ToolEvent::Result(text.clone()));
                    push_tool_message(&mut self.history, &call.id, &text);
                }
                Some(Err(err)) => {
                    ctx.emit_tool(call, args, ToolEvent::Error(err.clone()));
                    push_tool_message(&mut self.history, &call.id, &failure_note(&err));
                }
                None => {
                    return CallFlow::Interrupted {
                        note: t(
                            "用户已中断，该工具未执行",
                            "Interrupted by the user; this tool was not executed",
                        ),
                    };
                }
            }
            return CallFlow::Next;
        }

        // read_file 先解析符号链接真实目标：真实目标敏感时审批/执行都按它处理，
        // 避免 /tmp/link -> /etc/shadow 绕过敏感路径检查。
        let resolved_args = if tool == Tool::ReadFile {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match ctx.russh.resolve_path(ctx.host, path).await {
                Some(real) if real != path => {
                    let mut next = args.clone();
                    next["path"] = serde_json::json!(real);
                    next
                }
                _ => args.clone(),
            }
        } else {
            args.clone()
        };
        let args = &resolved_args;

        // 智能审核下，cat/head/tail/grep 的文件参数先做 realpath 预检：
        // 符号链接指向敏感文件时，不能因为输入路径本身不含敏感片段而自动执行。
        let resolved_sensitive = if tool == Tool::ExecCommand
            && ctx.permission_mode == PermissionMode::Smart
        {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            !command_has_sensitive_path(command)
                && ctx
                    .russh
                    .command_reads_sensitive_path(ctx.host, command)
                    .await
        } else {
            false
        };

        // 判定是否需要审批，审批原因随 request 事件下发给审批卡片展示
        let approval = match approval_reason_with_sensitive(
            ctx.permission_mode,
            tool,
            args,
            ctx.danger_rules,
            resolved_sensitive,
        ) {
            None => "auto",
            Some(reason) => match self.request_approval(call, args, reason).await {
                ApprovalDecision::Approved => "approved",
                ApprovalDecision::Denied { note, timed_out } => {
                    ctx.audit(
                        tool.as_str(),
                        args,
                        "denied",
                        "denied",
                        note.clone(),
                        started,
                    );
                    ctx.emit_tool(call, args, ToolEvent::Denied { reason: note });
                    push_tool_message(
                        &mut self.history,
                        &call.id,
                        if timed_out {
                            t(
                                "等待审批超时，已按拒绝处理",
                                "Approval timed out; treated as denied",
                            )
                        } else {
                            t("用户拒绝执行该操作", "The user denied this operation")
                        },
                    );
                    return CallFlow::Next;
                }
                ApprovalDecision::Cancelled => {
                    return CallFlow::Interrupted {
                        note: t(
                            "用户已中断，该工具未执行",
                            "Interrupted by the user; this tool was not executed",
                        ),
                    };
                }
            },
        };

        ctx.emit_tool(call, args, ToolEvent::Running);
        let cancel = CancellationToken::new();
        let exec = execute_tool(ctx.db, ctx.russh, ctx.host, tool, args, &cancel);
        tokio::pin!(exec);
        let Some(result) = until_cancelled(&mut self.rx, &mut exec).await else {
            // 取消不是简单丢弃 future：先通知工具层（SSH 通道会向远端进程
            // 发 SIGTERM），给它有界的清理时间，再回填未执行的工具结果
            cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut exec).await;
            ctx.emit_tool(
                call,
                args,
                ToolEvent::Error(t("已中断", "Interrupted").into()),
            );
            return CallFlow::Interrupted {
                note: t("用户已中断执行", "Interrupted by the user"),
            };
        };
        match result {
            Ok(output) => {
                // 大输出先归档再回填：历史里只存有界预览 + output:<id> 引用；
                // 归档失败时完整捕获留在历史（不丢数据），附一句说明。
                // 事件经 emit_tool 的展示投影压到有界，多 MB 原文不进 IPC；
                // 审计与历史同口径保留 stored 原文。
                // 归档等待经 until_cancelled：磁盘/杀毒卡住时「停止」立即生效，
                // 旧 run 不会为等一个 spawn_blocking 把租约一直占住。
                let stored = match until_cancelled(
                    &mut self.rx,
                    archive::store_tool_output(&ctx.lease.archive, output.clone()),
                )
                .await
                {
                    Some(stored) => stored,
                    None => {
                        // 命令已在远端真实执行完毕，只是归档等待被中断：
                        // 按归档失败口径把完整捕获留在历史、照常审计并下发
                        // 结果事件，本调用自行回填后由调用方为剩余调用回填。
                        // 后台 spawn_blocking 可能仍写完文件——它属于旧会话
                        // 归档实例，随其生命周期清理，引用进不了新会话。
                        let stored = format!("{output}\n{}", archive_interrupted_note());
                        ctx.audit(
                            tool.as_str(),
                            args,
                            approval,
                            "executed",
                            Some(stored.clone()),
                            started,
                        );
                        ctx.emit_tool(call, args, ToolEvent::Result(stored.clone()));
                        push_tool_message(&mut self.history, &call.id, &stored);
                        return CallFlow::InterruptedAfter {
                            note: t(
                                "用户已中断，该工具未执行",
                                "Interrupted by the user; this tool was not executed",
                            ),
                        };
                    }
                };
                ctx.audit(
                    tool.as_str(),
                    args,
                    approval,
                    "executed",
                    Some(stored.clone()),
                    started,
                );
                ctx.emit_tool(call, args, ToolEvent::Result(stored.clone()));
                push_tool_message(&mut self.history, &call.id, &stored);
            }
            Err(err) => {
                ctx.audit(
                    tool.as_str(),
                    args,
                    approval,
                    "error",
                    Some(err.clone()),
                    started,
                );
                ctx.emit_tool(call, args, ToolEvent::Error(err.clone()));
                push_tool_message(&mut self.history, &call.id, &failure_note(&err));
            }
        }
        CallFlow::Next
    }

    /// 为压缩前缀尝试一次辅助摘要：源构建按预算只取能放得下的最长前缀
    /// [start, end)，成功安装的检查点只覆盖到 end——积压前缀跨轮分批推进；
    /// 一轮都放不下或请求失败时记录 attempted + warning（视图改用确定性
    /// 摘录回退）。返回值 None 表示等待期间被中断——上层直接终止本轮，
    /// 不再发正常模型请求。
    async fn attempt_summary(&mut self, cutoff: usize) -> Option<()> {
        let ctx = self.ctx;
        let factor = ctx.agents.calibration_factor(ctx.model);
        let Some(source) = memory::build_summary_source(
            &self.history,
            self.memory.checkpoint.as_ref(),
            cutoff,
            ctx.context_window,
            factor,
        ) else {
            // 连一轮源材料都放不进摘要输入预算：跳过 API，明确回退；
            // 前缀标记为已尝试，之后每轮请求不会重复构造放不下的包络
            self.memory.attempted_through = self.memory.attempted_through.max(cutoff);
            self.memory.warning = Some(summary_fallback_warning());
            return Some(());
        };
        let through = source.through_round;
        let result = until_cancelled(
            &mut self.rx,
            memory::request_summary(
                ctx.client,
                ctx.url,
                ctx.api_key,
                ctx.model,
                &source,
                ctx.context_window,
            ),
        )
        .await?;
        match result {
            Ok(document) => {
                self.memory.checkpoint = Some(SummaryCheckpoint {
                    through_round: through,
                    document,
                });
                self.memory.attempted_through = self.memory.attempted_through.max(through);
                self.memory.warning = None;
            }
            Err(err) => {
                log::warn!(
                    "[agent] {}: {err}",
                    t("上下文摘要请求失败", "Context summary request failed")
                );
                // 只标到本次实际尝试的覆盖端点：前缀继续增长时会以
                // 更大的候选区间重试，而不是整段永久放弃
                self.memory.attempted_through = self.memory.attempted_through.max(through);
                self.memory.warning = Some(summary_fallback_warning());
            }
        }
        Some(())
    }

    /// 发出审批请求并等待用户决定：超时按拒绝处理，中断时补一条 denied
    /// 事件关闭仍停在 request 态的审批卡片。
    async fn request_approval(
        &mut self,
        call: &PreparedCall,
        args: &Value,
        reason: String,
    ) -> ApprovalDecision {
        let ctx = self.ctx;
        ctx.emit_tool(
            call,
            args,
            ToolEvent::Request {
                reason: reason.clone(),
            },
        );
        match wait_for_control(
            &mut self.rx,
            Duration::from_secs(APPROVAL_TIMEOUT_SECS),
            |msg| match msg {
                Control::Approve {
                    tool_call_id,
                    allow,
                } if tool_call_id == call.id => Some(allow),
                _ => None,
            },
        )
        .await
        {
            Waited::Got(true) => ApprovalDecision::Approved,
            Waited::Got(false) => ApprovalDecision::Denied {
                note: Some(reason),
                timed_out: false,
            },
            Waited::TimedOut => ApprovalDecision::Denied {
                note: Some(
                    t(
                        "等待审批超时，已按拒绝处理",
                        "Approval timed out; treated as denied",
                    )
                    .to_string(),
                ),
                timed_out: true,
            },
            Waited::Cancelled => {
                // 当前调用的审批卡片还停在 request 态（按钮可点），
                // 补一条 denied 让前端关闭它；其余未展示的调用无需发事件
                ctx.emit_tool(
                    call,
                    args,
                    ToolEvent::Denied {
                        reason: Some(t("用户已中断", "Interrupted by the user").to_string()),
                    },
                );
                ApprovalDecision::Cancelled
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
    args: &'a Value,
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
    db.insert_audit_log(&log).map_err(|e| {
        format!(
            "{}: {e}",
            t("写入操作审计失败", "Failed to write the audit log")
        )
    })
}

/// 解析 `update_task_plan` 的参数。采用“patch”语义：请求里出现的字段覆盖旧值，
/// 未出现的字段保留旧值，避免模型漏传某个列表时把已完成/待办整段清空。
fn parse_task_plan_patch(args: &Value, existing: TaskPlan) -> TaskPlan {
    fn list(args: &Value, key: &str, fallback: &[String]) -> Vec<String> {
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
    fn text(args: &Value, key: &str, fallback: &str) -> String {
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
    let mentions_param =
        lower.contains("parameter") || lower.contains("argument") || lower.contains("field");
    let unsupported = lower.contains("unknown")
        || lower.contains("unrecognized")
        || lower.contains("unsupported")
        || lower.contains("extra");
    mentions_param && unsupported
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn acc(id: &str, name: &str, args: &str) -> ToolCallAcc {
        ToolCallAcc {
            id: id.to_string(),
            name: name.to_string(),
            args: args.to_string(),
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(fut)
    }

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
            PreparedCall {
                id: "c1".to_string(),
                name: "exec_command".to_string(),
                raw_args: "{}".to_string(),
                args: Ok(serde_json::json!({})),
                tool: Some(Tool::ExecCommand),
            },
            PreparedCall {
                id: "c2".to_string(),
                name: "exec_command".to_string(),
                raw_args: "{}".to_string(),
                args: Ok(serde_json::json!({})),
                tool: Some(Tool::ExecCommand),
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
    fn prepare_tool_calls_drops_empty_shells_but_keeps_empty_object_args() {
        let mut calls = BTreeMap::new();
        calls.insert(0, acc("", "", ""));
        calls.insert(1, acc("", "  ", ""));
        calls.insert(2, acc("", "", "{}"));
        let prepared = prepare_tool_calls(calls);
        // 前两个是空壳调用被丢弃；"{}" 是合法的 args 信号，按空对象推断为 resource_usage
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].tool, Some(Tool::ResourceUsage));
        assert_eq!(prepared[0].name, "resource_usage");
    }

    #[test]
    fn prepare_tool_calls_keeps_non_object_json_args_as_signal() {
        // "[1]" 能解析为 JSON（虽不是对象），说明模型确实发出过参数增量，不能当空壳丢弃
        let mut calls = BTreeMap::new();
        calls.insert(0, acc("", "", "[1]"));
        let prepared = prepare_tool_calls(calls);
        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].args.is_err());
        assert!(prepared[0].tool.is_none());
    }

    #[test]
    fn prepare_tool_calls_synthesizes_id_and_canonical_name() {
        let mut calls = BTreeMap::new();
        calls.insert(3, acc("", "exec", "{\"command\":\"ls\"}"));
        let prepared = prepare_tool_calls(calls);
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].id, "call_3");
        assert_eq!(prepared[0].name, "exec_command");
        assert_eq!(prepared[0].tool, Some(Tool::ExecCommand));
        assert_eq!(prepared[0].raw_args, "{\"command\":\"ls\"}");
    }

    #[test]
    fn prepare_tool_calls_keeps_unresolvable_name_verbatim() {
        let mut calls = BTreeMap::new();
        calls.insert(0, acc("", "bash", "{\"command\":\"ls\"}"));
        let prepared = prepare_tool_calls(calls);
        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].tool.is_none());
        assert_eq!(prepared[0].name, "bash");
        assert_eq!(prepared[0].display_name(), "bash");
    }

    #[test]
    fn prepare_tool_calls_follows_index_order() {
        let mut calls = BTreeMap::new();
        calls.insert(5, acc("", "exec_command", "{}"));
        calls.insert(1, acc("", "read_file", "{}"));
        let prepared = prepare_tool_calls(calls);
        assert_eq!(prepared[0].name, "read_file");
        assert_eq!(prepared[1].name, "exec_command");
    }

    #[test]
    fn until_cancelled_returns_none_on_cancel_after_stale_receipts() {
        block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            tx.try_send(Control::Approve {
                tool_call_id: "x".to_string(),
                allow: true,
            })
            .unwrap();
            tx.try_send(Control::Continue { allow: true }).unwrap();
            tx.try_send(Control::Cancel).unwrap();
            // 永久 pending 的 future：只能被 Cancel 中断
            let out = until_cancelled(&mut rx, std::future::pending::<()>()).await;
            assert!(out.is_none());
        });
    }

    #[test]
    fn until_cancelled_returns_output_when_future_ready() {
        block_on(async {
            let (_tx, mut rx) = mpsc::channel(8);
            let out = until_cancelled(&mut rx, async { 7 }).await;
            assert_eq!(out, Some(7));
        });
    }

    #[test]
    fn wait_for_control_picks_matching_message() {
        block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            tx.try_send(Control::Approve {
                tool_call_id: "other".to_string(),
                allow: true,
            })
            .unwrap();
            tx.try_send(Control::Continue { allow: true }).unwrap();
            tx.try_send(Control::Approve {
                tool_call_id: "c1".to_string(),
                allow: false,
            })
            .unwrap();
            let got = wait_for_control(&mut rx, Duration::from_secs(60), |msg| match msg {
                Control::Approve {
                    tool_call_id,
                    allow,
                } if tool_call_id == "c1" => Some(allow),
                _ => None,
            })
            .await;
            assert!(matches!(got, Waited::Got(false)));
        });
    }

    #[test]
    fn wait_for_control_times_out() {
        block_on(async {
            let (_tx, mut rx) = mpsc::channel(8);
            let got = wait_for_control(&mut rx, Duration::from_millis(20), |_| None::<()>).await;
            assert!(matches!(got, Waited::TimedOut));
        });
    }

    #[test]
    fn wait_for_control_treats_cancel_and_closed_channel_as_cancelled() {
        block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            tx.try_send(Control::Cancel).unwrap();
            let got = wait_for_control(&mut rx, Duration::from_secs(60), |_| None::<()>).await;
            assert!(matches!(got, Waited::Cancelled));
            drop(tx);
            let got = wait_for_control(&mut rx, Duration::from_secs(60), |_| None::<()>).await;
            assert!(matches!(got, Waited::Cancelled));
        });
    }

    // ---------- 辅助摘要请求的取消 ----------

    fn block_on_all<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn one_round_history() -> Vec<Value> {
        vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "任务"}),
            serde_json::json!({"role": "assistant", "content": "好"}),
        ]
    }

    /// until_cancelled 包住整个摘要请求：服务端收到请求但迟迟不回时取消立即
    /// 返回，不会等到 SUMMARY_TIMEOUT_SECS 超时。回环服务经显式放行信号退出
    /// 并 join——不留悬挂线程。
    #[test]
    fn summary_request_cancelled_while_waiting() {
        block_on_all(async {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/chat/completions", server.server_addr());
            // got：服务端确认收到请求；release：测试放行服务端退出（有界等待）
            let (got_tx, got_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let handle = std::thread::spawn(move || {
                let req = server.recv().unwrap();
                let _ = got_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
                let _ = req.respond(tiny_http::Response::from_string("{}"));
            });
            let source =
                memory::build_summary_source(&one_round_history(), None, 1, 128_000, 1.0).unwrap();
            let (tx, mut rx) = mpsc::channel(8);
            let client = reqwest::Client::new();
            let request = until_cancelled(
                &mut rx,
                memory::request_summary(&client, &url, "k", "m", &source, 128_000),
            );
            tokio::pin!(request);
            // 服务端确认收到请求后再取消：测的是「等待响应中」而不是连接失败。
            // 与 request 并发等待 got 信号（future 必须被轮询才会发请求）——
            // 服务端在放行前不会响应，request 不可能先完成
            let got =
                tokio::task::spawn_blocking(move || got_rx.recv_timeout(Duration::from_secs(5)));
            tokio::pin!(got);
            tokio::select! {
                _ = &mut request => panic!("summary request completed before release"),
                res = &mut got => {
                    res.unwrap()
                        .expect("loopback server must receive the request");
                }
            }
            tx.send(Control::Cancel).await.unwrap();
            let out = request.await;
            assert!(out.is_none(), "等待响应期间取消必须立即返回");
            let _ = release_tx.send(());
            handle.join().expect("loopback server thread must exit");
        });
    }

    /// 响应头已收到、正文只发了一部分就停住：read_bounded_body 必然停在
    /// body 读取阶段（Content-Length 大于实发字节），此时取消立即返回。
    /// 回环 socket 用原始 TcpListener：显式发送响应头 + 部分正文，
    /// 之后等待放行信号退出并 join——不留悬挂线程。
    #[test]
    fn summary_body_read_cancelled_mid_stream() {
        block_on_all(async {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/x", listener.local_addr().unwrap());
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let handle = std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().unwrap();
                sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                // 读完请求头（GET 无 body），再写响应头 + 部分正文
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") && buf.len() < 8_192 {
                    if sock.read(&mut byte).is_err() {
                        break;
                    }
                    buf.push(byte[0]);
                }
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
                sock.write_all(&[b'x'; 128]).unwrap();
                sock.flush().unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            });
            let client = reqwest::Client::new();
            let resp = client.get(&url).send().await.unwrap();
            // 响应头已到 + Content-Length 4096 > 实发 128 →
            // read_bounded_body 必然停在 body 读取阶段
            assert_eq!(resp.status(), 200);
            let (tx, mut rx) = mpsc::channel(8);
            let reading = until_cancelled(&mut rx, memory::read_bounded_body(resp));
            tokio::pin!(reading);
            tx.send(Control::Cancel).await.unwrap();
            let out = reading.await;
            assert!(out.is_none(), "body 读取期间取消必须立即返回");
            let _ = release_tx.send(());
            handle.join().expect("loopback socket thread must exit");
        });
    }

    // ---------- 归档 IO 等待的取消 ----------

    /// 归档写入被 IO 闸门挂住：等待本身经 until_cancelled，「停止」立即返回，
    /// 不会为一个 spawn_blocking 的 JoinHandle 把租约一直占住。
    /// 闸门放行后后台任务自行收尾，不留悬挂等待。
    #[test]
    fn archive_save_wait_is_cancellable() {
        block_on_all(async {
            let archive = Arc::new(archive::OutputArchive::default());
            let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
            archive.install_save_gate_for_test(gate_rx);
            let (tx, mut rx) = mpsc::channel(8);
            // 超过预览上限才会走归档路径
            let output = "o".repeat(crate::agent::context_policy::TOOL_PREVIEW_MAX_CHARS + 1);
            let waiting = until_cancelled(&mut rx, archive::store_tool_output(&archive, output));
            tokio::pin!(waiting);
            tx.send(Control::Cancel).await.unwrap();
            assert!(waiting.await.is_none(), "归档写入卡住时取消必须立即返回");
            let _ = gate_tx.send(());
        });
    }

    /// 归档回查被 IO 闸门挂住：同样立即响应取消。
    #[test]
    fn archive_read_wait_is_cancellable() {
        block_on_all(async {
            let archive = Arc::new(archive::OutputArchive::default());
            let saved = archive.save("x".repeat(100)).await.unwrap();
            let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
            archive.install_read_gate_for_test(gate_rx);
            let (tx, mut rx) = mpsc::channel(8);
            let waiting = until_cancelled(&mut rx, archive.read(&saved.reference, 0, 10));
            tokio::pin!(waiting);
            tx.send(Control::Cancel).await.unwrap();
            assert!(waiting.await.is_none(), "回查读取卡住时取消必须立即返回");
            let _ = gate_tx.send(());
        });
    }

    /// InterruptedAfter 语义：本调用（已执行的归档取消路径）已自行回填，
    /// 调用方只为其后的调用回填——每个 tool_call 仍然恰好一条结果。
    #[test]
    fn interrupted_after_backfills_only_remaining_calls() {
        let mut history = vec![serde_json::json!({
            "role": "assistant", "content": "",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
            ],
        })];
        let calls = [
            PreparedCall {
                id: "c1".to_string(),
                name: "exec_command".to_string(),
                raw_args: "{}".to_string(),
                args: Ok(serde_json::json!({})),
                tool: Some(Tool::ExecCommand),
            },
            PreparedCall {
                id: "c2".to_string(),
                name: "exec_command".to_string(),
                raw_args: "{}".to_string(),
                args: Ok(serde_json::json!({})),
                tool: Some(Tool::ExecCommand),
            },
        ];
        // c1 已执行的输出先回填（归档取消路径的口径）
        push_tool_message(&mut history, "c1", "已执行的完整捕获");
        backfill_unanswered(&mut history, &calls[1..], "用户已中断，该工具未执行");
        let tool_msgs: Vec<&serde_json::Value> = history[1..]
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(tool_msgs.len(), 2, "每个 tool_call 恰好一条结果");
        assert_eq!(tool_msgs[0]["tool_call_id"], "c1");
        assert_eq!(tool_msgs[0]["content"], "已执行的完整捕获");
        assert_eq!(tool_msgs[1]["tool_call_id"], "c2");
        assert_eq!(tool_msgs[1]["content"], "用户已中断，该工具未执行");
    }
}
