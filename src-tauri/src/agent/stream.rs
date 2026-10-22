//! 流式响应解析：SSE 行解码与增量聚合。把字节缓冲、行解析、usage 提取和
//! content/tool_calls 分片累积收敛到这里，便于脱离 AppHandle 做单元测试。

use serde_json::Value;
use std::collections::BTreeMap;

/// 流式 tool_calls 增量累积器：模型把 id/name/arguments 分片下发，按 index 聚合。
#[derive(Default)]
pub(super) struct ToolCallAcc {
    /// 工具调用 id；模型漏发时由调用方按流内序号补一个本地 id
    pub(super) id: String,
    /// 工具名原始值（可能为空或别名，规范名在 prepare_tool_calls 里解析）
    pub(super) name: String,
    /// arguments 分片拼接出的原始 JSON 文本
    pub(super) args: String,
}

/// SSE 解码器：字节缓冲而非 String——SSE 的 UTF-8 多字节字符可能被切分到两个
/// chunk，逐 chunk 用 from_utf8_lossy 会破坏跨 chunk 的中文/emoji，改为先累积
/// 字节、在 \n 边界处解码完整行。
/// 单行 SSE 缓冲上限。正常 OpenAI 兼容流不会接近该值；超过通常意味着
/// 服务端没有按行发送或恶意构造，继续累积会耗尽内存。
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SseDecoder {
    /// 尚未遇到 \n 边界的残留字节（可能含被切开的 UTF-8 多字节字符）
    buf: Vec<u8>,
    overflowed: bool,
}

/// 一条 SSE 事件：`data:` 行解析出的 JSON，或流的 [DONE] 结束标记。
pub(super) enum SseEvent {
    /// `data: {json}` 行解析出的增量消息
    Data(Value),
    /// `data: [DONE]` 结束标记
    Done,
}

impl SseDecoder {
    /// 追加一段字节流，返回其中完整行解析出的事件；不足一行的残留留在缓冲区，
    /// 等后续 chunk 或 `finish` 处理。
    pub(super) fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.overflowed {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > MAX_SSE_LINE_BYTES {
            self.overflowed = true;
            self.buf.clear();
            return Vec::new();
        }
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..pos + 1).collect();
            if let Some(event) = parse_sse_line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// 处理流结束时缓冲区中残留的未换行数据，避免丢失最后一段内容
    /// （部分平台会在流末尾附带 usage，丢掉就把实测用量变回估算）。
    pub(super) fn finish(&mut self) -> Option<SseEvent> {
        if self.overflowed {
            return None;
        }
        let rest = std::mem::take(&mut self.buf);
        parse_sse_line(&rest)
    }

    /// 是否因单行超过上限而丢弃了数据；调用方应把该流当成错误，而不是正常结束。
    pub(super) fn overflowed(&self) -> bool {
        self.overflowed
    }
}

/// 解析一行 SSE 数据：只认 `data:` 行，跳过 `: keep-alive`、`event:` 等其它行；
/// `[DONE]` 映射为结束标记，合法 JSON 映射为 Data，其余（截断/裸文本）丢弃。
fn parse_sse_line(line: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(line);
    let data = text.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(SseEvent::Done);
    }
    serde_json::from_str(data).ok().map(SseEvent::Data)
}

/// 一次流式响应的累积结果：assistant 正文、tool_calls 分片聚合、平台报告的实测 token 数。
#[derive(Default)]
pub(super) struct StreamReply {
    /// assistant 正文增量累积
    pub(super) content: String,
    /// BTreeMap 按 tool_calls index 排序，保证执行顺序与模型声明的顺序一致且确定
    pub(super) tool_calls: BTreeMap<usize, ToolCallAcc>,
    /// 平台在流末尾报告的 prompt_tokens（实测值）；未报告时为 None
    pub(super) reported_prompt_tokens: Option<usize>,
}

