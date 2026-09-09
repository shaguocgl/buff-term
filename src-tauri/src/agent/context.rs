//! 上下文发送视图：canonical history 只追加不裁剪，这里按模型窗口生成真正发给模型的
//! messages。旧轮次用确定性规则抽取成摘要，绝不调用 LLM；任务锚点、任务台账和最近
//! 窗口始终优先保留，保证 tool_call/tool 结果不被拆散。

use serde::Serialize;
use serde_json::{json, Value};

use crate::util::truncate_output;

/// 输入预算占模型窗口的比例：剩余 30% 留给输出与安全余量（参考 Roo Code 的 70% 可用）。
pub(crate) const INPUT_BUDGET_RATIO: f64 = 0.7;
/// 用量超过历史预算的 80% 时开始压缩。
pub(crate) const COMPRESS_TRIGGER_RATIO: f64 = 0.8;
/// 压缩后目标降到历史预算的 60% 以下，避免每轮反复压缩。
pub(crate) const COMPRESS_TARGET_RATIO: f64 = 0.6;
/// 最近窗口默认保留的完整轮数。
pub(crate) const RECENT_ROUNDS: usize = 6;
/// 历史预算下限：窗口本身很小时仍保留一点对话空间，最终是否放得下由总预算判定。
pub(crate) const MIN_HISTORY_BUDGET: usize = 4_000;
/// 单轮摘要的最大字符数。
pub(crate) const ROUND_DIGEST_MAX_CHARS: usize = 600;
/// 历史摘要块最多占历史预算的 25%。
pub(crate) const DIGEST_BUDGET_RATIO: f64 = 0.25;
/// 任务锚点（首条用户消息）在硬重置时的最大字符数。
pub(crate) const ANCHOR_MAX_CHARS: usize = 2_000;
/// 硬重置时单条工具结果的最大字符数。
pub(crate) const HARD_RESET_TOOL_MAX_CHARS: usize = 2_000;
/// 每条消息的固定结构开销（role/name/分隔符等）。
const MESSAGE_OVERHEAD_TOKENS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionStrategy {
    None,
    Extractive,
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

#[derive(Debug)]
pub(crate) struct ContextView {
    pub messages: Vec<Value>,
    pub usage: ContextUsage,
}

/// 粗略估算一条消息的 token 数：字符数 × 1.3 + 固定开销。
/// `tool_calls` 是数组，必须序列化后计入（含工具名 + arguments）。
pub(crate) fn estimate_message_tokens(msg: &Value) -> usize {
    let content = msg["content"].as_str().unwrap_or("");
    let tool_call_id = msg["tool_call_id"].as_str().unwrap_or("");
    let name = msg["name"].as_str().unwrap_or("");
    let tool_calls = if msg["tool_calls"].is_array() {
        serde_json::to_string(&msg["tool_calls"]).unwrap_or_default()
    } else {
        String::new()
    };
    let total_chars = content.chars().count()
        + tool_calls.chars().count()
        + tool_call_id.chars().count()
        + name.chars().count();
    ((total_chars as f64) * 1.3) as usize + MESSAGE_OVERHEAD_TOKENS
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

fn role(msg: &Value) -> &str {
    msg["role"].as_str().unwrap_or("")
}

fn content(msg: &Value) -> &str {
    msg["content"].as_str().unwrap_or("")
}

fn clip_head_tail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        truncate_output(s, max)
    }
}

