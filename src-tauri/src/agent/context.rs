//! 上下文发送视图：canonical history 只追加不裁剪，这里按模型窗口预算生成真正发给模型的
//! messages。预算内逐字发送原始历史；超预算才把旧轮次折叠成证据块（优先已验收的
//! 模型摘要检查点，否则确定性规则摘录），任务锚点、任务台账以 assistant DATA 消息
//! 注入（不当成新指令），保证 tool_call/tool 结果不被拆散。
//! 纯函数模块：不发 HTTP、不读写磁盘，摘要请求与归档由调用方负责。

use serde::Serialize;
use serde_json::{json, Value};
use std::ops::Range;

use super::context_policy as policy;
use super::memory::{summary_sections, ContextMemory, StructuredSummary};
use crate::util::truncate_output;

/// 单轮摘要的最大字符数。
pub(crate) const ROUND_DIGEST_MAX_CHARS: usize = 600;
/// 历史摘要块最多占输入预算扣除固定开销后余量的 25%。
pub(crate) const DIGEST_BUDGET_RATIO: f64 = 0.25;
/// 任务锚点（首条用户消息原文）的最大字符数。
pub(crate) const ANCHOR_MAX_CHARS: usize = 2_000;
/// 应急削减时单条工具结果的最大字符数。
pub(crate) const HARD_RESET_TOOL_MAX_CHARS: usize = 2_000;
/// 每条消息的固定结构开销（role/name/分隔符等）。
pub(crate) const MESSAGE_OVERHEAD_TOKENS: usize = 4;
/// 单个图片块（image_url）的粗估 token：按各视觉平台 1024~1568px 图的高清档
/// 约 1.1k~2k tokens 取中值。base64 原文不计入字符估算，否则一张图会估出几十万假 token。
const IMAGE_PART_TOKENS: usize = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionStrategy {
    None,
    Extractive,
    /// 压缩前缀的证据块来自已验收的模型摘要检查点（未覆盖部分补规则摘录）
    LlmSummary,
    ReactiveReduce,
    HardReset,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextUsage {
    pub session_id: u32,
    /// 本次真正发送给模型的视图估算（含压缩后）。
    pub used_tokens: usize,
    /// 当前完整 canonical history 的估算（未压缩），用于解释“历史已经有多大”。
    pub history_tokens: usize,
    pub budget_tokens: usize,
    pub window_tokens: u32,
    pub compressed_rounds: usize,
    pub strategy: CompressionStrategy,
    pub estimated: bool,
    /// 估算值是否已用平台实测值校准过。
    pub calibrated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ContextUsage {
    /// 按校准系数把本地粗估折算为展示值（系数为 1 时原样返回）
    pub(crate) fn calibrated(mut self, factor: f64) -> Self {
        if factor != 1.0 {
            self.used_tokens = ((self.used_tokens as f64) * factor).round() as usize;
            self.history_tokens = ((self.history_tokens as f64) * factor).round() as usize;
            self.calibrated = true;
        }
        self
    }
}

#[derive(Debug)]
pub(crate) struct ContextView {
    pub messages: Vec<Value>,
    pub usage: ContextUsage,
    /// 请求对 [0, cutoff) 前缀做一次新的摘要压缩（None = 沿用现有记忆或无需压缩）。
    /// 由调用方发起一次辅助摘要请求并更新 ContextMemory，本模块不执行任何 IO。
    pub compact_through: Option<usize>,
}

/// 粗略估算一条消息的 token 数：字符数 × 1.3 + 固定开销。
/// `tool_calls` 是数组，必须序列化后计入（含工具名 + arguments）。
/// 多模态 content（数组）按 text 块计字符、image_url 块计固定估算值。
pub(crate) fn estimate_message_tokens(msg: &Value) -> usize {
    let mut image_parts = 0usize;
    let text = match msg["content"].as_array() {
        Some(parts) => {
            let mut buf = String::new();
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str() {
                            buf.push_str(t);
                        }
                    }
                    Some("image_url") => image_parts += 1,
                    _ => {}
                }
            }
            buf
        }
        None => msg["content"].as_str().unwrap_or("").to_string(),
    };
    let tool_call_id = msg["tool_call_id"].as_str().unwrap_or("");
    let name = msg["name"].as_str().unwrap_or("");
    let tool_calls = if msg["tool_calls"].is_array() {
        serde_json::to_string(&msg["tool_calls"]).unwrap_or_default()
    } else {
        String::new()
    };
    let total_chars = text.chars().count()
        + tool_calls.chars().count()
        + tool_call_id.chars().count()
        + name.chars().count();
    ((total_chars as f64) * 1.3) as usize
        + image_parts * IMAGE_PART_TOKENS
        + MESSAGE_OVERHEAD_TOKENS
}

pub(crate) fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

pub(crate) fn estimate_text_tokens(text: &str) -> usize {
    ((text.chars().count() as f64) * 1.3) as usize
}

fn estimate_tools_tokens(tools: &Value) -> usize {
    estimate_text_tokens(&serde_json::to_string(tools).unwrap_or_default())
}

pub(crate) fn role(msg: &Value) -> &str {
    msg["role"].as_str().unwrap_or("")
}

/// 取消息文本用于摘要/截断：字符串 content 借用返回；多模态数组 content
/// 拼合 text 块，image_url 块折成「[图片]」占位，保证摘要里能看出这轮带过图。
pub(crate) fn content(msg: &Value) -> std::borrow::Cow<'_, str> {
    match msg["content"].as_array() {
        Some(parts) => {
            let mut buf = String::new();
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str() {
                            if !buf.is_empty() {
                                buf.push_str("\n\n");
                            }
                            buf.push_str(t);
                        }
                    }
                    Some("image_url") => {
                        if !buf.is_empty() {
                            buf.push(' ');
                        }
                        buf.push_str(crate::i18n::t("[图片]", "[image]"));
                    }
                    _ => {}
                }
            }
            std::borrow::Cow::Owned(buf)
        }
        None => std::borrow::Cow::Borrowed(msg["content"].as_str().unwrap_or("")),
    }
}

fn clip_head_tail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        truncate_output(s, max)
    }
}

/// 截断消息 content 但保留结构：字符串直接截断；多模态数组逐块处理，
/// text 块截断、image_url 块原样保留（是否保留由图片驻留策略决定，这里不动）。
fn truncate_content_value(value: &Value, max: usize) -> Value {
    match value.as_array() {
        Some(parts) => Value::Array(
            parts
                .iter()
                .map(|p| {
                    if p["type"].as_str() == Some("text") {
                        json!({
                            "type": "text",
                            "text": truncate_output(p["text"].as_str().unwrap_or(""), max),
                        })
                    } else {
                        p.clone()
                    }
                })
                .collect(),
        ),
        None => json!(truncate_output(value.as_str().unwrap_or(""), max)),
    }
}

