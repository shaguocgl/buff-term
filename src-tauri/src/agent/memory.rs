//! Structured context memory: model-generated summary checkpoints, the strict
//! validation they must pass before installation, the source-envelope builder,
//! and the one-shot auxiliary request transport. No Tauri/AppHandle/event
//! dependencies so every piece stays unit-testable against loopback fixtures.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

use super::context::{
    estimate_text_tokens, message_without_images, split_round_ranges, MESSAGE_OVERHEAD_TOKENS,
};
use super::context_policy as policy;
use crate::i18n::is_en;
use crate::safety::sanitize;
use crate::util::truncate_output;

/// One summary note: a piece of text plus the zero-based source rounds that support it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryNote {
    /// Note body (sanitized before storage; nonempty, <= SUMMARY_MAX_NOTE_CHARS)
    pub text: String,
    /// Supporting source rounds: 1..=8 distinct indexes, every one < through_round
    /// and actually supplied to the model as raw source or via the previous checkpoint
    pub rounds: Vec<usize>,
}

/// The structured document the auxiliary model must return: exactly these seven
/// sections, all required, no extra keys allowed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StructuredSummary {
    /// User goals
    pub goal: Vec<SummaryNote>,
    /// Explicit constraints given by the user
    pub constraints: Vec<SummaryNote>,
    /// Confirmed environmental facts
    pub facts: Vec<SummaryNote>,
    /// Actions actually attempted and their observed results
    pub actions: Vec<SummaryNote>,
    /// Failed attempts and their reasons
    pub failures: Vec<SummaryNote>,
    /// Decisions that were made
    pub decisions: Vec<SummaryNote>,
    /// Unresolved questions and next steps
    pub open_questions: Vec<SummaryNote>,
}

/// A checkpoint that passed every validation rule: it covers the history prefix [0, through_round).
#[derive(Clone, Debug)]
pub(crate) struct SummaryCheckpoint {
    /// Exclusive upper bound of the covered history prefix
    pub through_round: usize,
    /// The validated, sanitized structured summary
    pub document: StructuredSummary,
}

/// Per-conversation compaction memory; cleared when the conversation resets.
#[derive(Clone, Debug, Default)]
pub(crate) struct ContextMemory {
    /// Rounds [0, compacted_through) are folded out of the sending view.
    /// Advanced even when summarization falls back to extractive digests, so
    /// "already compacted" is monotonic and failed prefixes are not retried.
    pub compacted_through: usize,
    /// The last checkpoint that passed validation; kept across later failures
    pub checkpoint: Option<SummaryCheckpoint>,
    /// The largest prefix for which a summary was already attempted:
    /// an identical prefix is never requested twice
    pub attempted_through: usize,
    /// Model-facing note injected into the view while rule-extracted
    /// context is standing in for an unavailable summary
    pub warning: Option<String>,
}

