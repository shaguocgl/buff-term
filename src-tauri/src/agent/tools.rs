//! Agent 可调用工具的定义与执行：系统提示词、工具 JSON Schema、工具名推断、以及
//! 实际在远程服务器上执行工具调用的 `execute_tool`。

use super::trend::format_metric_trend;
use crate::db::Db;
use crate::models::{AiProvider, Host};
use crate::russh::RusshManager;
use crate::safety::{normalize_tool, sanitize};
use crate::util::{format_exec_output, now, shq};

pub(crate) fn system_prompt(host: &Host, provider: &AiProvider, model: &str) -> String {
    format!(
        "你是 buffTerm，运行在用户本地的 SSH 管理工具中，帮助用户管理远程服务器。\n\
         当前由 {} 平台提供能力，当前配置的底层模型是 {}。\n\
         当前连接的服务器：{}（{}@{}:{}）\n\
         可用工具：exec_command（执行命令）、read_file（读文件）、list_dir（列目录）、resource_usage（资源占用）、query_history（查询历史指标趋势）。\n\
         规则：\n\
         1. 所有 exec_command 都会经过用户批准，获批后才执行，请先说明意图。\n\
         2. 命令输出可能被截断，只基于已有信息回答，不要编造。\n\
         3. 遇到破坏性操作（删除、格式化、改权限、停服务等）时，明确提示风险并给出命令原文。\n\
         4. 使用中文回答，简洁、专业、有条理。\n\
         5. 身份说明：当用户询问“你是什么模型/你由谁开发”时，如实回答你由 {} 驱动、配置的模型为 {}，
            以及你是 buffTerm；不要声称自己是任何其他 AI 助手（如 ChatGPT、Claude、Gemini 等），
            也不要编造版本号或开发厂商信息。\n\
         6. 工具调用约定：工具名称必须是以下之一——exec_command、read_file、list_dir、resource_usage、query_history；
            每次工具调用都必须包含完整的 name 字段且不能为空，不要发明新工具名；参数放入 arguments（JSON 对象）。\n\
         7. 当用户询问“最近怎么样”、“有没有异常”、“是不是变慢了”等涉及变化的问题时，优先调用 query_history 查看趋势，
            而不是只调用 resource_usage 看当下值。趋势比绝对值更有诊断价值——一个从 30% 涨到 78% 的磁盘比一个稳定在 78% 的磁盘更紧急。
            query_history 会返回历史序列、变化斜率和外推预测（如“按当前增速，X 天后达到 90%”），这是单次快照无法提供的信息。
            根据分析目的选择合适的 granularity 和 window_hours：看最近几小时的细节波动用 minute + 小窗口；
            看今天的走势用 hour + 24～48 小时窗口；看长周期趋势/容量规划用 day + 更大窗口（最长 90 天）。
            不确定时可不填 granularity，会按 window_hours 自动选择合适粒度。",
        provider.name,
        model,
        host.name,
        host.username,
        host.address,
        host.port,
        provider.name,
        model
    )
}

pub(crate) fn tools_schema() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "exec_command",
                "description": "在远程服务器上执行一条 shell 命令并返回输出。默认超时 30 秒。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout_secs": { "type": "number", "description": "超时秒数，默认 30" },
                        "requires_approval": { "type": "boolean", "description": "如果你认为该命令有危险（删除、格式化、修改系统状态、影响服务等），设为 true，便于用户安全策略处理" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取远程服务器上的文件内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "远程文件路径" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出远程服务器上的目录内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "目录路径，默认当前目录" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "resource_usage",
                "description": "查看服务器磁盘、内存、负载和 CPU/内存占用最高的进程",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_history",
                "description": "查询当前服务器的历史指标趋势。用于判断资源使用是否在持续增长、是否有周期性波动、是否接近告警阈值。当用户问'最近怎么样'、'有没有问题'、'变慢了吗'时优先调用此工具，而不是只看当下快照。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "metric": {
                            "type": "string",
                            "enum": ["cpu", "mem", "load", "disk"],
                            "description": "要查询的指标：cpu=CPU使用率, mem=内存使用率, load=系统负载, disk=磁盘使用率"
                        },
                        "window_hours": {
                            "type": "number",
                            "description": "回溯多少小时，默认 168（7 天），最大 2160（90 天）"
                        },
                        "granularity": {
                            "type": "string",
                            "enum": ["minute", "hour", "day"],
                            "description": "数据聚合粒度：minute=原始采样点（适合看最近几小时的细节波动），hour=按小时取均值（适合看当天走势），day=按天取均值（适合看多天/长周期趋势）。不填时按 window_hours 自动选择：≤6 小时用 minute，≤48 小时用 hour，否则用 day。"
                        }
                    },
                    "required": ["metric"]
                }
            }
        },
    ])
}

pub(crate) fn parse_args(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ "command": raw.trim() }),
    }
}