/// 生成一条消息的图片剥离投影：image_url 块直接替换为「[图片]」文本占位，
/// 不把 base64 原文复制出来再丢弃。其余字段逐字保留。
/// 用于摘要源/回查序列化：发送视图里的带图消息仍逐字发送。
pub(crate) fn message_without_images(msg: &Value) -> Value {
    let Some(obj) = msg.as_object() else {
        return msg.clone();
    };
    let mut out = serde_json::Map::with_capacity(obj.len());
    for (key, value) in obj {
        if key == "content" {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    let content = match msg["content"].as_array() {
        Some(parts) => Value::Array(
            parts
                .iter()
                .map(|part| {
                    if part["type"].as_str() == Some("image_url") {
                        json!({
                            "type": "text",
                            "text": crate::i18n::t("[图片]", "[image]"),
                        })
                    } else {
                        part.clone()
                    }
                })
                .collect(),
        ),
        None => msg["content"].clone(),
    };
    out.insert("content".to_string(), content);
    Value::Object(out)
}

/// 修复历史里悬挂的 tool_call：assistant 消息声明了 tool_calls，但其后连续的
/// tool 消息没有覆盖全部 id（上次对话被中断/会话结束时可能留下这种历史）。
/// OpenAI 兼容平台会拒绝携带未配对 tool_calls 的请求（400），因此为每个未应答的
/// id 补一条占位 tool 消息，插在该 tool 组末尾（无 tool 消息时紧随 assistant）。
pub(crate) fn repair_dangling_tool_calls(history: &mut Vec<Value>) {
    let mut i = 0;
    while i < history.len() {
        if role(&history[i]) != "assistant" {
            i += 1;
            continue;
        }
        let Some(calls) = history[i]["tool_calls"].as_array() else {
            i += 1;
            continue;
        };
        let mut pending: Vec<String> = calls
            .iter()
            .filter_map(|c| c["id"].as_str())
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect();
        // 其后连续的 tool 消息依次应答这些调用
        let mut j = i + 1;
        while j < history.len() && role(&history[j]) == "tool" {
            if let Some(id) = history[j]["tool_call_id"].as_str() {
                pending.retain(|p| p != id);
            }
            j += 1;
        }
        let n = pending.len();
        for (k, id) in pending.into_iter().enumerate() {
            history.insert(
                j + k,
                json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": crate::i18n::t(
                        "（该工具调用未完成，无结果）",
                        "(this tool call did not finish and has no result)",
                    ),
                }),
            );
        }
        // 新插入的是 tool 消息无需再扫，从插入点之后继续
        i = j + n;
    }
}

/// 把历史拆成「system 前缀 + 每轮的消息区间」。一轮 = 一次 assistant 应答 + 其后
/// 连续的 tool 结果（首个 user 消息带起这一轮时一并计入）。边界：遇到 user 消息，
/// 或当前区间已含 assistant 时再遇 assistant 消息，都切新轮——一条 user 驱动的
/// 长任务因此会拆成多个原子交换轮，assistant.tool_calls 与其结果永不拆散。
/// 索引只随尾部追加增长（append-stable），调用方不得折叠最后一个区间。
/// 只返回索引区间不复制消息，视图构建全程借用原历史切片。
pub(crate) fn split_round_ranges(history: &[Value]) -> (usize, Vec<Range<usize>>) {
    let mut start = 0;
    while start < history.len() && role(&history[start]) == "system" {
        start += 1;
    }
    let mut rounds: Vec<Range<usize>> = Vec::new();
    let mut current: Option<usize> = None;
    let mut has_assistant = false;
    for (i, msg) in history.iter().enumerate().skip(start) {
        let r = role(msg);
        if (r == "user" && current.is_some()) || (r == "assistant" && has_assistant) {
            rounds.push(current.take().expect("round in progress")..i);
            has_assistant = false;
        }
        if current.is_none() {
            current = Some(i);
        }
        if r == "assistant" {
            has_assistant = true;
        }
    }
    if let Some(s) = current {
        rounds.push(s..history.len());
    }
    (start, rounds)
}