/// 把历史拆成「system 消息 + 若干轮」。一轮 = 一条 user 消息 + 其后的 assistant/tool
/// 消息；若历史开头不是 user，则这些消息并入第一轮。
fn split_system_and_rounds(history: &[Value]) -> (Option<Value>, Vec<Vec<Value>>) {
    let (system, rest) = match history.first() {
        Some(first) if role(first) == "system" => (Some(first.clone()), &history[1..]),
        _ => (None, history),
    };
    let mut rounds: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    for msg in rest {
        if role(msg) == "user" && !current.is_empty() {
            rounds.push(std::mem::take(&mut current));
        }
        current.push(msg.clone());
    }
    if !current.is_empty() {
        rounds.push(current);
    }
    (system, rounds)
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

fn tool_result_summary(text: &str) -> (bool, String) {
    let lower = text.to_lowercase();
    let failed = text.contains("执行失败")
        || lower.contains("error")
        || lower.contains("failed")
        || lower.contains("exception")
        || lower.contains("traceback");
    (failed, clip_head_tail(text, 240))
}

/// 单轮规则摘要：保留命令原文、路径和错误尾部，不做语义概括。
fn round_digest(round: &[Value], index: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in round {
        match role(msg) {
            "user" => parts.push(format!(
                "[轮次{index}｜用户] {}",
                clip_head_tail(content(msg), 400)
            )),
            "assistant" => {
                if !content(msg).trim().is_empty() {
                    parts.push(format!("[助手] {}", clip_head_tail(content(msg), 300)));
                }
                if let Some(calls) = msg["tool_calls"].as_array() {
                    for call in calls {
                        let name = call["function"]["name"].as_str().unwrap_or("未知工具");
                        let args = call["function"]["arguments"].as_str().unwrap_or("");
                        parts.push(format!("[调用 {name}] {}", summarize_args(name, args)));
                    }
                }
            }
            "tool" => {
                let (failed, summary) = tool_result_summary(content(msg));
                parts.push(format!(
                    "[结果 {}] {}",
                    if failed { "失败" } else { "成功" },
                    summary
                ));
            }
            _ => {}
        }
    }
    let text = if parts.is_empty() {
        format!("[轮次{index}] （无可提取内容）")
    } else {
        parts.join("\n")
    };
    clip_head_tail(&text, ROUND_DIGEST_MAX_CHARS)
}

/// 最老轮次折叠成一行，避免摘要块无限增长。
fn one_line_digest(round: &[Value], index: usize) -> String {
    let user = round
        .iter()
        .find(|m| role(m) == "user")
        .map(|m| clip_head_tail(content(m), 120))
        .unwrap_or_else(|| "（无用户消息）".to_string());
    let outcome = round
        .iter()
        .rev()
        .find(|m| role(m) == "assistant" && !content(m).trim().is_empty())
        .map(|m| clip_head_tail(content(m), 80))
        .or_else(|| {
            round.iter().rev().find(|m| role(m) == "tool").map(|m| {
                let (failed, _) = tool_result_summary(content(m));
                if failed {
                    "工具失败".to_string()
                } else {
                    "工具完成".to_string()
                }
            })
        })
        .unwrap_or_else(|| "（无结果）".to_string());
    format!("[轮次{index}] 用户：{user} → {outcome}")
}

/// 组装历史摘要块。超过摘要预算时先折叠最老轮次，再省略最老条目并给出省略数量。
fn build_digest_block(rounds: &[Vec<Value>], start_index: usize, budget_tokens: usize) -> String {
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
        let mut out = String::from("[历史摘要｜规则抽取，非模型生成]\n");
        if omitted > 0 {
            out.push_str(&format!("[更早的 {omitted} 轮已省略]\n"));
        }
        out.push_str(&entries.join("\n"));
        out.push_str("\n[历史摘要结束]");
        out
    };

    // 阶段一：把最老的非一行摘要折叠成一行
    let mut collapsed = vec![false; entries.len()];
    while estimate_text_tokens(&block(&entries, omitted)) > budget_tokens {
        let Some(pos) = collapsed.iter().position(|done| !*done) else {
            break;
        };
        entries[pos] = one_line_digest(&rounds[pos], start_index + pos);
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

fn system_message(system: &str, plan_block: Option<&str>, digest_block: &str) -> Value {
    let mut text = system.to_string();
    if let Some(plan) = plan_block {
        if !plan.trim().is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(plan.trim());
        }
    }
    if !digest_block.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(digest_block);
    }
    json!({"role": "system", "content": text})
}

fn truncate_round(round: &[Value], tool_max_chars: usize) -> Vec<Value> {
    round
        .iter()
        .map(|msg| {
            let mut msg = msg.clone();
            match role(&msg) {
                "tool" => {
                    msg["content"] = json!(truncate_output(content(&msg), tool_max_chars));
                }
                "user" => {
                    msg["content"] = json!(truncate_output(content(&msg), ANCHOR_MAX_CHARS));
                }
                "assistant" => {
                    if content(&msg).chars().count() > ANCHOR_MAX_CHARS {
                        msg["content"] = json!(truncate_output(content(&msg), ANCHOR_MAX_CHARS));
                    }
                }
                _ => {}
            }
            msg
        })
        .collect()
}