/// 模型漏填工具名（`name` 为空）时，根据参数形态推断真实工具：
/// command → exec_command，path → read_file，metric/window_hours → query_history，
/// 空对象（无法从参数形态判断）→ resource_usage（唯一不需要参数的工具，即使猜错，
/// resource_usage 也是只读且无副作用的操作，代价可接受）。
/// 若 `name` 非空，原样返回。
pub(crate) fn infer_tool_name<'a>(name: &'a str, args: &serde_json::Value) -> &'a str {
    if !name.trim().is_empty() {
        return name;
    }
    if args
        .get("command")
        .and_then(|c| c.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        "exec_command"
    } else if args.get("path").and_then(|p| p.as_str()).is_some() {
        "read_file"
    } else if args.get("metric").is_some() || args.get("window_hours").is_some() {
        "query_history"
    } else if args.as_object().map(|m| m.is_empty()).unwrap_or(false) {
        "resource_usage"
    } else {
        name
    }
}

pub(crate) async fn execute_tool(
    db: &Db,
    russh: &RusshManager,
    host: &Host,
    name: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    match name {
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| "缺少 command 参数".to_string())?;
            let timeout = args.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(30);
            let out = russh
                .exec(host, command, std::time::Duration::from_secs(timeout))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "缺少 path 参数".to_string())?;
            let out = russh
                .exec(host, &format!("cat {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let out = russh
                .exec(host, &format!("ls -lah {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "resource_usage" => {
            let script = "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8";
            let out = russh
                .exec(host, script, std::time::Duration::from_secs(25))
                .await?;
            Ok(sanitize(&format_exec_output(&out)))
        }
        "query_history" => {
            let metric = args.get("metric").and_then(|v| v.as_str()).unwrap_or("cpu");
            let window_h = args.get("window_hours").and_then(|v| v.as_f64()).unwrap_or(168.0);
            let window_h = window_h.clamp(1.0, 2160.0);
            let granularity = args
                .get("granularity")
                .and_then(|v| v.as_str())
                .filter(|g| matches!(*g, "minute" | "hour" | "day"))
                .unwrap_or(if window_h <= 6.0 {
                    "minute"
                } else if window_h <= 48.0 {
                    "hour"
                } else {
                    "day"
                });
            let since = now().saturating_sub((window_h * 3600.0) as u64);
            // 查询上限按窗口大小动态计算，覆盖最小采样间隔（60s）下窗口内可能出现的
            // 全部行数，避免长窗口 + 密集采样时旧的固定上限悄悄丢最新数据；同时设硬
            // 上限保护内存/性能。最终发给模型的文本经聚合/降采样后长度与此无关。
            let limit = (((window_h * 3600.0 / 60.0) as u64).saturating_add(500)).min(200_000) as u32;
            let rows = db
                .list_metrics(&host.id, since, limit)
                .map_err(|e| format!("查询历史指标失败: {e}"))?;
            Ok(format_metric_trend(metric, &rows, window_h, granularity))
        }
        _ => {
            let effective = infer_tool_name(name, args);
            let normalized = normalize_tool(effective);
            if normalized != name {
                return Box::pin(execute_tool(db, russh, host, normalized, args)).await;
            }
            eprintln!("[agent] 未知工具调用: {name}，参数: {args}");
            Err(format!(
                "未知工具: {name}。可用工具：exec_command（执行命令）、read_file（读文件）、\
                 list_dir（列目录）、resource_usage（资源占用）、query_history（查询历史指标趋势）。\
                 请改用这些工具重试。"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_tool_name_keeps_nonempty_name() {
        let args = serde_json::json!({});
        assert_eq!(infer_tool_name("read_file", &args), "read_file");
    }

    #[test]
    fn infer_tool_name_infers_exec_command_from_command_arg() {
        let args = serde_json::json!({"command": "df -h"});
        assert_eq!(infer_tool_name("", &args), "exec_command");
    }

    #[test]
    fn infer_tool_name_infers_read_file_from_path_arg() {
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(infer_tool_name("", &args), "read_file");
    }

    /// 回归测试：AI 对话中出现过 query_history 调用漏填 name，
    /// 命中"未知工具"报错（问题截图场景），现在应能从 metric/window_hours 推断出来。
    #[test]
    fn infer_tool_name_infers_query_history_from_metric_arg() {
        let args = serde_json::json!({"metric": "mem", "window_hours": 168});
        assert_eq!(infer_tool_name("", &args), "query_history");

        let args2 = serde_json::json!({"window_hours": 24});
        assert_eq!(infer_tool_name("", &args2), "query_history");
    }

    #[test]
    fn infer_tool_name_returns_empty_when_unrecognizable() {
        let args = serde_json::json!({"foo": "bar"});
        assert_eq!(infer_tool_name("", &args), "");
    }

    /// 回归测试：resource_usage 是唯一无参数的工具，模型漏填 name 时其调用形态是空对象 `{}`，
    /// 此前会被误判为无法识别，甚至在 run_agent_loop 的空壳过滤逻辑中被静默丢弃，
    /// 导致模型宣布"再看一下资源快照"后对话就静默中断（未触发任何错误提示）。
    #[test]
    fn infer_tool_name_defaults_empty_object_to_resource_usage() {
        let args = serde_json::json!({});
        assert_eq!(infer_tool_name("", &args), "resource_usage");
    }
}