fn summarize_args(name: &str, raw: &str) -> String {
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let pick = |key: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let value = match name {
        "exec_command" => pick("command"),
        "read_file" | "list_dir" => pick("path"),
        "query_history" => {
            let metric = pick("metric").unwrap_or_else(|| "cpu".to_string());
            let window = parsed
                .as_ref()
                .and_then(|v| v.get("window_hours"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| "168".to_string());
            Some(format!("metric={metric}, window_hours={window}"))
        }
        "update_task_plan" => pick("goal"),
        _ => None,
    };
    clip_head_tail(value.as_deref().unwrap_or(raw), 240)
}

/// 工具结果摘录：只截取原文头尾，不从关键词推断成败——
/// "[exit code 1]" 或 "0 errors" 这类文本的语义判断交给模型/摘要检查点，
/// 确定性摘录一律使用中性标签。
fn tool_result_excerpt(text: &str) -> String {
    clip_head_tail(text, 240)
}

/// 单轮规则摘要：保留命令原文、路径和错误尾部，不做语义概括。
fn round_digest(round: &[Value], index: usize) -> String {
    let en = crate::i18n::is_en();
    let mut parts: Vec<String> = Vec::new();
    for msg in round {
        match role(msg) {
            "user" => parts.push(if en {
                format!(
                    "[round:{index} | user] {}",
                    clip_head_tail(&content(msg), 400)
                )
            } else {
                format!(
                    "[round:{index}｜用户] {}",
                    clip_head_tail(&content(msg), 400)
                )
            }),
            "assistant" => {
                if !content(msg).trim().is_empty() {
                    parts.push(format!(
                        "[{}] {}",
                        if en { "assistant" } else { "助手" },
                        clip_head_tail(&content(msg), 300)
                    ));
                }
                if let Some(calls) = msg["tool_calls"].as_array() {
                    for call in calls {
                        let name = call["function"]["name"]
                            .as_str()
                            .unwrap_or(crate::i18n::t("未知工具", "unknown tool"));
                        let args = call["function"]["arguments"].as_str().unwrap_or("");
                        parts.push(format!(
                            "[{} {name}] {}",
                            if en { "call" } else { "调用" },
                            summarize_args(name, args)
                        ));
                    }
                }
            }
            "tool" => {
                let summary = tool_result_excerpt(&content(msg));
                parts.push(format!(
                    "[{}] {}",
                    if en { "result" } else { "结果" },
                    summary
                ));
            }
            _ => {}
        }
    }
    let text = if parts.is_empty() {
        if en {
            format!("[round:{index}] (no extractable content)")
        } else {
            format!("[round:{index}] （无可提取内容）")
        }
    } else {
        parts.join("\n")
    };
    clip_head_tail(&text, ROUND_DIGEST_MAX_CHARS)
}

/// 最老轮次折叠成一行，避免摘要块无限增长。
fn one_line_digest(round: &[Value], index: usize) -> String {
    let en = crate::i18n::is_en();
    let user = round
        .iter()
        .find(|m| role(m) == "user")
        .map(|m| clip_head_tail(&content(m), 120))
        .unwrap_or_else(|| crate::i18n::t("（无用户消息）", "(no user message)").to_string());
    let outcome = round
        .iter()
        .rev()
        .find(|m| role(m) == "assistant" && !content(m).trim().is_empty())
        .map(|m| clip_head_tail(&content(m), 80))
        .or_else(|| {
            round
                .iter()
                .rev()
                .find(|m| role(m) == "tool")
                .map(|_| crate::i18n::t("工具结果可回查", "tool result available").to_string())
        })
        .unwrap_or_else(|| crate::i18n::t("（无结果）", "(no result)").to_string());
    if en {
        format!("[round:{index}] user: {user} → {outcome}")
    } else {
        format!("[round:{index}] 用户：{user} → {outcome}")
    }
}

/// 组装历史摘录块（不含外框，外层用 history_summary_data 包装）。
/// 超过摘要预算时先折叠最老轮次，再省略最老条目并给出省略数量。
fn build_digest_block(rounds: &[&[Value]], start_index: usize, budget_tokens: usize) -> String {
    if rounds.is_empty() {
        return String::new();
    }
    let mut entries: Vec<String> = rounds
        .iter()
        .enumerate()
        .map(|(i, r)| round_digest(r, start_index + i))
        .collect();
    let mut omitted = 0usize;

    let block = |entries: &[String], omitted: usize| {
        let en = crate::i18n::is_en();
        let mut out = String::new();
        if omitted > 0 {
            out.push_str(&if en {
                format!("[{omitted} earlier rounds omitted]\n")
            } else {
                format!("[更早的 {omitted} 轮已省略]\n")
            });
        }
        out.push_str(&entries.join("\n"));
        out
    };

    // 阶段一：把最老的非一行摘要折叠成一行
    let mut collapsed = vec![false; entries.len()];
    while estimate_text_tokens(&block(&entries, omitted)) > budget_tokens {
        let Some(pos) = collapsed.iter().position(|done| !*done) else {
            break;
        };
        entries[pos] = one_line_digest(rounds[pos], start_index + pos);
        collapsed[pos] = true;
    }
    // 阶段二：仍然超预算时，从最老开始整条省略
    while estimate_text_tokens(&block(&entries, omitted)) > budget_tokens && entries.len() > 1 {
        entries.remove(0);
        collapsed.remove(0);
        omitted += 1;
    }
    block(&entries, omitted)
}

/// assistant DATA 消息：动态状态/摘要/锚点以「引用数据」而非新指令注入。
fn data_message(text: String) -> Value {
    json!({"role": "assistant", "content": text})
}

/// 视图唯一的 system 消息 = 原始系统提示 + 固定的 CONTEXT_DATA_RULE。
fn system_view_text(system: &str) -> String {
    let mut text = system.to_string();
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(policy::CONTEXT_DATA_RULE);
    text
}

/// 原始首条用户请求的任务锚点：只取其文本（图片经 content() 已是占位符），
/// 仅在首轮被压缩出视图时注入；DATA 包装保证不会被当成新的用户指令。
fn task_anchor(history: &[Value], ranges: &[Range<usize>]) -> Option<String> {
    let first = ranges.first()?;
    let user = history[first.clone()].iter().find(|m| role(m) == "user")?;
    let text = clip_head_tail(&content(user), ANCHOR_MAX_CHARS);
    if text.trim().is_empty() {
        None
    } else {
        Some(policy::task_anchor_data(&text))
    }
}

/// 结构化摘要渲染成模型可读的小节文本：每条笔记带 round:<n> 引用，
/// 与 recall_context 的引用格式一致，便于回查原文。
fn render_summary(doc: &StructuredSummary) -> String {
    let mut out = String::new();
    for (name, notes) in summary_sections(doc) {
        if notes.is_empty() {
            continue;
        }
        out.push_str(name);
        out.push_str(":\n");
        for note in notes {
            let refs = note
                .rounds
                .iter()
                .map(|r| format!("round:{r}"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("- {} ({})\n", note.text, refs));
        }
    }
    out.trim_end().to_string()
}

/// 压缩前缀 [0, cutoff) 的证据块：优先渲染已验收的结构化摘要（渲染结果本身
/// 也受证据预算约束——过大的检查点文本退回确定性摘录，但检查点在记忆里照常
/// 保留，不会为了塞进视图而丢弃），未被摘要覆盖的部分用规则摘录补齐；
/// warning 存在时前置说明。digest_budget 为粗估 token 口径的证据总预算，
/// 摘要与缺口摘录共用。
/// 返回 (块内文, 是否含模型生成摘要)。
fn compacted_evidence(
    history: &[Value],
    ranges: &[Range<usize>],
    cutoff: usize,
    memory: Option<&ContextMemory>,
    digest_budget: usize,
) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    let mut generated = false;
    let mut covered = 0usize;
    let mut gap_budget = digest_budget;
    if let Some(mem) = memory {
        if let Some(warning) = &mem.warning {
            parts.push(warning.clone());
        }
        if let Some(cp) = mem
            .checkpoint
            .as_ref()
            .filter(|cp| cp.through_round <= cutoff)
        {
            let block = render_summary(&cp.document);
            if !block.is_empty() && estimate_text_tokens(&block) <= digest_budget {
                gap_budget = digest_budget.saturating_sub(estimate_text_tokens(&block));
                parts.push(block);
                covered = cp.through_round;
                generated = true;
            }
        }
    }
    if covered < cutoff {
        let slices: Vec<&[Value]> = ranges[covered..cutoff]
            .iter()
            .map(|r| &history[r.clone()][..])
            .collect();
        let digests = build_digest_block(&slices, covered, gap_budget);
        if !digests.is_empty() {
            parts.push(digests);
        }
    }
    (parts.join("\n\n"), generated)
}

/// 最新一条 user 消息在历史里的下标（无 user 消息时为 None）。
fn last_user_index(history: &[Value]) -> Option<usize> {
    history.iter().rposition(|m| role(m) == "user")
}

/// 组装前缀压缩视图：system(含数据规则) → 任务锚点 → 历史证据 DATA → 台账 DATA →
/// （最新 user 原文，其所在轮被折叠时）→ [cutoff, len) 轮次逐字原文。
/// 锚点/证据/台账都是 assistant DATA 消息，不会冒充新的用户指令或 system 指令。
/// 返回 (messages, strategy, warning)。
fn compacted_view(
    history: &[Value],
    ranges: &[Range<usize>],
    system_text: &str,
    plan: Option<&str>,
    cutoff: usize,
    memory: Option<&ContextMemory>,
    digest_budget: usize,
) -> (Vec<Value>, CompressionStrategy, Option<String>) {
    let mut messages = vec![json!({"role": "system", "content": system_text})];
    let mut generated = false;
    // 最新 user 消息所在轮是否已被折叠：被折叠就必须在视图里逐字补回，
    // 模型当前要应答的用户输入绝不以摘录/锚点形式出现
    let last_user = last_user_index(history);
    let last_user_compacted = last_user
        .map(|idx| {
            ranges
                .iter()
                .position(|r| r.contains(&idx))
                .map(|r| r < cutoff)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if cutoff > 0 {
        // 首条 user 与最新 user 是同一条时，逐字原文已在下方补回，不再加锚点
        let first_user = ranges
            .first()
            .and_then(|r| history[r.clone()].iter().position(|m| role(m) == "user"))
            .map(|p| ranges[0].start + p);
        if first_user != last_user {
            if let Some(anchor) = task_anchor(history, ranges) {
                messages.push(data_message(anchor));
            }
        }
        let (block, summary_used) =
            compacted_evidence(history, ranges, cutoff, memory, digest_budget);
        generated = summary_used;
        if !block.is_empty() {
            messages.push(data_message(policy::history_summary_data(
                &block, generated,
            )));
        }
    }
    if let Some(p) = plan {
        messages.push(data_message(policy::task_state_data(p)));
    }
    if last_user_compacted {
        messages.push(history[last_user.expect("checked above")].clone());
    }
    for r in &ranges[cutoff..] {
        messages.extend(history[r.clone()].iter().cloned());
    }
    let strategy = if generated {
        CompressionStrategy::LlmSummary
    } else {
        CompressionStrategy::Extractive
    };
    let warning = memory.and_then(|m| m.warning.clone());
    (messages, strategy, warning)
}

/// 应急削减：只保留最后一轮，压缩其中 tool 结果与 assistant 文本。
/// 最新用户请求原文绝不截断；图片块原样保留（由固定估算计价）。
fn truncate_round(round: &[Value], tool_max_chars: usize) -> Vec<Value> {
    round
        .iter()
        .map(|msg| {
            let mut msg = msg.clone();
            match role(&msg) {
                "tool" => {
                    msg["content"] = json!(truncate_output(&content(&msg), tool_max_chars));
                }
                "assistant" => {
                    if content(&msg).chars().count() > ANCHOR_MAX_CHARS {
                        msg["content"] =
                            truncate_content_value(&msg["content"], ANCHOR_MAX_CHARS);
                    }
                }
                _ => {}
            }
            msg
        })
        .collect()
}

/// 按窗口预算生成发送视图。
///
/// 输入预算 = window × INPUT_BUDGET_RATIO；固定开销（system+CONTEXT_DATA_RULE、
/// 台账 DATA 消息、工具 schema）先折算成真实 token。触发线 = 固定开销 + 剩余预算的
/// 80%，目标线 = 固定开销 + 剩余预算的 60%。判定顺序：
/// 1) 全量历史放得下触发线 → 逐字发送（含图片/tool 结果），strategy=None；
/// 2) 已有压缩记忆 → 沿用「折叠前缀 + 之后全部轮次」基线直到它过线（滞回，
///    追加一个小轮次不会每轮触发新摘要）；
/// 3) 过线 → 优先保住最近 RECENT_ROUNDS 个完整轮次，只按目标线需要上推前缀切点
///    （不越过最后一轮，绝不回退），视情况返回 compact_through 请求一次新摘要；
/// 4) 仍超总预算 → 应急削减（最后一轮的 tool/assistant 文本压缩、摘要块省略），
///    最新用户请求与台账绝不截断，放不下就返回明确的 context_window 错误。
///
/// 所有丢弃都以整轮为单位，保证不会出现孤立的 tool 消息或缺失 tool 结果的
/// assistant.tool_calls。canonical 历史本身永不被改动。
///
/// `calibration` 是「平台实测 / 本地估算」的校准系数：本地估算只是按字符数粗估，
/// 预算判定必须先折算成真实 token（`估算 × 系数`），否则圆环展示的是折算后的真实用量、
/// 压缩判定却用粗估，两者会脱节（粗估偏小时用量显示到 90% 仍不触发压缩）。
/// 返回的 `usage.used_tokens / history_tokens` 仍保持未校准的粗估，
/// 由调用方按同一系数折算后展示，避免重复折算。
pub(crate) fn build_context_view(
    history: &[Value],
    plan_block: Option<&str>,
    tools: &Value,
    window: u32,
    session_id: u32,
    calibration: f64,
    memory: Option<&ContextMemory>,
) -> Result<ContextView, String> {
    // 系数来自平台实测的 EMA，这里再做一次兜底钳制，避免异常值把预算算飞
    let calibration = if calibration.is_finite() {
        calibration.clamp(0.25, 4.0)
    } else {
        1.0
    };
    // 本地粗估 → 真实 token 的折算
    let scale = |tokens: usize| -> usize {
        if (calibration - 1.0).abs() < f64::EPSILON {
            tokens
        } else {
            ((tokens as f64) * calibration).round() as usize
        }
    };

    let (sys_end, ranges) = split_round_ranges(history);
    let system = history[..sys_end]
        .iter()
        .map(|m| content(m).into_owned())
        .collect::<Vec<_>>()
        .join("\n\n");
    let system_text = system_view_text(&system);
    let plan = plan_block.map(str::trim).filter(|p| !p.is_empty());
    let tools_tokens = estimate_tools_tokens(tools);
    let full_history_tokens = estimate_messages_tokens(history) + tools_tokens;
    let input_budget = ((window as f64) * policy::INPUT_BUDGET_RATIO) as usize;
    // 固定开销（system+数据规则 + 台账 DATA 消息 + 工具定义）按同一系数只折算一次
    let fixed_raw = estimate_text_tokens(&system_text)
        + plan
            .map(|p| estimate_text_tokens(&policy::task_state_data(p)))
            .unwrap_or(0)
        + tools_tokens
        + 2 * MESSAGE_OVERHEAD_TOKENS;
    let fixed_tokens = scale(fixed_raw);

    if fixed_tokens >= input_budget {
        return Err(if crate::i18n::is_en() {
            format!(
                "The context window configured for the current model is too small ({window} tokens): \
                 the system prompt and tool definitions already take about {fixed_tokens} tokens. \
                 Switch to another model or increase context_window"
            )
        } else {
            format!(
                "当前模型的上下文窗口配置过小（{window} tokens）：系统提示与工具定义已占约 \
                 {fixed_tokens} tokens，请更换模型或调大 context_window"
            )
        });
    }

    let remaining = input_budget - fixed_tokens;
    let trigger = fixed_tokens + ((remaining as f64) * policy::COMPRESS_TRIGGER_RATIO) as usize;
    let target = fixed_tokens + ((remaining as f64) * policy::COMPRESS_TARGET_RATIO) as usize;
    // 证据预算按余量的 25% 分配，但预算比较用的是校准后口径、摘录估算用的是
    // 粗估口径——先把分配的 token 数换回粗估口径再交给摘录/摘要检查
    let unscale = |tokens: usize| -> usize {
        if (calibration - 1.0).abs() < f64::EPSILON {
            tokens
        } else {
            ((tokens as f64) / calibration).round() as usize
        }
    };
    let digest_budget = unscale(((remaining as f64) * DIGEST_BUDGET_RATIO) as usize);

    let usage = |used_tokens: usize,
                 compressed: usize,
                 strategy: CompressionStrategy,
                 warning: Option<String>| {
        ContextUsage {
            session_id,
            used_tokens,
            history_tokens: full_history_tokens,
            budget_tokens: input_budget,
            window_tokens: window,
            compressed_rounds: compressed,
            strategy,
            estimated: true,
            calibrated: false,
            warning,
        }
    };

    // 1) 全量候选：先按借用切片估算（system 包装 + 台账 DATA 包装 + 全部轮次原文
    //    + 工具 schema），放得下触发线才物化消息——否则不复制带 base64 的历史。
    let full_raw = estimate_text_tokens(&system_text)
        + MESSAGE_OVERHEAD_TOKENS
        + plan
            .map(|p| estimate_text_tokens(&policy::task_state_data(p)) + MESSAGE_OVERHEAD_TOKENS)
            .unwrap_or(0)
        + estimate_messages_tokens(&history[sys_end..])
        + tools_tokens;
    if scale(full_raw) <= trigger {
        let mut messages = vec![json!({"role": "system", "content": system_text.clone()})];
        if let Some(p) = plan {
            messages.push(data_message(policy::task_state_data(p)));
        }
        for r in &ranges {
            messages.extend(history[r.clone()].iter().cloned());
        }
        return Ok(ContextView {
            messages,
            compact_through: None,
            usage: usage(full_raw, 0, CompressionStrategy::None, None),
        });
    }

    // 2) 压缩记忆基线：折叠前缀 + 其后的全部轮次。未过触发线就继续用——
    //    追加一个小轮次不该每轮触发新摘要。缓存切点钳到最后一个区间之前，
    //    仍在进行的最后一轮绝不折叠。
    let compacted = memory
        .map(|m| m.compacted_through.min(ranges.len().saturating_sub(1)))
        .unwrap_or(0);
    if compacted > 0 {
        let (messages, strategy, warning) = compacted_view(
            history,
            &ranges,
            &system_text,
            plan,
            compacted,
            memory,
            digest_budget,
        );
        let raw = estimate_messages_tokens(&messages) + tools_tokens;
        if scale(raw) <= trigger {
            return Ok(ContextView {
                messages,
                compact_through: None,
                usage: usage(raw, compacted, strategy, warning),
            });
        }
    }

    // 3) 触发压缩：优先保住最近 RECENT_ROUNDS 个完整轮次，只按目标线需要
    //    把前缀切点上推（不越过 len-1，最后一轮始终原文保留，切点绝不回退）。
    let mut cutoff = compacted
        .max(ranges.len().saturating_sub(policy::RECENT_ROUNDS))
        .max(1);
    while cutoff < ranges.len() {
        let (messages, strategy, warning) = compacted_view(
            history,
            &ranges,
            &system_text,
            plan,
            cutoff,
            memory,
            digest_budget,
        );
        let raw = estimate_messages_tokens(&messages) + tools_tokens;
        let scaled = scale(raw);
        // 达标即收；已到最后一个可行切点时只要不超过总预算也接受
        if scaled <= target || (cutoff + 1 >= ranges.len() && scaled <= input_budget) {
            return Ok(ContextView {
                messages,
                compact_through: (cutoff > compacted).then_some(cutoff),
                usage: usage(raw, cutoff, strategy, warning),
            });
        }
        cutoff += 1;
    }

    // 4) 应急削减：最后一轮的 tool/assistant 文本压缩、历史证据块省略；
    //    最新 user 消息逐字保留（其轮次未被最后一轮覆盖时单独补回），
    //    台账绝不截断，仍放不下就明确报错而不是静默砍头。
    let mut messages = vec![json!({"role": "system", "content": system_text})];
    let last_user = last_user_index(history);
    let last_round = ranges.last();
    let last_user_in_last = last_user
        .map(|idx| last_round.map(|r| r.contains(&idx)).unwrap_or(false))
        .unwrap_or(false);
    if ranges.len() > 1 {
        let first_user = ranges
            .first()
            .and_then(|r| history[r.clone()].iter().position(|m| role(m) == "user"))
            .map(|p| ranges[0].start + p);
        if first_user != last_user {
            if let Some(anchor) = task_anchor(history, &ranges) {
                messages.push(data_message(anchor));
            }
        }
    }
    if let Some(p) = plan {
        messages.push(data_message(policy::task_state_data(p)));
    }
    if !last_user_in_last {
        if let Some(idx) = last_user {
            messages.push(history[idx].clone());
        }
    }
    if let Some(last) = last_round {
        messages.extend(truncate_round(
            &history[last.clone()],
            HARD_RESET_TOOL_MAX_CHARS,
        ));
    }
    let raw = estimate_messages_tokens(&messages) + tools_tokens;
    if scale(raw) <= input_budget {
        return Ok(ContextView {
            messages,
            compact_through: (ranges.len().saturating_sub(1) > compacted)
                .then_some(ranges.len() - 1),
            usage: usage(
                raw,
                ranges.len().saturating_sub(1),
                CompressionStrategy::HardReset,
                Some(
                    crate::i18n::t(
                        "为适配上下文预算仅削减了发送视图；原始历史未改动",
                        "Only the sending view was reduced to fit the context budget; original history is unchanged",
                    )
                    .to_string(),
                ),
            ),
        });
    }

    let used = scale(raw);
    Err(if crate::i18n::is_en() {
        format!(
            "The context window configured for the current model is too small ({window} tokens): \
             even keeping only the system prompt, the task plan and the latest round needs about \
             {used} tokens. Increase this model's context_window or switch to a model with a larger window"
        )
    } else {
        format!(
            "当前模型的上下文窗口配置过小（{window} tokens）：即使只保留系统提示、任务台账和最近一轮，\
             仍需要约 {used} tokens。请调大该模型的 context_window，或更换窗口更大的模型"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(i: usize) -> Vec<Value> {
        vec![
            json!({"role": "user", "content": format!("用户任务{i}")}),
            json!({
                "role": "assistant",
                "content": format!("执行第{i}步"),
                "tool_calls": [{
                    "id": format!("call{i}"),
                    "type": "function",
                    "function": {"name": "exec_command", "arguments": format!("{{\"command\":\"echo step{i}\"}}")}
                }]
            }),
            json!({"role": "tool", "tool_call_id": format!("call{i}"), "content": format!("step{i} 成功")}),
        ]
    }

    fn history(rounds: usize) -> Vec<Value> {
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        for i in 0..rounds {
            h.extend(round(i));
        }
        h
    }

    fn tools() -> Value {
        json!([{"type": "function", "function": {"name": "exec_command"}}])
    }

    /// 极端预算下：原始请求以「任务锚点」DATA 消息（assistant 角色、引用格式）
    /// 保留一次，最后一轮原文保留；锚点不会以 user 身份重新出现。
    #[test]
    fn keeps_system_anchor_and_last_round_under_extreme_budget() {
        let h = history(20);
        let view = build_context_view(&h, None, &tools(), 2_000, 1, 1.0, None).unwrap();
        assert_eq!(view.messages[0]["role"], "system");
        let anchors: Vec<&Value> = view
            .messages
            .iter()
            .filter(|m| m["content"].as_str().unwrap_or("").contains("用户任务0"))
            .collect();
        assert_eq!(anchors.len(), 1, "任务锚点必须恰好出现一次: {anchors:?}");
        assert_eq!(
            anchors[0]["role"], "assistant",
            "锚点是 DATA 消息而非新用户指令"
        );
        let users: Vec<&str> = view
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            !users.contains(&"用户任务0"),
            "首轮用户消息不应重复出现: {users:?}"
        );
        assert!(users.contains(&"用户任务19"), "最后一轮必须保留: {users:?}");
    }

    #[test]
    fn compression_never_breaks_tool_call_pairing() {
        let h = history(20);
        let view = build_context_view(&h, None, &tools(), 8_000, 1, 1.0, None).unwrap();
        let mut pending: Vec<String> = Vec::new();
        for msg in &view.messages {
            if let Some(calls) = msg["tool_calls"].as_array() {
                for call in calls {
                    pending.push(call["id"].as_str().unwrap().to_string());
                }
            }
            if msg["role"] == "tool" {
                let id = msg["tool_call_id"].as_str().unwrap();
                assert!(
                    pending.contains(&id.to_string()),
                    "tool 结果缺少对应调用: {id}"
                );
                pending.retain(|x| x != id);
            }
        }
        assert!(pending.is_empty(), "存在没有结果的 tool_call: {pending:?}");
    }

    #[test]
    fn digest_preserves_command_and_error_tail() {
        let r = vec![
            json!({"role": "user", "content": "部署服务"}),
            json!({
                "role": "assistant",
                "content": "开始",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "exec_command", "arguments": "{\"command\":\"systemctl restart nginx\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "启动中...\nERROR: bind() to 0.0.0.0:80 failed"}),
        ];
        let d = round_digest(&r, 3);
        assert!(
            d.contains("systemctl restart nginx"),
            "必须保留命令原文: {d}"
        );
        assert!(
            d.contains("bind() to 0.0.0.0:80 failed"),
            "必须保留错误尾部: {d}"
        );
        assert!(
            !d.contains("[结果 失败]") && !d.contains("[result failed]"),
            "摘录不做关键词成败推断，只给中性标签: {d}"
        );
    }

    /// 确定性摘录一律使用中性标签：含 "[exit code 1]" 的输出不会被推断成
    /// 「失败」，写着 "0 errors" 的输出也不会被推断成「成功」——语义判断
    /// 交给模型与摘要检查点，摘录只搬运原文。
    #[test]
    fn digest_labels_are_neutral_regardless_of_keywords() {
        let make = |output: &str| {
            vec![
                json!({"role": "assistant", "content": "", "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "exec_command", "arguments": "{}"}
                }]}),
                json!({"role": "tool", "tool_call_id": "c1", "content": output}),
            ]
        };
        let d = round_digest(&make("checking...\n[exit code 1]"), 0);
        assert!(d.contains("[exit code 1]"), "退出码原文必须保留: {d}");
        assert!(
            !d.contains("失败]") && !d.contains("failed]"),
            "exit code 1 不得被推断为失败标签: {d}"
        );
        let d = round_digest(&make("scan complete: 0 errors found"), 0);
        assert!(d.contains("0 errors found"), "结果原文必须保留: {d}");
        assert!(
            !d.contains("成功]") && !d.contains("ok]"),
            "\"0 errors\" 不得被推断为成功标签: {d}"
        );
    }

    #[test]
    fn digest_is_byte_identical_for_same_range() {
        let h = history(12);
        let a = build_context_view(&h, None, &tools(), 4_000, 1, 1.0, None).unwrap();
        let b = build_context_view(&h, None, &tools(), 4_000, 1, 1.0, None).unwrap();
        assert_eq!(a.messages, b.messages, "相同输入必须生成字节一致的视图");
    }

    /// 台账以独立 assistant DATA 消息注入（不是 system 指令的一部分），
    /// 内容完整不被裁剪，也不会被模型摘要替换。
    #[test]
    fn plan_block_is_always_present_and_never_trimmed() {
        let h = history(20);
        let plan = "[任务台账｜必须遵守]\n目标：部署 nginx";
        let view = build_context_view(&h, Some(plan), &tools(), 3_000, 1, 1.0, None).unwrap();
        let sys = view.messages[0]["content"].as_str().unwrap();
        assert!(!sys.contains("部署 nginx"), "台账不应塞进 system 消息");
        let hits: Vec<&Value> = view
            .messages
            .iter()
            .filter(|m| m["content"].as_str().unwrap_or("").contains("部署 nginx"))
            .collect();
        assert_eq!(hits.len(), 1, "台账 DATA 消息应恰好一条: {hits:?}");
        let text = hits[0]["content"].as_str().unwrap();
        assert_eq!(hits[0]["role"], "assistant");
        assert!(text.contains(plan), "台账内容必须完整: {text}");
    }

    #[test]
    fn tiny_window_returns_clear_error() {
        let h = history(2);
        let err = build_context_view(&h, None, &tools(), 40, 1, 1.0, None).unwrap_err();
        assert!(
            err.contains("context_window"),
            "错误应提示调整 context_window: {err}"
        );
    }

    /// 回归：中断后历史里会留下没有 tool 结果的 assistant.tool_calls，
    /// 修复时应为缺失的 id 补占位 tool 消息，且完整历史原样不动。
    #[test]
    fn repair_backfills_missing_tool_results() {
        let mut h = vec![
            json!({"role": "user", "content": "任务"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
                    {"id": "c2", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
            json!({"role": "user", "content": "下一条"}),
        ];
        repair_dangling_tool_calls(&mut h);
        // c2 的占位结果插在该 tool 组末尾、下一条 user 之前
        assert_eq!(h.len(), 5);
        assert_eq!(h[3]["role"], "tool");
        assert_eq!(h[3]["tool_call_id"], "c2");
        assert_eq!(h[4]["role"], "user");

        let mut complete = history(3);
        let before = complete.clone();
        repair_dangling_tool_calls(&mut complete);
        assert_eq!(complete, before, "完整历史不应被改动");
    }

    #[test]
    fn repair_inserts_tool_result_before_next_user() {
        let mut h = vec![
            json!({"role": "user", "content": "任务"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{}"}}]
            }),
            json!({"role": "user", "content": "新问题"}),
        ];
        repair_dangling_tool_calls(&mut h);
        assert_eq!(h.len(), 4);
        assert_eq!(h[2]["role"], "tool");
        assert_eq!(h[2]["tool_call_id"], "c1");
        assert!(h[2]["content"].as_str().unwrap().contains("未完成"));
        assert_eq!(h[3]["role"], "user");
    }

    /// 回归：压缩判定必须按校准后的真实 token 折算，否则平台实测远高于本地粗估时，
    /// 圆环已经很高却不触发压缩。
    #[test]
    fn calibration_factor_shifts_compression_trigger() {
        let h = history(20);
        // 系数 1.0：粗估即真实值，窗口够用，走自然视图
        let loose = build_context_view(&h, None, &tools(), 12_000, 1, 1.0, None).unwrap();
        assert_eq!(loose.usage.strategy, CompressionStrategy::None);
        // 系数 4.0：平台实测是粗估的 4 倍，同样的历史应被判为超预算并压缩
        let tight = build_context_view(&h, None, &tools(), 12_000, 1, 4.0, None).unwrap();
        assert_ne!(tight.usage.strategy, CompressionStrategy::None);
    }

    fn image_part(url: &str) -> Value {
        json!({"type": "image_url", "image_url": {"url": url}})
    }

    #[test]
    fn multimodal_content_estimates_flat_tokens_not_base64_chars() {
        // 200KB base64 若按字符估算会产出几十万假 token，应按图片块固定估算
        let big = "x".repeat(200_000);
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "看图"},
                image_part(&format!("data:image/jpeg;base64,{big}")),
            ],
        });
        let est = estimate_message_tokens(&msg);
        assert!(
            est < 3_000,
            "图片应按固定估算而非 base64 字符计数: {est}"
        );
        assert!(
            est >= IMAGE_PART_TOKENS,
            "至少应包含一张图片的估算: {est}"
        );
    }

    /// 预算内全量发送：带图消息（含首轮）逐字保留，旧图在真正需要压缩前不会动。
    #[test]
    fn full_view_keeps_every_original_image() {
        let img = |url: &str| {
            json!({"role": "user", "content": [
                {"type": "text", "text": format!("带图 {url}")},
                image_part(url),
            ]})
        };
        // 10 轮：首轮与最后一轮各带一张图
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        h.push(img("data:image/jpeg;base64,ANCHOR"));
        h.push(json!({"role": "assistant", "content": "收到"}));
        for i in 1..9 {
            h.push(json!({"role": "user", "content": format!("任务{i}")}));
            h.push(json!({"role": "assistant", "content": format!("执行{i}")}));
        }
        h.push(img("data:image/jpeg;base64,NEW"));
        h.push(json!({"role": "assistant", "content": "处理中"}));
        let view = build_context_view(&h, None, &tools(), 128_000, 1, 1.0, None).unwrap();
        assert_eq!(view.usage.strategy, CompressionStrategy::None);
        let mut seen_new = 0;
        let mut seen_anchor = 0;
        for m in &view.messages {
            let Some(parts) = m["content"].as_array() else {
                continue;
            };
            for p in parts {
                let url = p["image_url"]["url"].as_str().unwrap_or("");
                if url.contains("NEW") {
                    seen_new += 1;
                } else if url.contains("ANCHOR") {
                    seen_anchor += 1;
                }
            }
        }
        assert_eq!(seen_new, 1, "最后一轮的图片必须保留");
        assert_eq!(seen_anchor, 1, "首轮的旧图在全量视图里原样保留");
    }

    /// 真正触发压缩后：首轮折叠成锚点（图片折成占位文本，base64 不重发），
    /// 保留窗口内的带图消息仍逐字发送。
    #[test]
    fn compressed_view_strips_anchor_image_keeps_recent() {
        let img = |url: &str| {
            json!({"role": "user", "content": [
                {"type": "text", "text": format!("带图 {url}")},
                image_part(url),
            ]})
        };
        // 首轮带图 + 19 个普通轮 + 最后一轮带图：小窗口触发压缩
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        h.push(img("data:image/jpeg;base64,ANCHOR"));
        h.push(json!({"role": "assistant", "content": "收到"}));
        for i in 1..20 {
            h.push(json!({"role": "user", "content": format!("任务{i}")}));
            h.push(json!({"role": "assistant", "content": format!("执行{i}")}));
        }
        h.push(img("data:image/jpeg;base64,NEW"));
        h.push(json!({"role": "assistant", "content": "处理中"}));
        let view = build_context_view(&h, None, &tools(), 6_000, 1, 1.0, None).unwrap();
        assert_ne!(view.usage.strategy, CompressionStrategy::None);
        let mut seen_new = 0;
        let mut seen_anchor = 0;
        let mut anchor_placeholder = false;
        for m in &view.messages {
            if let Some(parts) = m["content"].as_array() {
                for p in parts {
                    let url = p["image_url"]["url"].as_str().unwrap_or("");
                    if url.contains("NEW") {
                        seen_new += 1;
                    } else if url.contains("ANCHOR") {
                        seen_anchor += 1;
                    }
                }
            }
            let text = m["content"].as_str().unwrap_or("");
            if m["role"] == "assistant" && text.contains("带图") && text.contains("[图片]") {
                anchor_placeholder = true;
            }
        }
        assert_eq!(seen_new, 1, "保留窗口内的图片必须原样发送");
        assert_eq!(seen_anchor, 0, "锚点里的旧图不应重发 base64");
        assert!(anchor_placeholder, "锚点应留下图片占位符");
    }

    /// 同一最近窗口内连续多条带图消息全部保留。
    #[test]
    fn view_keeps_images_across_multiple_recent_turns() {
        let img = |url: &str| {
            json!({"role": "user", "content": [
                {"type": "text", "text": "看图"},
                image_part(url),
            ]})
        };
        let h = vec![
            json!({"role": "system", "content": "系统提示"}),
            img("data:image/png;base64,A1"),
            json!({"role": "assistant", "content": "收到1"}),
            img("data:image/png;base64,A2"),
            json!({"role": "assistant", "content": "收到2"}),
            img("data:image/png;base64,A3"),
            json!({"role": "assistant", "content": "收到3"}),
        ];
        let view = build_context_view(&h, None, &tools(), 128_000, 1, 1.0, None).unwrap();
        let kept: Vec<&str> = view
            .messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter_map(|p| p["image_url"]["url"].as_str())
            .collect();
        assert_eq!(kept.len(), 3, "最近窗口内所有带图消息都应保留: {kept:?}");
    }

    #[test]
    fn truncate_content_value_preserves_image_parts() {
        let v = json!([
            {"type": "text", "text": "很长的文本"},
            image_part("data:image/png;base64,AAA"),
        ]);
        let out = truncate_content_value(&v, 1000);
        let arr = out.as_array().unwrap();
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(
            arr[1]["image_url"]["url"].as_str().unwrap(),
            "data:image/png;base64,AAA"
        );
    }

    #[test]
    fn digest_includes_image_marker() {
        let r = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "看截图"},
                image_part("data:image/png;base64,AAA"),
            ]}),
            json!({"role": "assistant", "content": "已查看"}),
        ];
        let d = round_digest(&r, 1);
        assert!(d.contains("看截图"), "应保留文本: {d}");
        assert!(d.contains("[图片]"), "摘要应标记图片: {d}");
    }

    // ---------- 预算触发选择与压缩记忆 ----------

    use crate::agent::memory::{SummaryCheckpoint, SummaryNote};

    /// 12+ 个小轮次在大窗口下逐字保留全部原始 user/assistant/tool 消息，
    /// 不做任何压缩，canonical 历史原样不动。
    #[test]
    fn full_view_at_large_window_preserves_all_rounds() {
        let h = history(12);
        let canonical = h.clone();
        let view = build_context_view(&h, None, &tools(), 128_000, 1, 1.0, None).unwrap();
        assert_eq!(view.usage.strategy, CompressionStrategy::None);
        assert_eq!(view.usage.compressed_rounds, 0);
        assert_eq!(view.compact_through, None);
        // 无台账时 system 之后就是全部原始轮次消息，没有额外 DATA 消息
        assert_eq!(view.messages.len(), h.len());
        assert_eq!(view.messages[1..], h[1..], "原始消息必须逐字保留");
        assert_eq!(h, canonical, "canonical 历史不得被改动");
    }

    /// 压缩前缀的证据块覆盖 round 0：assistant/工具结果进摘录（显式 round:<n> 引用）。
    #[test]
    fn compacted_evidence_covers_round_zero() {
        let h = history(8);
        let (_, ranges) = split_round_ranges(&h);
        let (block, generated) = compacted_evidence(&h, &ranges, 4, None, 10_000);
        assert!(!generated);
        assert!(block.contains("round:0"), "摘录必须覆盖 round 0: {block}");
        assert!(
            block.contains("step0 成功"),
            "应保留 round 0 的工具结果: {block}"
        );
        assert!(
            block.contains("echo step0"),
            "应保留 round 0 的命令: {block}"
        );
        assert!(
            block.contains("round:3") && !block.contains("round:4"),
            "只覆盖压缩前缀: {block}"
        );
    }

    /// 触发压缩：compressed_rounds == 被压缩的前缀长度（含 round 0），
    /// compact_through 请求一次新的前缀压缩；锚点恰好一次、以 DATA 消息出现。
    #[test]
    fn triggered_compression_reports_prefix_and_requests_summary() {
        let h = history(20);
        let view = build_context_view(&h, None, &tools(), 4_000, 1, 1.0, None).unwrap();
        assert_eq!(view.usage.strategy, CompressionStrategy::Extractive);
        let cutoff = view.compact_through.expect("应请求新的前缀压缩");
        assert_eq!(view.usage.compressed_rounds, cutoff);
        assert!(cutoff > 0 && cutoff < 20);
        let anchor_hits = view
            .messages
            .iter()
            .filter(|m| m["content"].as_str().unwrap_or("").contains("用户任务0"))
            .count();
        assert_eq!(anchor_hits, 1, "锚点不重复: {anchor_hits}");
    }

    /// 已缓存检查点 + 追加一个小轮次：沿用基线（不请求新压缩、策略 llm_summary）；
    /// 换更大窗口则恢复全量原文。
    #[test]
    fn cached_checkpoint_reused_then_restored_by_larger_window() {
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        for i in 0..12 {
            h.push(json!({"role": "user", "content": format!("任务{i} {}", "需".repeat(200))}));
            h.push(json!({"role": "assistant", "content": "完成"}));
        }
        let mut doc = StructuredSummary::default();
        doc.goal.push(SummaryNote {
            text: "部署服务".into(),
            rounds: vec![0],
        });
        let memory = ContextMemory {
            compacted_through: 8,
            attempted_through: 8,
            checkpoint: Some(SummaryCheckpoint {
                through_round: 8,
                document: doc,
            }),
            warning: None,
        };
        // 小窗口：全量超触发线 → 走基线，不请求新压缩
        let view = build_context_view(&h, None, &tools(), 6_000, 1, 1.0, Some(&memory)).unwrap();
        assert_eq!(view.usage.strategy, CompressionStrategy::LlmSummary);
        assert_eq!(view.usage.compressed_rounds, 8);
        assert_eq!(view.compact_through, None);
        let joined: String = view
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            joined.contains("部署服务") && joined.contains("round:0"),
            "摘要渲染必须带 round:<n> 引用: {joined}"
        );
        // 更大窗口：全量恢复原文
        let restored =
            build_context_view(&h, None, &tools(), 30_000, 1, 1.0, Some(&memory)).unwrap();
        assert_eq!(restored.usage.strategy, CompressionStrategy::None);
        assert_eq!(restored.usage.compressed_rounds, 0);
        assert_eq!(restored.messages.len(), h.len());
    }

    /// 摘要失败的 warning 注入视图（证据块前置 + usage.warning 上报）。
    #[test]
    fn fallback_warning_is_visible_in_view() {
        let h = history(12);
        let warning = "Summary unavailable; rule-extracted context is being used.";
        let memory = ContextMemory {
            compacted_through: 6,
            attempted_through: 6,
            checkpoint: None,
            warning: Some(warning.to_string()),
        };
        let view = build_context_view(&h, None, &tools(), 4_000, 1, 1.0, Some(&memory)).unwrap();
        assert_ne!(view.usage.strategy, CompressionStrategy::None);
        let joined: String = view
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            joined.contains("rule-extracted context is being used"),
            "warning 必须注入视图: {joined}"
        );
        assert_eq!(view.usage.warning.as_deref(), Some(warning));
    }

    /// 摘要体量 + 台账开销不溢出预算：压缩视图必须放进输入预算，
    /// 否则走应急削减——任何结局都不允许静默超发。
    #[test]
    fn summary_and_plan_overhead_stays_within_budget() {
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        for i in 0..14 {
            h.push(json!({"role": "user", "content": format!("任务{i} {}", "需".repeat(300))}));
            h.push(json!({"role": "assistant", "content": "完成"}));
        }
        let mut doc = StructuredSummary::default();
        doc.facts.push(SummaryNote {
            text: format!("fact {}", "x".repeat(120)),
            rounds: vec![0],
        });
        doc.actions.push(SummaryNote {
            text: format!("action {}", "y".repeat(120)),
            rounds: vec![2],
        });
        let memory = ContextMemory {
            compacted_through: 10,
            attempted_through: 10,
            checkpoint: Some(SummaryCheckpoint {
                through_round: 10,
                document: doc,
            }),
            warning: None,
        };
        let plan = format!("[任务台账]\n目标：{}", "x".repeat(400));
        let view =
            build_context_view(&h, Some(&plan), &tools(), 8_000, 1, 1.0, Some(&memory)).unwrap();
        assert!(
            view.usage.used_tokens <= view.usage.budget_tokens,
            "视图估算必须放进输入预算: {} > {}",
            view.usage.used_tokens,
            view.usage.budget_tokens
        );
    }

    /// 最新用户请求自身超预算：明确返回 context_window 错误而不是静默截断它。
    #[test]
    fn oversized_latest_user_returns_error_not_truncation() {
        let mut h = history(3);
        h.push(json!({"role": "user", "content": "长".repeat(20_000)}));
        let err = build_context_view(&h, None, &tools(), 4_000, 1, 1.0, None).unwrap_err();
        assert!(
            err.contains("context_window") || err.contains("上下文窗口"),
            "应为明确的窗口错误: {err}"
        );
    }

    // ---------- 单 user 长任务：assistant 交换轮 ----------

    /// 一条 user 驱动 32 次工具交换：每个「assistant 应答 + 连续 tool 结果」
    /// 各成一轮，前缀可折叠。最新 user 原文（其轮已折叠）逐字补回一次，
    /// 最近完整交换保留、视图放进预算、tool 配对不悬挂、canonical 不动，
    /// 且摘要源能从旧交换轮构建。
    #[test]
    fn single_user_many_tool_exchanges_compact_as_exchange_rounds() {
        let mut h = vec![
            json!({"role": "system", "content": "系统提示"}),
            json!({"role": "user", "content": "部署并验证服务"}),
        ];
        for i in 0..32 {
            h.push(json!({
                "role": "assistant", "content": format!("步骤{i}"),
                "tool_calls": [{"id": format!("call{i}"), "type": "function",
                    "function": {"name": "exec_command",
                        "arguments": format!("{{\"command\":\"cmd{i}\"}}")}}]
            }));
            h.push(json!({"role": "tool", "tool_call_id": format!("call{i}"),
                "content": format!("out{i}-{}", "x".repeat(1_000 + (i % 2) * 1_000))}));
        }
        let canonical = h.clone();
        let (_, ranges) = split_round_ranges(&h);
        assert_eq!(
            ranges.len(),
            32,
            "round0=user+首次交换，其后每个 assistant 交换各一轮"
        );

        let view = build_context_view(&h, None, &tools(), 16_000, 1, 1.0, None).unwrap();
        let cutoff = view.compact_through.expect("长任务必须触发新的前缀压缩");
        assert!(cutoff > 0 && cutoff < 32);
        assert_eq!(view.usage.compressed_rounds, cutoff);
        assert!(
            view.usage.used_tokens <= view.usage.budget_tokens,
            "视图必须放进预算: {} > {}",
            view.usage.used_tokens,
            view.usage.budget_tokens
        );
        // 最新 user 原文逐字出现恰好一次；它在数据块之后、保留的交换之前
        let user_hits = view
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .filter(|m| m["content"].as_str() == Some("部署并验证服务"))
            .count();
        assert_eq!(user_hits, 1, "当前 user 消息必须逐字保留一次");
        let user_pos = view
            .messages
            .iter()
            .position(|m| m["role"] == "user")
            .unwrap();
        assert!(
            view.messages[user_pos + 1..]
                .iter()
                .any(|m| m["role"] == "assistant"),
            "保留的 assistant 交换应在当前 user 之后"
        );
        // tool 配对完整
        let mut pending: Vec<String> = Vec::new();
        for msg in &view.messages {
            for call in msg["tool_calls"].as_array().into_iter().flatten() {
                pending.push(call["id"].as_str().unwrap().to_string());
            }
            if msg["role"] == "tool" {
                let id = msg["tool_call_id"].as_str().unwrap();
                assert!(pending.contains(&id.to_string()), "悬挂 tool 结果: {id}");
                pending.retain(|x| x != id);
            }
        }
        assert!(pending.is_empty(), "存在没有结果的 tool_call: {pending:?}");
        // 旧交换轮可以构建摘要源（编号与压缩口径一致）：超出摘要输入预算的
        // 前缀按增量分批——只取能放得下的最长前缀子区间，而不是整段放弃
        let source = crate::agent::memory::build_summary_source(&h, None, cutoff, 16_000, 1.0)
            .expect("生产参数（16k 窗口）下必须能构建有界摘要源");
        assert!(
            source.through_round > 0 && source.through_round <= cutoff,
            "分批覆盖端点必须落在请求前缀内: {}",
            source.through_round
        );
        assert_eq!(h, canonical, "canonical 历史不得被改动");
    }

    /// 当前 user 带图且其所在轮被折叠/摘要覆盖：逐字补回的消息必须保留
    /// 原始 image_url 块（不替换为占位符）——模型正要应答的输入一字不少。
    #[test]
    fn compacted_active_user_keeps_images_verbatim() {
        let mut h = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "看图部署"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,ACTIVE"}},
            ]}),
        ];
        for i in 0..30 {
            h.push(json!({
                "role": "assistant", "content": format!("步骤{i}"),
                "tool_calls": [{"id": format!("call{i}"), "type": "function",
                    "function": {"name": "exec_command", "arguments": "{}"}}]
            }));
            h.push(json!({"role": "tool", "tool_call_id": format!("call{i}"),
                "content": format!("out{i} {}", "y".repeat(1_500))}));
        }
        // 已验收检查点覆盖前 5 轮（含带图 round0）
        let mut doc = StructuredSummary::default();
        doc.goal.push(SummaryNote {
            text: "看图部署".into(),
            rounds: vec![0],
        });
        let memory = ContextMemory {
            compacted_through: 5,
            attempted_through: 5,
            checkpoint: Some(SummaryCheckpoint {
                through_round: 5,
                document: doc,
            }),
            warning: None,
        };
        let view = build_context_view(&h, None, &tools(), 16_000, 1, 1.0, Some(&memory)).unwrap();
        assert_eq!(view.usage.strategy, CompressionStrategy::LlmSummary);
        let users: Vec<&Value> = view
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .collect();
        assert_eq!(users.len(), 1, "当前 user 只补回一次");
        let parts = users[0]["content"].as_array().expect("多模态原文");
        assert!(
            parts
                .iter()
                .any(|p| p["image_url"]["url"].as_str() == Some("data:image/png;base64,ACTIVE")),
            "当前 user 的图片必须原样保留，不得换成占位符"
        );
    }

    /// 有效但超大的缓存检查点不得挤爆视图：渲染结果超证据预算时退回
    /// 确定性摘录（策略 Extractive），检查点在记忆里原样保留，不丢
    /// 最近的完整上下文与台账。
    #[test]
    fn oversized_cached_checkpoint_falls_back_to_extractive_view() {
        let mut h = vec![json!({"role": "system", "content": "系统提示"})];
        for i in 0..14 {
            h.push(json!({"role": "user", "content": format!("任务{i} {}", "需".repeat(300))}));
            h.push(json!({"role": "assistant", "content": "完成"}));
        }
        // 大但合法的文档：单节 8 条 × 600 字符，逼近 SUMMARY_MAX_CHARS
        let mut doc = StructuredSummary::default();
        for _ in 0..policy::SUMMARY_MAX_ITEMS_PER_SECTION {
            doc.facts.push(SummaryNote {
                text: "x".repeat(policy::SUMMARY_MAX_NOTE_CHARS),
                rounds: vec![0],
            });
        }
        let memory = ContextMemory {
            compacted_through: 10,
            attempted_through: 10,
            checkpoint: Some(SummaryCheckpoint {
                through_round: 10,
                document: doc,
            }),
            warning: None,
        };
        let plan = format!("[任务台账]\n目标：{}", "z".repeat(400));
        let view =
            build_context_view(&h, Some(&plan), &tools(), 8_000, 1, 1.0, Some(&memory)).unwrap();
        assert_eq!(
            view.usage.strategy,
            CompressionStrategy::Extractive,
            "渲染摘要超证据预算时退回确定性摘录"
        );
        assert!(
            view.usage.used_tokens <= view.usage.budget_tokens,
            "回退视图仍必须放进预算"
        );
        let joined: String = view
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(joined.contains("round:"), "应有确定性摘录证据: {joined}");
        assert!(joined.contains("任务台账"), "台账不得因摘要过大被丢");
        // 记忆里的检查点没有被改写（调用方仍持有同一份 doc）
        assert!(memory.checkpoint.is_some());
        // 视图里没有塞入那条 600 字符的超大笔记
        assert!(
            !joined.contains(&"x".repeat(policy::SUMMARY_MAX_NOTE_CHARS)),
            "超预算的摘要渲染不应出现在视图里"
        );
    }
}