fn assemble_view(
    system: &str,
    plan_block: Option<&str>,
    rounds: &[Vec<Value>],
    recent_count: usize,
    digest_budget_tokens: usize,
    hard_reset: bool,
) -> (Vec<Value>, usize) {
    let recent_start = rounds.len().saturating_sub(recent_count);
    let anchor_in_recent = recent_start == 0;
    let digest_rounds: &[Vec<Value>] = if recent_start > 1 {
        &rounds[1..recent_start]
    } else {
        &[]
    };
    let digest_block = if hard_reset {
        String::new()
    } else {
        build_digest_block(digest_rounds, 1, digest_budget_tokens)
    };

    let mut messages = vec![system_message(system, plan_block, &digest_block)];
    if !anchor_in_recent {
        if let Some(anchor) = rounds
            .first()
            .and_then(|round| round.iter().find(|m| role(m) == "user"))
        {
            let mut anchor = anchor.clone();
            anchor["content"] = json!(truncate_output(content(&anchor), ANCHOR_MAX_CHARS));
            messages.push(anchor);
        }
    }
    if hard_reset {
        if let Some(last) = rounds.last() {
            messages.extend(truncate_round(last, HARD_RESET_TOOL_MAX_CHARS));
        }
    } else {
        for round in &rounds[recent_start..] {
            messages.extend(round.iter().cloned());
        }
    }
    let compressed_rounds = if hard_reset {
        rounds.len().saturating_sub(1)
    } else {
        digest_rounds.len()
    };
    (messages, compressed_rounds)
}