impl StreamReply {
    /// 应用一条 `data:` 增量：先提取 usage（若有），再按 OpenAI 两种流式格式
    /// （function 嵌套 / 顶层 name+arguments）累积 content 与 tool_calls。
    /// 返回本次的 content 增量供调用方 emit ai:stream（空串也返回，保持原有下发行为）。
    pub(super) fn apply<'v>(&mut self, value: &'v Value) -> Option<&'v str> {
        // 部分平台会在流末尾附带 usage；有就采信，用于把用量条从“估算”变成实测。
        if let Some(u) = value.get("usage") {
            self.reported_prompt_tokens = u["prompt_tokens"]
                .as_u64()
                .or_else(|| u["total_tokens"].as_u64())
                .map(|v| v as usize)
                .or(self.reported_prompt_tokens);
        }
        let delta = &value["choices"][0]["delta"];
        let text = delta["content"].as_str();
        if let Some(t) = text {
            self.content.push_str(t);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                let acc = self.tool_calls.entry(index).or_default();
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
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn utf8_multibyte_split_across_chunks() {
        // 在「中」这个三字节字符中间切开：第一段不应产生事件，第二段应解析出完整内容
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"中文\"}}]}\n";
        let bytes = line.as_bytes();
        let pos = line.find('中').unwrap();
        let mut dec = SseDecoder::default();
        assert!(dec.feed(&bytes[..pos + 1]).is_empty());
        let events = dec.feed(&bytes[pos + 1..]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::Data(v) => {
                assert_eq!(v["choices"][0]["delta"]["content"], "中文");
            }
            SseEvent::Done => panic!("应为 Data 事件"),
        }
    }

    #[test]
    fn rejects_single_line_over_memory_cap() {
        let mut dec = SseDecoder::default();
        let chunk = vec![b'a'; MAX_SSE_LINE_BYTES + 1];
        assert!(dec.feed(&chunk).is_empty());
        assert!(dec.overflowed());
        assert!(dec.finish().is_none());
    }

    #[test]
    fn skips_non_data_and_malformed_lines() {
        let mut dec = SseDecoder::default();
        let events = dec.feed(b": keep-alive\nevent: x\ndata: not-json\n");
        assert!(events.is_empty());
    }

    #[test]
    fn done_marker_ends_stream_and_lines_keep_order() {
        let mut dec = SseDecoder::default();
        let events = dec.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\ndata: [DONE]\n",
        );
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], SseEvent::Data(_)));
        assert!(matches!(events[1], SseEvent::Data(_)));
        assert!(matches!(events[2], SseEvent::Done));
    }

    #[test]
    fn residual_line_keeps_usage() {
        // 回归：流末尾不带换行的 data 行此前会被丢弃，导致漏掉平台报告的 usage
        let mut dec = SseDecoder::default();
        assert!(dec
            .feed(b"data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":42}}")
            .is_empty());
        let Some(SseEvent::Data(v)) = dec.finish() else {
            panic!("残留行应解析为 Data 事件");
        };
        let mut reply = StreamReply::default();
        reply.apply(&v);
        assert_eq!(reply.reported_prompt_tokens, Some(42));
    }

    #[test]
    fn reply_accumulates_content_and_tool_call_fragments() {
        let mut reply = StreamReply::default();
        reply.apply(&json!({"choices":[{"delta":{"content":"你"}}]}));
        assert_eq!(
            reply.apply(&json!({"choices":[{"delta":{"content":"好"}}]})),
            Some("好")
        );
        assert_eq!(reply.content, "你好");
        // function 嵌套格式（OpenAI 风格）：name 一次到位，arguments 分片
        reply.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"exec_command","arguments":"{\"comm"}}]}}]}));
        reply.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}));
        // 顶层 name/arguments 格式（部分模型）
        reply.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","name":"read_file","arguments":"{\"path\":\"/etc/hosts\"}"}]}}]}));
        let acc = &reply.tool_calls[&0];
        assert_eq!(acc.id, "c1");
        assert_eq!(acc.name, "exec_command");
        assert_eq!(acc.args, "{\"command\":\"ls\"}");
        let acc1 = &reply.tool_calls[&1];
        assert_eq!(acc1.id, "c2");
        assert_eq!(acc1.name, "read_file");
        assert_eq!(acc1.args, "{\"path\":\"/etc/hosts\"}");
    }

    #[test]
    fn usage_null_keeps_previous_value() {
        let mut reply = StreamReply::default();
        reply.apply(&json!({"choices":[{"delta":{}}],"usage":{"prompt_tokens":42}}));
        reply.apply(&json!({"choices":[{"delta":{}}],"usage":null}));
        assert_eq!(reply.reported_prompt_tokens, Some(42));
    }
}
