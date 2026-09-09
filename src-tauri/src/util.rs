//! 通用工具函数：时间戳、字符串截断、shell 转义、错误提取、命令输出格式化、随机 token。

use crate::russh::ExecResult;

/// 当前 Unix 时间戳（秒）。
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 短字段截断（审计摘要、巡检摘要等），超长用省略号结尾。
pub fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head: String = chars[..max].iter().collect();
        format!("{head}…")
    }
}

/// 命令 / 基线输出截断，带换行提示，用于回填给模型或展示的长文本。
/// 头尾各保留一半：命令回显、报错、汇总结论通常在尾部，只留头部会丢关键信息。
pub fn truncate_output(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head_len = max / 2;
        let tail_len = max.saturating_sub(head_len);
        let head: String = chars[..head_len].iter().collect();
        let tail: String = chars[chars.len() - tail_len..].iter().collect();
        let omitted = chars.len().saturating_sub(max);
        format!("{head}\n...[输出过长，已省略 {omitted} 字符]...\n{tail}")
    }
}

/// 单引号安全转义，用于把用户/模型提供的路径拼接进 shell 命令（' → '\''）。
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 从 AI 平台的非成功响应里提取可读错误信息。
pub fn extract_error(text: &str, status: reqwest::StatusCode) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| text.chars().take(200).collect());
    format!("AI 平台返回 HTTP {}: {}", status.as_u16(), detail)
}

/// 统一格式化命令输出：trim + 超长截断（头 8000 + 尾 4000）+ 超时/退出码标记。
pub fn format_exec_output(out: &ExecResult) -> String {
    let mut text = out.text.trim().to_string();
    const MAX: usize = 12000;
    if text.chars().count() > MAX {
        let head: String = text.chars().take(8000).collect();
        let tail: String = text.chars().skip(text.chars().count() - 4000).collect();
        text = format!("{head}\n...[输出过长，已截断]...\n{tail}");
    }
    if out.timed_out {
        text.push_str("\n[命令执行超时，输出可能不完整]");
    }
    if let Some(code) = out.exit_code {
        if code != 0 {
            text.push_str(&format!("\n[退出码 {code}]"));
        }
    }
    text
}

/// 生成随机 token（两个 UUID 拼成 64 位 hex）。
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_output_keeps_head_and_tail() {
        let head = "HEAD".repeat(10);
        let tail = "TAIL".repeat(10);
        let input = format!("{head}{}{tail}", "x".repeat(200));
        let out = truncate_output(&input, 80);
        assert!(out.starts_with("HEAD"), "应保留头部: {out}");
        assert!(out.ends_with("TAIL"), "应保留尾部: {out}");
        assert!(out.contains("已省略"), "应提示省略字符数: {out}");
    }

    #[test]
    fn truncate_output_noop_when_short() {
        assert_eq!(truncate_output("short", 80), "short");
    }
}