/// 按窗口预算生成发送视图。
///
/// 优先级：system + 任务台账 > 任务锚点 > 最近窗口 > 历史摘要。所有丢弃都以整轮为单位，
/// 保证不会出现孤立的 tool 消息或缺失 tool 结果的 assistant.tool_calls。
pub(crate) fn build_context_view(
    history: &[Value],
    plan_block: Option<&str>,
    tools: &Value,
    window: u32,
    session_id: u32,
) -> Result<ContextView, String> {
    let (system_msg, rounds) = split_system_and_rounds(history);
    let system = system_msg.as_ref().map(content).unwrap_or("").to_string();
    let tools_tokens = estimate_tools_tokens(tools);
    let full_history_tokens = estimate_messages_tokens(history) + tools_tokens;
    let plan_tokens = plan_block.map(estimate_text_tokens).unwrap_or(0);
    let system_tokens = estimate_text_tokens(&system);
    let input_budget = ((window as f64) * INPUT_BUDGET_RATIO) as usize;
    let fixed_tokens = system_tokens + plan_tokens + tools_tokens;

    if fixed_tokens >= input_budget {
        return Err(format!(
            "当前模型的上下文窗口配置过小（{window} tokens）：系统提示与工具定义已占约 \
             {fixed_tokens} tokens，请更换模型或调大 context_window"
        ));
    }

    let history_budget = input_budget
        .saturating_sub(fixed_tokens)
        .max(MIN_HISTORY_BUDGET);
    // 触发线/目标线不能超过总预算本身，否则极小窗口会被 MIN_HISTORY_BUDGET 放大后误放行。
    let trigger = (fixed_tokens + ((history_budget as f64) * COMPRESS_TRIGGER_RATIO) as usize)
        .min(input_budget);
    let target = (fixed_tokens + ((history_budget as f64) * COMPRESS_TARGET_RATIO) as usize)
        .min(input_budget);
    let digest_budget = ((history_budget as f64) * DIGEST_BUDGET_RATIO) as usize;

    if rounds.is_empty() {
        let messages = vec![system_message(&system, plan_block, "")];
        let used_tokens = estimate_messages_tokens(&messages) + tools_tokens;
        return Ok(ContextView {
            messages,
            usage: ContextUsage {
                session_id,
                used_tokens,
                history_tokens: full_history_tokens,
                budget_tokens: input_budget,
                window_tokens: window,
                compressed_rounds: 0,
                strategy: CompressionStrategy::None,
                estimated: true,
                calibrated: false,
                warning: None,
            },
        });
    }

    let max_recent = RECENT_ROUNDS.min(rounds.len());

    // 1) 自然视图：最近 6 轮原样 + 更早轮次规则摘要。未超过触发线就直接用。
    let (natural_messages, natural_compressed) = assemble_view(
        &system,
        plan_block,
        &rounds,
        max_recent,
        digest_budget,
        false,
    );
    let natural_used = estimate_messages_tokens(&natural_messages) + tools_tokens;
    if natural_used <= trigger {
        return Ok(ContextView {
            messages: natural_messages,
            usage: ContextUsage {
                session_id,
                used_tokens: natural_used,
                history_tokens: full_history_tokens,
                budget_tokens: input_budget,
                window_tokens: window,
                compressed_rounds: natural_compressed,
                strategy: CompressionStrategy::None,
                estimated: true,
                calibrated: false,
                warning: None,
            },
        });
    }

    // 2) 触发压缩：逐步缩小最近窗口，直到降到目标线。
    let mut recent_count = max_recent;
    while recent_count > 1 {
        recent_count -= 1;
        let (messages, compressed) = assemble_view(
            &system,
            plan_block,
            &rounds,
            recent_count,
            digest_budget,
            false,
        );
        let used = estimate_messages_tokens(&messages) + tools_tokens;
        if used <= target {
            return Ok(ContextView {
                messages,
                usage: ContextUsage {
                    session_id,
                    used_tokens: used,
                    history_tokens: full_history_tokens,
                    budget_tokens: input_budget,
                    window_tokens: window,
                    compressed_rounds: compressed,
                    strategy: CompressionStrategy::Extractive,
                    estimated: true,
                    calibrated: false,
                    warning: None,
                },
            });
        }
    }

    // 3) 最近窗口只剩 1 轮仍超过目标线：只要没超总预算就接受，否则硬重置。
    let (messages, compressed) =
        assemble_view(&system, plan_block, &rounds, 1, digest_budget, false);
    let used = estimate_messages_tokens(&messages) + tools_tokens;
    if used <= input_budget {
        return Ok(ContextView {
            messages,
            usage: ContextUsage {
                session_id,
                used_tokens: used,
                history_tokens: full_history_tokens,
                budget_tokens: input_budget,
                window_tokens: window,
                compressed_rounds: compressed,
                strategy: CompressionStrategy::Extractive,
                estimated: true,
                calibrated: false,
                warning: None,
            },
        });
    }

    // 4) 硬重置：system + 台账 + 任务锚点 + 最后一轮，工具输出进一步截断。
    let (messages, compressed) =
        assemble_view(&system, plan_block, &rounds, 1, digest_budget, true);
    let used = estimate_messages_tokens(&messages) + tools_tokens;
    if used <= input_budget {
        return Ok(ContextView {
            messages,
            usage: ContextUsage {
                session_id,
                used_tokens: used,
                history_tokens: full_history_tokens,
                budget_tokens: input_budget,
                window_tokens: window,
                compressed_rounds: compressed,
                strategy: CompressionStrategy::HardReset,
                estimated: true,
                calibrated: false,
                warning: Some(
                    "上下文已达上限，已重置早期历史，仅保留任务台账、任务锚点和最近一轮"
                        .to_string(),
                ),
            },
        });
    }

    Err(format!(
        "当前模型的上下文窗口配置过小（{window} tokens）：即使只保留系统提示、任务台账和最近一轮，\
         仍需要约 {used} tokens。请调大该模型的 context_window，或更换窗口更大的模型"
    ))
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

    #[test]
    fn keeps_system_anchor_and_last_round_under_extreme_budget() {
        let h = history(20);
        let view = build_context_view(&h, None, &tools(), 2_000, 1).unwrap();
        assert_eq!(view.messages[0]["role"], "system");
        let users: Vec<&str> = view
            .messages
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(users.contains(&"用户任务0"), "任务锚点必须保留: {users:?}");
        assert!(users.contains(&"用户任务19"), "最后一轮必须保留: {users:?}");
    }

    #[test]
    fn compression_never_breaks_tool_call_pairing() {
        let h = history(20);
        let view = build_context_view(&h, None, &tools(), 8_000, 1).unwrap();
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
        assert!(d.contains("失败"), "应标记失败: {d}");
    }

    #[test]
    fn digest_is_byte_identical_for_same_range() {
        let h = history(12);
        let a = build_context_view(&h, None, &tools(), 4_000, 1).unwrap();
        let b = build_context_view(&h, None, &tools(), 4_000, 1).unwrap();
        let sa = a.messages[0]["content"].as_str().unwrap();
        let sb = b.messages[0]["content"].as_str().unwrap();
        assert_eq!(sa, sb, "相同输入范围必须生成字节一致的摘要");
    }

    #[test]
    fn plan_block_is_always_present_and_never_trimmed() {
        let h = history(20);
        let plan = "[任务台账｜必须遵守]\n目标：部署 nginx";
        let view = build_context_view(&h, Some(plan), &tools(), 3_000, 1).unwrap();
        let sys = view.messages[0]["content"].as_str().unwrap();
        assert!(sys.contains("部署 nginx"), "台账必须注入: {sys}");
    }

    #[test]
    fn tiny_window_returns_clear_error() {
        let h = history(2);
        let err = build_context_view(&h, None, &tools(), 40, 1).unwrap_err();
        assert!(
            err.contains("context_window"),
            "错误应提示调整 context_window: {err}"
        );
    }
}