/// Read-only section iterator shared by validation and view rendering.
pub(crate) fn summary_sections(doc: &StructuredSummary) -> [(&'static str, &Vec<SummaryNote>); 7] {
    [
        ("goal", &doc.goal),
        ("constraints", &doc.constraints),
        ("facts", &doc.facts),
        ("actions", &doc.actions),
        ("failures", &doc.failures),
        ("decisions", &doc.decisions),
        ("open_questions", &doc.open_questions),
    ]
}

/// Mutable section iterator used only by validation (sanitizes notes in place).
fn summary_sections_mut(doc: &mut StructuredSummary) -> [(&'static str, &mut Vec<SummaryNote>); 7] {
    [
        ("goal", &mut doc.goal),
        ("constraints", &mut doc.constraints),
        ("facts", &mut doc.facts),
        ("actions", &mut doc.actions),
        ("failures", &mut doc.failures),
        ("decisions", &mut doc.decisions),
        ("open_questions", &mut doc.open_questions),
    ]
}

/// Validate and sanitize a generated summary. `through_round` is the exclusive
/// coverage bound of this request; `supplied` is the set of rounds the model
/// could legitimately cite (raw source rounds plus rounds covered by the
/// previous checkpoint). Any single violation rejects the whole document:
/// a partial or invalid summary is never installed.
pub(crate) fn validate_summary(
    mut doc: StructuredSummary,
    through_round: usize,
    supplied: &BTreeSet<usize>,
) -> Result<StructuredSummary, String> {
    let mut total_notes = 0usize;
    for (name, notes) in summary_sections_mut(&mut doc) {
        if notes.len() > policy::SUMMARY_MAX_ITEMS_PER_SECTION {
            return Err(format!(
                "summary section {name} has more than {} notes",
                policy::SUMMARY_MAX_ITEMS_PER_SECTION
            ));
        }
        for note in notes.iter_mut() {
            note.text = sanitize(&note.text);
            if note.text.trim().is_empty() {
                return Err(format!("summary section {name} contains an empty note"));
            }
            if note.text.chars().count() > policy::SUMMARY_MAX_NOTE_CHARS {
                return Err(format!(
                    "summary note in {name} exceeds {} characters",
                    policy::SUMMARY_MAX_NOTE_CHARS
                ));
            }
            let distinct: BTreeSet<usize> = note.rounds.iter().copied().collect();
            if note.rounds.is_empty()
                || note.rounds.len() > policy::SUMMARY_MAX_SOURCE_REFS
                || distinct.len() != note.rounds.len()
            {
                return Err(format!(
                    "summary note in {name} has invalid round citations"
                ));
            }
            for r in &note.rounds {
                if *r >= through_round {
                    return Err(format!("summary note cites future round {r}"));
                }
                if !supplied.contains(r) {
                    return Err(format!("summary note cites unsupplied round {r}"));
                }
            }
            total_notes += 1;
        }
    }
    if total_notes == 0 {
        return Err("summary contains no notes".to_string());
    }
    let raw = serde_json::to_string(&doc)
        .map_err(|e| format!("cannot serialize summary document: {e}"))?;
    if raw.chars().count() > policy::SUMMARY_MAX_CHARS {
        return Err("summary document exceeds the size limit".to_string());
    }
    Ok(doc)
}

/// Pull the summary text out of a chat/completions body. Only a clean
/// `stop` + nonempty plain-text `content` is accepted; tool calls, missing or
/// empty content and truncated responses (`finish_reason: "length"` etc.) are
/// all rejected so a partial generation is never installed.
pub(crate) fn extract_summary_content(body: &Value) -> Result<String, String> {
    let choice = body["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .ok_or_else(|| "summary response has no choices".to_string())?;
    match choice["finish_reason"].as_str() {
        Some("stop") => {}
        other => {
            return Err(format!(
                "summary response did not finish cleanly (finish_reason: {})",
                other.unwrap_or("missing")
            ))
        }
    }
    let message = &choice["message"];
    if message["tool_calls"]
        .as_array()
        .map(|calls| !calls.is_empty())
        .unwrap_or(false)
    {
        return Err("summary response contains tool calls".to_string());
    }
    let text = message["content"].as_str().unwrap_or("");
    if text.trim().is_empty() {
        return Err("summary response has empty content".to_string());
    }
    Ok(text.to_string())
}

/// Source material for one summary request: the user-message envelope plus the
/// set of rounds the model is allowed to cite.
pub(crate) struct SummarySource {
    /// Exclusive coverage bound requested from the model
    pub through_round: usize,
    /// Rounds that may be cited (raw source rounds union checkpoint coverage)
    pub supplied: BTreeSet<usize>,
    /// The JSON envelope placed into the user message
    pub envelope: Value,
}

/// Build one source message for the envelope: image parts are replaced by
/// explicit placeholder text (never base64), oversized text/arguments are
/// head/tail excerpted. Call IDs, tool names and output references are kept verbatim.
fn source_message(msg: &Value, limit: usize, excerpted: &mut bool) -> Value {
    let mut m = message_without_images(msg);
    match m["content"].as_array() {
        Some(parts) => {
            let parts: Vec<Value> = parts
                .iter()
                .map(|p| {
                    let text = p["text"].as_str().unwrap_or("");
                    if p["type"].as_str() == Some("text") && text.chars().count() > limit {
                        *excerpted = true;
                        json!({"type": "text", "text": truncate_output(text, limit)})
                    } else {
                        p.clone()
                    }
                })
                .collect();
            m["content"] = Value::Array(parts);
        }
        None => {
            let text = m["content"].as_str().unwrap_or("");
            if text.chars().count() > limit {
                *excerpted = true;
                m["content"] = json!(truncate_output(text, limit));
            }
        }
    }
    if let Some(calls) = m["tool_calls"].as_array() {
        let calls: Vec<Value> = calls
            .iter()
            .map(|call| {
                let mut call = call.clone();
                let args = call["function"]["arguments"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if args.chars().count() > limit {
                    *excerpted = true;
                    call["function"]["arguments"] = json!(truncate_output(&args, limit));
                }
                call
            })
            .collect();
        m["tool_calls"] = Value::Array(calls);
    }
    m
}

/// Assemble the summary source envelope. A compatible checkpoint
/// (coverage <= through_round) is attached as previous_summary and rounds after
/// it are supplied as quoted source — incrementally: when the whole pending
/// range cannot fit the summary input budget, only the longest fitting prefix
/// [start, end) is sent and the returned source reports `through_round = end`,
/// so a large backlog is summarized batch by batch across turns instead of
/// permanently failing while the pending prefix keeps growing. Per-message
/// excerpt limits are tried largest to smallest; the level covering the most
/// rounds wins (equal coverage keeps the higher-fidelity level). Whole source
/// rounds are never silently dropped while claiming coverage; returns None
/// only when not even one source round fits (callers then stay on the
/// extractive fallback and mark the prefix attempted).
pub(crate) fn build_summary_source(
    history: &[Value],
    checkpoint: Option<&SummaryCheckpoint>,
    through_round: usize,
    window: u32,
    calibration: f64,
) -> Option<SummarySource> {
    let (_, ranges) = split_round_ranges(history);
    let through = through_round.min(ranges.len());
    let (previous, start) = match checkpoint.filter(|cp| cp.through_round <= through) {
        Some(cp) => (Some(cp), cp.through_round),
        None => (None, 0),
    };
    if start >= through {
        return None;
    }
    let calibration = if calibration.is_finite() {
        calibration.clamp(0.25, 4.0)
    } else {
        1.0
    };
    let scale = |tokens: usize| -> usize { ((tokens as f64) * calibration).round() as usize };
    let budget = policy::SUMMARY_INPUT_TOKEN_CAP
        .min(((window as f64) * policy::INPUT_BUDGET_RATIO) as usize);
    let language = if is_en() { "en" } else { "zh" };
    let previous_json = previous.map_or(Value::Null, |cp| {
        json!({
            "through_round": cp.through_round,
            "document": cp.document,
        })
    });

    // 包络固定开销：system prompt + 骨架（language/through_round/previous_summary/rounds）。
    // 逐轮增量累加各轮成本，避免每加一个候选轮就重序列化整个包络。
    let base_tokens = estimate_text_tokens(policy::SUMMARY_SYSTEM_PROMPT)
        + estimate_text_tokens(
            &json!({
                "language": language,
                "through_round": through,
                "previous_summary": previous_json.clone(),
                "rounds": [],
            })
            .to_string(),
        )
        + 2 * MESSAGE_OVERHEAD_TOKENS;

    let mut best: Option<(usize, Vec<Value>)> = None;
    for limit in policy::SUMMARY_SOURCE_CHAR_LIMITS {
        let mut tokens = base_tokens;
        let mut rounds_json = Vec::new();
        let mut end = start;
        for index in start..through {
            let mut excerpted = false;
            let messages: Vec<Value> = history[ranges[index].clone()]
                .iter()
                .map(|msg| source_message(msg, limit, &mut excerpted))
                .collect();
            let round = json!({
                "index": index,
                "messages": messages,
                "excerpted": excerpted,
            });
            let cost = estimate_text_tokens(&round.to_string());
            if scale(tokens + cost) > budget {
                break;
            }
            tokens += cost;
            rounds_json.push(round);
            end = index + 1;
        }
        if end > start && best.as_ref().is_none_or(|(b, _)| end > *b) {
            best = Some((end, rounds_json));
        }
        if end == through {
            break;
        }
    }
    let (end, rounds_json) = best?;

    // 可引用轮集合 = 本次新供给的轮 ∪ 上一轮检查点文档中实际出现过的引用。
    // 检查点只携带笔记及其显式引用，不能为整个历史区间背书——未被引用过的
    // 旧轮不得引用（其原文可经 recall_context 取回后再进入新的源轮）。
    let mut supplied: BTreeSet<usize> = (start..end).collect();
    if let Some(cp) = previous {
        for (_, notes) in summary_sections(&cp.document) {
            for note in notes {
                supplied.extend(note.rounds.iter().copied());
            }
        }
    }
    let envelope = json!({
        "language": language,
        "through_round": end,
        "previous_summary": previous_json,
        "rounds": rounds_json,
    });
    Some(SummarySource {
        through_round: end,
        supplied,
        envelope,
    })
}

/// Decide whether a built view needs a new auxiliary summary request:
/// only when the newly compacted prefix goes beyond the last attempted one.
/// An identical (already attempted and failed) prefix is never requested twice.
pub(crate) fn needs_summary(memory: &ContextMemory, compact_through: Option<usize>) -> bool {
    matches!(compact_through, Some(cutoff) if cutoff > memory.attempted_through)
}

/// Read a response body with a hard byte cap: the limit is enforced before
/// any JSON parsing so an unexpectedly large auxiliary response cannot grow
/// memory or stall parsing. pub(crate) so run.rs cancellation tests can drive
/// the body-read stage directly.
pub(crate) async fn read_bounded_body(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read summary response: {e}"))?;
        if buf.len() + chunk.len() > policy::SUMMARY_RESPONSE_MAX_BYTES {
            return Err("summary response exceeds the size limit".to_string());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Issue the single auxiliary summary request for a model turn: exact authored
/// system prompt, non-streaming, bounded max_tokens, and deliberately no tools,
/// tool_choice, response_format or temperature. The whole send + body read is
/// bounded by SUMMARY_TIMEOUT_SECS; callers wrap this future in until_cancelled
/// so cancellation interrupts body consumption too. Every failure path returns
/// Err and installs nothing — callers fall back to extractive context.
pub(crate) async fn request_summary(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    model: &str,
    source: &SummarySource,
    window: u32,
) -> Result<StructuredSummary, String> {
    let max_tokens = policy::SUMMARY_MAX_TOKENS
        .min(((window as f64) * 0.2) as usize)
        .max(1);
    let body = json!({
        "model": model,
        "stream": false,
        "max_tokens": max_tokens,
        "messages": [
            {"role": "system", "content": policy::SUMMARY_SYSTEM_PROMPT},
            {"role": "user", "content": source.envelope.to_string()},
        ],
    });
    let op = async {
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("summary request failed: {e}"))?;
        let status = resp.status();
        let bytes = read_bounded_body(resp).await?;
        if !status.is_success() {
            return Err(format!("summary request failed with HTTP {status}"));
        }
        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("summary response is not valid JSON: {e}"))?;
        let text = extract_summary_content(&parsed)?;
        let doc: StructuredSummary =
            serde_json::from_str(&text).map_err(|e| format!("summary document is invalid: {e}"))?;
        validate_summary(doc, source.through_round, &source.supplied)
    };
    match tokio::time::timeout(Duration::from_secs(policy::SUMMARY_TIMEOUT_SECS), op).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "summary request timed out after {}s",
            policy::SUMMARY_TIMEOUT_SECS
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn valid_doc_json() -> String {
        serde_json::json!({
            "goal": [{"text": "deploy nginx", "rounds": [0]}],
            "constraints": [],
            "facts": [{"text": "disk 78%", "rounds": [0]}],
            "actions": [],
            "failures": [],
            "decisions": [],
            "open_questions": [],
        })
        .to_string()
    }

    fn supplied(range: std::ops::Range<usize>) -> BTreeSet<usize> {
        range.collect()
    }

    #[test]
    fn validate_accepts_valid_cited_fixture() {
        let doc: StructuredSummary = serde_json::from_str(&valid_doc_json()).unwrap();
        let out = validate_summary(doc, 2, &supplied(0..2)).unwrap();
        assert_eq!(out.goal[0].text, "deploy nginx");
        assert_eq!(out.facts[0].rounds, vec![0]);
    }

    #[test]
    fn serde_rejects_missing_and_unknown_fields() {
        // 缺字段：七节全为必填
        assert!(serde_json::from_str::<StructuredSummary>(
            r#"{"goal":[],"constraints":[],"facts":[],"actions":[],"failures":[],"decisions":[]}"#
        )
        .is_err());
        // 未知字段：deny_unknown_fields
        let mut doc: Value = serde_json::from_str(&valid_doc_json()).unwrap();
        doc["extra"] = json!([]);
        assert!(serde_json::from_value::<StructuredSummary>(doc).is_err());
        // note 里的未知字段同样被拒
        let mut doc: Value = serde_json::from_str(&valid_doc_json()).unwrap();
        doc["goal"][0]["extra"] = json!(1);
        assert!(serde_json::from_value::<StructuredSummary>(doc).is_err());
    }

    #[test]
    fn validate_rejects_all_empty_summary() {
        let doc = StructuredSummary::default();
        let err = validate_summary(doc, 2, &supplied(0..2)).unwrap_err();
        assert!(err.contains("no notes"), "{err}");
    }

    #[test]
    fn validate_rejects_citation_violations() {
        let make = |rounds: Vec<usize>| {
            let mut doc = StructuredSummary::default();
            doc.goal.push(SummaryNote {
                text: "x".into(),
                rounds,
            });
            doc
        };
        // 未来轮次
        assert!(validate_summary(make(vec![2]), 2, &supplied(0..4)).is_err());
        // 未供给的轮次
        assert!(validate_summary(make(vec![3]), 5, &supplied(0..2)).is_err());
        // 空引用
        assert!(validate_summary(make(vec![]), 5, &supplied(0..5)).is_err());
        // 超过 8 条引用
        assert!(validate_summary(make((0..9).collect()), 10, &supplied(0..10)).is_err());
        // 重复引用
        assert!(validate_summary(make(vec![0, 0]), 5, &supplied(0..5)).is_err());
    }

    #[test]
    fn validate_rejects_oversized_notes_and_response() {
        // 单条超长
        let mut doc = StructuredSummary::default();
        doc.goal.push(SummaryNote {
            text: "x".repeat(policy::SUMMARY_MAX_NOTE_CHARS + 1),
            rounds: vec![0],
        });
        assert!(validate_summary(doc, 2, &supplied(0..2)).is_err());
        // 单节超 8 条
        let mut doc = StructuredSummary::default();
        for _ in 0..=policy::SUMMARY_MAX_ITEMS_PER_SECTION {
            doc.facts.push(SummaryNote {
                text: "f".into(),
                rounds: vec![0],
            });
        }
        assert!(validate_summary(doc, 2, &supplied(0..2)).is_err());
        // 总量超 SUMMARY_MAX_CHARS
        let mut doc = StructuredSummary::default();
        for section in [
            &mut doc.goal,
            &mut doc.constraints,
            &mut doc.facts,
            &mut doc.actions,
            &mut doc.failures,
            &mut doc.decisions,
            &mut doc.open_questions,
        ] {
            for _ in 0..policy::SUMMARY_MAX_ITEMS_PER_SECTION {
                section.push(SummaryNote {
                    text: "y".repeat(policy::SUMMARY_MAX_NOTE_CHARS),
                    rounds: vec![0],
                });
            }
        }
        let err = validate_summary(doc, 2, &supplied(0..2)).unwrap_err();
        assert!(
            err.contains("size limit") || err.contains("more than"),
            "{err}"
        );
    }

    #[test]
    fn extract_summary_content_accepts_only_clean_stop_text() {
        let ok = json!({"choices": [{
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "{}"},
        }]});
        assert_eq!(extract_summary_content(&ok).unwrap(), "{}");

        // length 截断
        let truncated = json!({"choices": [{
            "finish_reason": "length",
            "message": {"content": "{}"},
        }]});
        assert!(extract_summary_content(&truncated).is_err());
        // 缺 finish_reason
        let missing = json!({"choices": [{"message": {"content": "{}"}}]});
        assert!(extract_summary_content(&missing).is_err());
        // 带 tool_calls
        let tools = json!({"choices": [{
            "finish_reason": "stop",
            "message": {"content": "{}", "tool_calls": [{"id": "c"}]},
        }]});
        assert!(extract_summary_content(&tools).is_err());
        // 空 content / 无 choices
        let empty = json!({"choices": [{"finish_reason": "stop", "message": {"content": "  "}}]});
        assert!(extract_summary_content(&empty).is_err());
        assert!(extract_summary_content(&json!({"choices": []})).is_err());
    }

    fn small_history() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "部署 nginx"}),
            json!({"role": "assistant", "content": "好的"}),
        ]
    }

    #[test]
    fn summary_source_covers_new_rounds_and_strips_images() {
        let mut history = small_history();
        // round1：带图 + 超长输出（触发摘录标记）
        history.push(json!({"role": "user", "content": [
            {"type": "text", "text": "看截图"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,BIGBASE64DATA"}},
        ]}));
        history.push(json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "exec_command", "arguments": "{\"command\":\"tail -f log\"}"}}]
        }));
        history.push(json!({"role": "tool", "tool_call_id": "c1",
            "content": format!("{}… available as output:ref1 …\n{}", "h".repeat(5_000), "ERROR tail 失败")}));

        let checkpoint = SummaryCheckpoint {
            through_round: 1,
            document: StructuredSummary::default(),
        };
        let source = build_summary_source(&history, Some(&checkpoint), 2, 128_000, 1.0).unwrap();
        assert_eq!(source.through_round, 2);
        // 只有检查点之后的新轮次作为源供给
        let rounds = source.envelope["rounds"].as_array().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0]["index"], 1);
        assert_eq!(source.envelope["through_round"], 2);
        assert!(source.envelope["previous_summary"].is_object());
        let raw = source.envelope.to_string();
        assert!(!raw.contains("BIGBASE64DATA"), "base64 绝不进摘要源");
        assert!(
            raw.contains("[图片]") || raw.contains("[image]"),
            "图片占位符保留"
        );
        assert!(raw.contains("output:ref1"), "输出引用逐字保留");
        assert!(raw.contains("ERROR tail 失败"), "错误尾部必须保留");
        assert!(raw.contains("tail -f log"), "工具调用保留");
        assert_eq!(rounds[0]["excerpted"], true, "超长消息应标记摘录");
        // 空文档检查点不携带引用：可引用集合只有新供给的 round1
        assert!(source.supplied.contains(&1) && !source.supplied.contains(&0));
    }

    /// 可引用轮集合 = 上一轮检查点文档中实际出现的引用 + 本次新供给的轮：
    /// 检查点覆盖 [0,5) 但只引用了 round 0 时，{0,5} 可引、round3 必拒。
    #[test]
    fn supplied_citations_follow_previous_document_not_its_coverage() {
        let mut history = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..6 {
            history.push(json!({"role": "user", "content": format!("任务{i}")}));
            history.push(json!({"role": "assistant", "content": format!("完成{i}")}));
        }
        let mut doc = StructuredSummary::default();
        doc.facts.push(SummaryNote {
            text: "known fact".into(),
            rounds: vec![0],
        });
        let checkpoint = SummaryCheckpoint {
            through_round: 5,
            document: doc,
        };
        let source = build_summary_source(&history, Some(&checkpoint), 6, 128_000, 1.0).unwrap();
        assert_eq!(
            source.supplied.iter().copied().collect::<Vec<_>>(),
            vec![0, 5],
            "只允许引用文档中的引用 + 新供给轮"
        );
        // 覆盖区间里但未被引用过的 round3 不得引用
        let mut bad = StructuredSummary::default();
        bad.facts.push(SummaryNote {
            text: "x".into(),
            rounds: vec![3],
        });
        assert!(validate_summary(bad, 6, &source.supplied).is_err());
        // {0,5} 合法
        let mut good = StructuredSummary::default();
        good.facts.push(SummaryNote {
            text: "x".into(),
            rounds: vec![0, 5],
        });
        assert!(validate_summary(good, 6, &source.supplied).is_ok());
    }

    #[test]
    fn summary_source_all_new_rounds_when_no_checkpoint() {
        let mut history = small_history();
        history.push(json!({"role": "user", "content": "第二轮"}));
        history.push(json!({"role": "assistant", "content": "好"}));
        let source = build_summary_source(&history, None, 2, 128_000, 1.0).unwrap();
        let rounds = source.envelope["rounds"].as_array().unwrap();
        assert_eq!(rounds.len(), 2, "无检查点时从 round 0 全量供给");
        assert!(source.envelope["previous_summary"].is_null());
        // 不可见的 excerpted=false：短消息不标摘录
        assert_eq!(rounds[0]["excerpted"], false);
    }

    #[test]
    fn summary_source_returns_none_when_nothing_fits() {
        // 窗口极小：连最紧摘录也放不进摘要输入预算
        let mut history = small_history();
        history.push(json!({"role": "user", "content": "x".repeat(50_000)}));
        assert!(build_summary_source(&history, None, 2, 100, 1.0).is_none());
    }

    #[test]
    fn needs_summary_only_for_new_prefix() {
        let memory = ContextMemory {
            compacted_through: 5,
            attempted_through: 5,
            ..Default::default()
        };
        assert!(!needs_summary(&memory, Some(5)), "已尝试过的前缀不重试");
        assert!(!needs_summary(&memory, Some(3)), "更小的前缀不重试");
        assert!(!needs_summary(&memory, None));
        assert!(needs_summary(&memory, Some(6)), "更远的前缀需要新摘要");
    }

    /// Loopback fixture：只处理一次请求，请求体经 channel 回传给测试断言。
    /// 返回 JoinHandle：线程处理完请求即退出，测试末尾 join 掉不留悬挂线程。
    fn serve_once(
        respond: impl FnOnce(&str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> + Send + 'static,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/chat/completions", server.server_addr());
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let _ = tx.send(body.clone());
            let _ = req.respond(respond(&body));
        });
        (url, rx, handle)
    }

    fn chat_ok(content: &str) -> String {
        format!(
            r#"{{"choices":[{{"finish_reason":"stop","message":{{"role":"assistant","content":{content}}}}}]}}"#
        )
    }

    #[test]
    fn request_summary_valid_response_and_exact_request_shape() {
        block_on(async {
            let doc = valid_doc_json();
            let reply = chat_ok(&serde_json::to_string(&doc).unwrap());
            let (url, rx, handle) = serve_once(move |_| tiny_http::Response::from_string(reply));
            let history = small_history();
            let source = build_summary_source(&history, None, 1, 128_000, 1.0).unwrap();
            let client = reqwest::Client::new();
            let out = request_summary(&client, &url, "k", "test-model", &source, 128_000)
                .await
                .unwrap();
            assert_eq!(out.goal[0].text, "deploy nginx");

            let body: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
            assert_eq!(body["model"], "test-model");
            assert_eq!(body["stream"], false, "摘要请求必须非流式");
            assert!(
                body["max_tokens"].as_u64().unwrap() <= policy::SUMMARY_MAX_TOKENS as u64,
                "max_tokens 必须有界"
            );
            assert_eq!(body["messages"][0]["role"], "system");
            assert_eq!(
                body["messages"][0]["content"],
                policy::SUMMARY_SYSTEM_PROMPT,
                "必须使用 authoring prompt 原文"
            );
            assert_eq!(body["messages"][1]["role"], "user");
            for key in ["tools", "tool_choice", "response_format", "temperature"] {
                assert!(body.get(key).is_none(), "请求体不应携带 {key}");
            }
            handle.join().expect("loopback server thread must exit");
        });
    }

    #[test]
    fn request_summary_surfaces_http_error() {
        block_on(async {
            let (url, _rx, handle) = serve_once(|_| {
                tiny_http::Response::from_string("boom")
                    .with_status_code(tiny_http::StatusCode(500))
            });
            let history = small_history();
            let source = build_summary_source(&history, None, 1, 128_000, 1.0).unwrap();
            let err = request_summary(&reqwest::Client::new(), &url, "k", "m", &source, 128_000)
                .await
                .unwrap_err();
            assert!(err.contains("500"), "{err}");
            handle.join().expect("loopback server thread must exit");
        });
    }

    #[test]
    fn request_summary_rejects_malformed_and_invalid_responses() {
        block_on(async {
            let history = small_history();
            let source = build_summary_source(&history, None, 1, 128_000, 1.0).unwrap();
            // 非 JSON 响应
            let (url, _rx, h1) = serve_once(|_| tiny_http::Response::from_string("not json"));
            assert!(
                request_summary(&reqwest::Client::new(), &url, "k", "m", &source, 128_000)
                    .await
                    .is_err()
            );
            h1.join().expect("loopback server thread must exit");
            // JSON 但 finish_reason=length
            let (url, _rx, h2) = serve_once(|_| {
                tiny_http::Response::from_string(
                    r#"{"choices":[{"finish_reason":"length","message":{"content":"{}"}}]}"#,
                )
            });
            assert!(
                request_summary(&reqwest::Client::new(), &url, "k", "m", &source, 128_000)
                    .await
                    .is_err()
            );
            h2.join().expect("loopback server thread must exit");
            // 文档引用了未供给轮次 → 校验拒绝
            let bad_doc = serde_json::json!({
                "goal": [{"text": "x", "rounds": [9]}],
                "constraints": [], "facts": [], "actions": [],
                "failures": [], "decisions": [], "open_questions": [],
            })
            .to_string();
            let reply = chat_ok(&serde_json::to_string(&bad_doc).unwrap());
            let (url, _rx, h3) = serve_once(move |_| tiny_http::Response::from_string(reply));
            assert!(
                request_summary(&reqwest::Client::new(), &url, "k", "m", &source, 128_000)
                    .await
                    .is_err()
            );
            h3.join().expect("loopback server thread must exit");
        });
    }

    #[test]
    fn request_summary_rejects_oversized_body() {
        block_on(async {
            let big = "x".repeat(policy::SUMMARY_RESPONSE_MAX_BYTES + 1);
            let (url, _rx, handle) = serve_once(move |_| tiny_http::Response::from_string(big));
            let history = small_history();
            let source = build_summary_source(&history, None, 1, 128_000, 1.0).unwrap();
            let err = request_summary(&reqwest::Client::new(), &url, "k", "m", &source, 128_000)
                .await
                .unwrap_err();
            assert!(err.contains("size limit"), "{err}");
            handle.join().expect("loopback server thread must exit");
        });
    }

    /// 生产尺寸复现：16k 窗口 + 32 次工具交换，整个待摘要前缀放不进一次
    /// 辅助请求。增量构建必须只取能放得下的最长前缀子区间，装上检查点后
    /// 下一批从断点继续——有界步数内覆盖整个前缀，而不是永久失败。
    #[test]
    fn summary_source_batches_oversized_prefix_incrementally() {
        let mut history = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "排查磁盘暴涨并修复"}),
        ];
        for i in 0..32 {
            history.push(json!({
                "role": "assistant", "content": format!("步骤{i}"),
                "tool_calls": [{"id": format!("call{i}"), "type": "function",
                    "function": {"name": "exec_command",
                        "arguments": format!("{{\"command\":\"cmd{i}\"}}")}}]
            }));
            // 每条工具结果 ~3000 字符：整个前缀远超 16k 窗口的摘要输入预算
            history.push(json!({"role": "tool", "tool_call_id": format!("call{i}"),
                "content": format!("out{i}-{}", "x".repeat(3_000))}));
        }
        let (_, ranges) = split_round_ranges(&history);
        let cutoff = ranges.len() - 1; // 最后一轮不折叠

        let first = build_summary_source(&history, None, cutoff, 16_000, 1.0)
            .expect("放不下的前缀应给出有界批次而不是 None");
        assert!(
            first.through_round > 0 && first.through_round < cutoff,
            "整段放不进时应只覆盖前缀的一段: {}",
            first.through_round
        );
        assert_eq!(first.envelope["through_round"], first.through_round);
        // 供给与声称覆盖严格一致：声称的轮 = 实际供给的轮
        let rounds = first.envelope["rounds"].as_array().unwrap();
        assert_eq!(rounds.len(), first.through_round);
        assert!(first.supplied == (0..first.through_round).collect::<BTreeSet<_>>());

        // 装上检查点后逐批推进：每批的覆盖端点单调前进，有界步数内追平
        let mut covered = first.through_round;
        for _ in 0..64 {
            if covered >= cutoff {
                break;
            }
            let checkpoint = SummaryCheckpoint {
                through_round: covered,
                document: StructuredSummary::default(),
            };
            let next = build_summary_source(&history, Some(&checkpoint), cutoff, 16_000, 1.0)
                .expect("装上检查点后应能继续构建下一批");
            let next_rounds = next.envelope["rounds"].as_array().unwrap();
            assert_eq!(
                next_rounds[0]["index"], covered,
                "下一批必须从检查点断点开始"
            );
            assert!(
                next.through_round > covered,
                "每批都必须推进覆盖: {} -> {}",
                covered,
                next.through_round
            );
            covered = next.through_round;
        }
        assert_eq!(covered, cutoff, "分批必须在有界步数内覆盖整个前缀");
    }

    /// 分批后检查点文档里出现过的旧轮引用仍然可引：
    /// 第一批覆盖 [0,e)，第二批的 supplied = [e,e2) ∪ 第一批文档引用。
    #[test]
    fn batched_source_keeps_previous_checkpoint_citations() {
        let mut history = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..20 {
            history.push(json!({"role": "user", "content": format!("任务{i}")}));
            history.push(json!({"role": "assistant", "content": format!("完成{i}")}));
        }
        let mut doc = StructuredSummary::default();
        doc.facts.push(SummaryNote {
            text: "磁盘 78%".into(),
            rounds: vec![0],
        });
        let checkpoint = SummaryCheckpoint {
            through_round: 10,
            document: doc,
        };
        let source = build_summary_source(&history, Some(&checkpoint), 15, 128_000, 1.0).unwrap();
        assert_eq!(source.through_round, 15);
        // 新供给轮 [10,15) + 检查点引用过的 round 0；从未供给的 round 3 不可引
        assert_eq!(
            source.supplied.iter().copied().collect::<Vec<_>>(),
            vec![0, 10, 11, 12, 13, 14]
        );
    }
}
