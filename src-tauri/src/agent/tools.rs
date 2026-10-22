//! Agent 可调用工具的定义与执行：系统提示词、工具 JSON Schema、工具名推断、以及
//! 实际在远程服务器上执行工具调用的 `execute_tool`。

use super::trend::format_metric_trend;
use crate::db::Db;
use crate::i18n::{is_en, t};
use crate::models::{AiProvider, Host};
use crate::russh::RusshManager;
use crate::safety::{normalize_tool, sanitize};
use crate::util::{format_exec_output_full, now, shq};
use tokio_util::sync::CancellationToken;

pub(crate) fn system_prompt(
    host: &Host,
    provider: &AiProvider,
    model: &str,
    permission_mode: crate::models::PermissionMode,
) -> String {
    // 审批规则的说明必须与后端实际判定一致，避免误导模型：
    // - all：全部需要批准；smart：只读自动执行，写/危险命令需批准；none：直接执行
    // 提示词语言跟随界面语言：中文界面用中文提示词，英文界面用英文提示词
    let en = is_en();
    let catalog = tool_catalog(en);
    let names = tool_names(en);
    let approval_rule = match (permission_mode, en) {
        (crate::models::PermissionMode::All, false) => {
            "所有 exec_command 都会经过用户批准，获批后才执行，请先说明意图。".to_string()
        }
        (crate::models::PermissionMode::All, true) => {
            "Every exec_command goes through user approval and only runs once approved; state your intent first."
                .to_string()
        }
        (crate::models::PermissionMode::Smart, false) => {
            "exec_command 中只有简单、明确的安全只读命令会自动执行；其他命令（包括解释器、\
             脚本、管道、重定向、未知命令、敏感路径、修改/删除/安装/下载等）以及你标记 \
             requires_approval 的命令都会经过用户批准，获批后才执行，请先说明意图。\
             不要用 shell 引号/变量拼接规避审批。"
                .to_string()
        }
        (crate::models::PermissionMode::Smart, true) => {
            "Only simple, proven safe read-only exec_command calls run automatically. All other \
             commands (interpreters, scripts, pipes, redirections, unknown commands, sensitive \
             paths, modifying/deleting/installing/downloading, …) and commands you mark with \
             requires_approval go through user approval and only run once approved; state your \
             intent first. Do not use shell quoting or variable tricks to bypass approval."
                .to_string()
        }
        (crate::models::PermissionMode::None, false) => {
            "exec_command 会直接执行、不再逐条审批；但破坏性操作（删除、格式化、改权限、\
             停服务等）仍应明确提示风险并给出命令原文。"
                .to_string()
        }
        (crate::models::PermissionMode::None, true) => {
            "exec_command runs directly without per-command approval; for destructive operations \
             (deleting, formatting, changing permissions, stopping services, …) still call out the \
             risk explicitly and show the exact command."
                .to_string()
        }
    };
    if is_en() {
        return format!(
            "You are buffTerm, running inside the user's local SSH management tool, helping the user \
             manage remote servers.\n\
             Capabilities are currently provided by the {} platform, and the configured underlying \
             model is {}.\n\
             Currently connected server: {} ({}@{}:{})\n\
             Available tools: {catalog}.\n\
             Rules:\n\
             1. {}\n\
             2. Command output may be truncated; answer only from the information you have and never \
                make things up.\n\
             3. For destructive operations (deleting, formatting, changing permissions, stopping \
                services, …) call out the risk explicitly and show the exact command.\n\
             4. Answer in English, concisely, professionally and in a well-structured way.\n\
             5. Identity: when the user asks which model you are or who develops you, answer truthfully \
                that you are driven by {} with the configured model {}, and that you are buffTerm; do \
                not claim to be any other AI assistant (such as ChatGPT, Claude, Gemini, …) and do not \
                invent version numbers or vendor information.\n\
             6. Tool call conventions: the tool name must be one of {names}; every tool call must include a \
                non-empty name field, never invent new tool names, and put parameters in arguments (a \
                JSON object).\n\
             7. When the user asks about changes — \"how has it been lately\", \"is anything wrong\", \
                \"has it slowed down\" — prefer query_history to look at trends instead of only calling \
                resource_usage for the current value. Trends are more diagnostic than absolute values: \
                a disk that grew from 30% to 78% is more urgent than one sitting steady at 78%. \
                query_history returns historical series, slopes and extrapolated predictions (e.g. \
                \"at the current rate, X will reach 90% in N days\"), which a single snapshot cannot \
                provide. Choose granularity and window_hours to match your goal: minute + a small window \
                for detail over the last hours; hour + a 24–48 hour window for today's trend; day + a \
                larger window (up to 90 days) for long-term trends and capacity planning. If unsure, \
                omit granularity and it will be chosen from window_hours.\n\
             8. For multi-step tasks (installing, deploying, migrating, troubleshooting, bulk edits) \
                call update_task_plan first to establish a plan; update the plan after every completed \
                or failed step, and record failed attempts in `failed` with the reason. Never repeat an \
                approach that already failed without new evidence; the plan is injected every round and \
                is not lost when history is compressed. update_task_plan only updates the local task \
                plan, does not touch servers and needs no approval.",
            provider.name,
            model,
            host.name,
            host.username,
            host.address,
            host.port,
            approval_rule,
            provider.name,
            model
        );
    }
    format!(
        "你是 buffTerm，运行在用户本地的 SSH 管理工具中，帮助用户管理远程服务器。\n\
         当前由 {} 平台提供能力，当前配置的底层模型是 {}。\n\
         当前连接的服务器：{}（{}@{}:{}）\n\
         可用工具：{catalog}。\n\
         规则：\n\
         1. {}\n\
         2. 命令输出可能被截断，只基于已有信息回答，不要编造。\n\
         3. 遇到破坏性操作（删除、格式化、改权限、停服务等）时，明确提示风险并给出命令原文。\n\
         4. 使用中文回答，简洁、专业、有条理。\n\
         5. 身份说明：当用户询问“你是什么模型/你由谁开发”时，如实回答你由 {} 驱动、配置的模型为 {}，
            以及你是 buffTerm；不要声称自己是任何其他 AI 助手（如 ChatGPT、Claude、Gemini 等），
            也不要编造版本号或开发厂商信息。\n\
         6. 工具调用约定：工具名称必须是以下之一——{names}；
            每次工具调用都必须包含完整的 name 字段且不能为空，不要发明新工具名；参数放入 arguments（JSON 对象）。\n\
         7. 当用户询问“最近怎么样”、“有没有异常”、“是不是变慢了”等涉及变化的问题时，优先调用 query_history 查看趋势，
            而不是只调用 resource_usage 看当下值。趋势比绝对值更有诊断价值——一个从 30% 涨到 78% 的磁盘比一个稳定在 78% 的磁盘更紧急。
            query_history 会返回历史序列、变化斜率和外推预测（如“按当前增速，X 天后达到 90%”），这是单次快照无法提供的信息。
            根据分析目的选择合适的 granularity 和 window_hours：看最近几小时的细节波动用 minute + 小窗口；
            看今天的走势用 hour + 24～48 小时窗口；看长周期趋势/容量规划用 day + 更大窗口（最长 90 天）。
            不确定时可不填 granularity，会按 window_hours 自动选择合适粒度。\n\
         8. 多步任务（安装、部署、迁移、排障、批量修改等）开始前必须先调用 update_task_plan 建立计划；
            每完成或失败一步后必须更新台账，失败尝试要写入 failed 并说明原因。
            不得在没有新依据时重复已失败的同一做法；台账会在每轮自动注入，不会因历史压缩而丢失。
            update_task_plan 只更新任务台账，不访问服务器，不需要审批。",
        provider.name,
        model,
        host.name,
        host.username,
        host.address,
        host.port,
        approval_rule,
        provider.name,
        model
    )
}

pub(crate) fn tools_schema() -> serde_json::Value {
    let mut tools = serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "exec_command",
                "description": t(
                    "在远程服务器上执行一条 shell 命令并返回输出。默认超时 30 秒。",
                    "Run a shell command on the remote server and return its output. Default timeout 30s."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": t("要执行的 shell 命令", "Shell command to run") },
                        "timeout_secs": { "type": "number", "description": t("超时秒数，默认 30", "Timeout in seconds, default 30") },
                        "requires_approval": { "type": "boolean", "description": t(
                            "如果你认为该命令有危险（删除、格式化、修改系统状态、影响服务等），设为 true，便于用户安全策略处理",
                            "Set to true if you consider the command dangerous (deleting, formatting, changing system state, affecting services, …) so the user's safety policy can handle it"
                        ) }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": t("读取远程服务器上的文件内容", "Read the contents of a file on the remote server"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": t("远程文件路径", "Remote file path") }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": t("列出远程服务器上的目录内容", "List the contents of a directory on the remote server"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": t("目录路径，默认当前目录", "Directory path, defaults to the current directory") }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "resource_usage",
                "description": t(
                    "查看服务器磁盘、内存、负载和 CPU/内存占用最高的进程",
                    "Show server disk, memory, load and the processes using the most CPU/memory"
                ),
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_history",
                "description": t(
                    "查询当前服务器的历史指标趋势。用于判断资源使用是否在持续增长、是否有周期性波动、是否接近告警阈值。当用户问'最近怎么样'、'有没有问题'、'变慢了吗'时优先调用此工具，而不是只看当下快照。",
                    "Query historical metric trends for the current server. Use it to tell whether resource usage keeps growing, fluctuates periodically, or is approaching a warning threshold. When the user asks 'how has it been lately', 'is anything wrong' or 'has it slowed down', prefer this tool over a single current snapshot."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "metric": {
                            "type": "string",
                            "enum": ["cpu", "mem", "load", "disk"],
                            "description": t(
                                "要查询的指标：cpu=CPU使用率, mem=内存使用率, load=系统负载, disk=磁盘使用率",
                                "Metric to query: cpu=CPU usage, mem=memory usage, load=system load, disk=disk usage"
                            )
                        },
                        "window_hours": {
                            "type": "number",
                            "description": t(
                                "回溯多少小时，默认 168（7 天），最大 2160（90 天）",
                                "How many hours to look back; default 168 (7 days), maximum 2160 (90 days)"
                            )
                        },
                        "granularity": {
                            "type": "string",
                            "enum": ["minute", "hour", "day"],
                            "description": t(
                                "数据聚合粒度：minute=原始采样点（适合看最近几小时的细节波动），hour=按小时取均值（适合看当天走势），day=按天取均值（适合看多天/长周期趋势）。不填时按 window_hours 自动选择：≤6 小时用 minute，≤48 小时用 hour，否则用 day。",
                                "Data aggregation granularity: minute=raw samples (good for detail over the last hours), hour=hourly averages (good for today's trend), day=daily averages (good for multi-day/long-term trends). When omitted it is chosen from window_hours: minute for ≤6h, hour for ≤48h, otherwise day."
                            )
                        }
                    },
                    "required": ["metric"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_task_plan",
                "description": t(
                    "创建或更新当前任务的结构化计划。多步任务（安装、部署、迁移、排障）开始前必须先调用一次，并在每完成或失败一步后更新。该工具只更新本地任务台账，不访问服务器、不需要审批。",
                    "Create or update the structured plan for the current task. Call it once before starting a multi-step task (installing, deploying, migrating, troubleshooting) and update it after every completed or failed step. It only updates the local task plan, does not touch servers and needs no approval."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": { "type": "string", "description": t("一句话目标", "One-sentence goal") },
                        "constraints": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": t(
                                "必须遵守的约束，如端口、版本、不能重启的服务等",
                                "Constraints that must be respected, e.g. ports, versions, services that must not be restarted"
                            )
                        },
                        "completed": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": t("已完成的步骤", "Completed steps")
                        },
                        "pending": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": t("待办步骤", "Pending steps")
                        },
                        "failed": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": t(
                                "失败尝试及原因，避免重复同一做法",
                                "Failed attempts and why they failed, to avoid repeating the same approach"
                            )
                        },
                        "current_step": { "type": "string", "description": t("当前正在执行的步骤", "Step currently being executed") }
                    },
                    "required": ["goal"]
                }
            }
        },
    ]);
    // 本地回查工具的 schema 由 context_policy 统一维护（模型可见文案的唯一出处）
    tools
        .as_array_mut()
        .expect("tools schema is an array")
        .push(super::context_policy::recall_context_schema());
    tools
}

/// 解析工具调用参数：空串 → 空对象（无参数工具如 resource_usage）；合法 JSON 对象 → 原样；
/// 其余（截断的 JSON、裸字符串等）→ 错误。此前会把原文包成 {"command": raw}，
/// 模型输出被截断的 read_file 参数就会被当成 shell 命令送到远端执行。
pub(crate) fn parse_args(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(t(
            "工具参数必须是 JSON 对象",
            "Tool arguments must be a JSON object",
        )
        .to_string()),
        Err(e) => Err(format!(
            "{}: {e}",
            t("工具参数不是合法 JSON", "Tool arguments are not valid JSON")
        )),
    }
}

/// 模型漏填工具名（`name` 为空）时，根据参数形态推断真实工具：
/// reference → recall_context，command → exec_command，path → read_file，
/// metric/window_hours → query_history，空对象（无法从参数形态判断）→ resource_usage
/// （唯一不需要参数的工具，即使猜错，resource_usage 也是只读且无副作用的操作，代价可接受）。
/// 若 `name` 非空，原样返回。
pub(crate) fn infer_tool_name<'a>(name: &'a str, args: &serde_json::Value) -> &'a str {
    if !name.trim().is_empty() {
        return name;
    }
    if args
        .get("reference")
        .and_then(|r| r.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        "recall_context"
    } else if args
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
    } else if args.get("goal").is_some()
        || args.get("pending").is_some()
        || args.get("completed").is_some()
        || args.get("current_step").is_some()
    {
        "update_task_plan"
    } else if args.as_object().map(|m| m.is_empty()).unwrap_or(false) {
        "resource_usage"
    } else {
        name
    }
}

/// Agent 可调用的工具集合。模型给出的原始工具名只在 `resolve` 处解析一次，
/// 之后审批、执行、审计都用同一个值，避免用别名/空名绕过审批。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    /// 在远程服务器上执行 shell 命令
    ExecCommand,
    /// 读取远程服务器上的文件内容
    ReadFile,
    /// 列出远程服务器上的目录内容
    ListDir,
    /// 查看服务器磁盘、内存、负载和占用最高的进程
    ResourceUsage,
    /// 查询当前服务器的历史指标趋势
    QueryHistory,
    /// 创建或更新本地任务台账（不访问远程主机）
    UpdateTaskPlan,
    /// 回查本会话的历史轮次/归档输出原文（本地只读，不访问远程主机）
    RecallContext,
}

impl Tool {
    /// 全部工具，顺序即系统提示词与错误消息里的展示顺序
    const ALL: [Tool; 7] = [
        Tool::ExecCommand,
        Tool::ReadFile,
        Tool::ListDir,
        Tool::ResourceUsage,
        Tool::QueryHistory,
        Tool::UpdateTaskPlan,
        Tool::RecallContext,
    ];

    /// 与模型约定、写回历史的规范工具名
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tool::ExecCommand => "exec_command",
            Tool::ReadFile => "read_file",
            Tool::ListDir => "list_dir",
            Tool::ResourceUsage => "resource_usage",
            Tool::QueryHistory => "query_history",
            Tool::UpdateTaskPlan => "update_task_plan",
            Tool::RecallContext => "recall_context",
        }
    }

    /// 工具清单里跟在名字后面的用途说明
    fn summary(self, en: bool) -> &'static str {
        match (self, en) {
            (Tool::ExecCommand, false) => "执行命令",
            (Tool::ExecCommand, true) => "run a command",
            (Tool::ReadFile, false) => "读文件",
            (Tool::ReadFile, true) => "read a file",
            (Tool::ListDir, false) => "列目录",
            (Tool::ListDir, true) => "list a directory",
            (Tool::ResourceUsage, false) => "资源占用",
            (Tool::ResourceUsage, true) => "resource usage",
            (Tool::QueryHistory, false) => "查询历史指标趋势",
            (Tool::QueryHistory, true) => "query historical metric trends",
            (Tool::UpdateTaskPlan, false) => "更新任务台账",
            (Tool::UpdateTaskPlan, true) => "update the task plan",
            (Tool::RecallContext, false) => "回查上下文原文",
            (Tool::RecallContext, true) => "recall original context",
        }
    }

    /// 把模型给出的原始工具名解析为 Tool：先按参数形态补全空名，再归一化别名；
    /// 无法识别的名字返回 None，由调用方按未知工具处理。
    pub(crate) fn resolve(raw_name: &str, args: &serde_json::Value) -> Option<Tool> {
        let normalized = normalize_tool(infer_tool_name(raw_name, args));
        Tool::ALL
            .iter()
            .copied()
            .find(|tool| tool.as_str() == normalized)
    }
}

/// 「名字（用途说明）」清单：供系统提示词和未知工具错误消息共用，
/// 保证两处列出的工具永远一致（此前错误消息里漏了 update_task_plan）。
pub(crate) fn tool_catalog(en: bool) -> String {
    Tool::ALL
        .iter()
        .map(|tool| {
            if en {
                format!("{} ({})", tool.as_str(), tool.summary(true))
            } else {
                format!("{}（{}）", tool.as_str(), tool.summary(false))
            }
        })
        .collect::<Vec<_>>()
        .join(if en { ", " } else { "、" })
}

/// 「工具名称必须是以下之一」清单：只列规范名，不带用途说明。
pub(crate) fn tool_names(en: bool) -> String {
    Tool::ALL
        .iter()
        .map(|tool| tool.as_str())
        .collect::<Vec<_>>()
        .join(if en { ", " } else { "、" })
}

/// 模型调用了无法识别的工具名时回填给它的错误消息：列出全部可用工具，引导其改用重试。
pub(crate) fn unknown_tool_message(name: &str) -> String {
    let en = is_en();
    let catalog = tool_catalog(en);
    if en {
        format!("Unknown tool: {name}. Available tools: {catalog}. Please retry with these tools.")
    } else {
        format!("未知工具: {name}。可用工具：{catalog}。请改用这些工具重试。")
    }
}

/// `query_history` 工具的实现：查询本地指标库并格式化为趋势文本（纯本地查询，不访问远程主机）。
fn query_history(db: &Db, host: &Host, args: &serde_json::Value) -> Result<String, String> {
    let metric = args.get("metric").and_then(|v| v.as_str()).unwrap_or("cpu");
    let window_h = args
        .get("window_hours")
        .and_then(|v| v.as_f64())
        .unwrap_or(168.0);
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
    // 全部行数；同时设硬上限（5 万行）保护内存/性能，超过时保留最新样本，
    // 日粒度/小时粒度的趋势聚合不受影响
    let limit = (((window_h * 3600.0 / 60.0) as u64).saturating_add(500)).min(50_000) as u32;
    let rows = db
        .list_metrics(&host.id, since, limit)
        .map_err(|e| {
            format!(
                "{}: {e}",
                t("查询历史指标失败", "Failed to query historical metrics")
            )
        })?;
    Ok(format_metric_trend(metric, &rows, window_h, granularity))
}

/// 在远程服务器上执行已解析的工具调用。`update_task_plan` 由 Agent 在本地处理，
/// 永远不会走到这里（防御性返回错误）；未知工具在调用前的 `Tool::resolve` 处就被拒绝。
pub(crate) async fn execute_tool(
    db: &Db,
    russh: &RusshManager,
    host: &Host,
    tool: Tool,
    args: &serde_json::Value,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let (command, timeout_secs): (String, u64) = match tool {
        Tool::ExecCommand => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| t("缺少 command 参数", "Missing command parameter").to_string())?;
            let timeout = args
                .get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(30)
                .clamp(1, 600);
            (command.to_string(), timeout)
        }
        Tool::ReadFile => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| t("缺少 path 参数", "Missing path parameter").to_string())?;
            (format!("cat -- {}", shq(path)), 15)
        }
        Tool::ListDir => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            (format!("ls -lah -- {}", shq(path)), 15)
        }
        Tool::ResourceUsage => {
            let script = if is_en() {
                "echo '-- Disk --'; df -h; echo; echo '-- Memory --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- Load --'; uptime; echo; echo '-- TOP processes --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8"
            } else {
                "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8"
            };
            (script.to_string(), 25)
        }
        Tool::QueryHistory => return query_history(db, host, args),
        Tool::UpdateTaskPlan => return Err(t(
            "update_task_plan 由 Agent 在本地处理，不在远程主机上执行",
            "update_task_plan is handled locally by the agent and never runs on the remote host",
        )
        .to_string()),
        Tool::RecallContext => {
            return Err(t(
                "recall_context 由 Agent 在本地处理，不在远程主机上执行",
                "recall_context is handled locally by the agent and never runs on the remote host",
            )
            .to_string())
        }
    };
    let out = russh
        .exec_cancellable(
            host,
            &command,
            std::time::Duration::from_secs(timeout_secs),
            cancel,
        )
        .await?;
    // 返回完整格式化输出（不在此截断）：超长输出由 Agent 归档后回填有界预览
    Ok(sanitize(&format_exec_output_full(&out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AiProvider, AuthType, Host, PermissionMode};

    fn fixture_host() -> Host {
        Host {
            id: "h1".to_string(),
            name: "web".to_string(),
            address: "10.0.0.1".to_string(),
            port: 22,
            username: "root".to_string(),
            auth_type: AuthType::Password,
            key_path: None,
            notes: None,
            created_at: 0,
        }
    }

    fn fixture_provider() -> AiProvider {
        AiProvider {
            id: "p1".to_string(),
            name: "DeepSeek".to_string(),
            base_url: "https://x".to_string(),
            protocol: "openai-compatible".to_string(),
            enabled: true,
            created_at: 0,
            models: vec![],
        }
    }

    #[test]
    fn tool_resolve_normalizes_aliases_and_infers_empty_names() {
        assert_eq!(
            Tool::resolve("shell", &serde_json::json!({"command": "ls"})),
            Some(Tool::ExecCommand)
        );
        assert_eq!(
            Tool::resolve("", &serde_json::json!({"command": "rm -rf /"})),
            Some(Tool::ExecCommand)
        );
        assert_eq!(
            Tool::resolve("ls", &serde_json::json!({})),
            Some(Tool::ListDir)
        );
        assert_eq!(
            Tool::resolve("", &serde_json::json!({})),
            Some(Tool::ResourceUsage)
        );
        assert_eq!(
            Tool::resolve("bash", &serde_json::json!({"command": "ls"})),
            None
        );
        assert_eq!(
            Tool::resolve("", &serde_json::json!({"foo": "bar"})),
            None
        );
    }

    #[test]
    fn tool_catalog_and_names_match_prompt_text() {
        assert_eq!(
            tool_catalog(false),
            "exec_command（执行命令）、read_file（读文件）、list_dir（列目录）、resource_usage（资源占用）、query_history（查询历史指标趋势）、update_task_plan（更新任务台账）、recall_context（回查上下文原文）"
        );
        assert_eq!(
            tool_catalog(true),
            "exec_command (run a command), read_file (read a file), list_dir (list a directory), resource_usage (resource usage), query_history (query historical metric trends), update_task_plan (update the task plan), recall_context (recall original context)"
        );
        assert_eq!(
            tool_names(false),
            "exec_command、read_file、list_dir、resource_usage、query_history、update_task_plan、recall_context"
        );
        assert_eq!(
            tool_names(true),
            "exec_command, read_file, list_dir, resource_usage, query_history, update_task_plan, recall_context"
        );
    }

    #[test]
    fn unknown_tool_message_lists_every_tool() {
        // 此前错误消息里漏了 update_task_plan，模型被纠正后仍不知道该工具存在
        let msg = unknown_tool_message("bash");
        for tool in Tool::ALL {
            assert!(msg.contains(tool.as_str()), "缺少 {}", tool.as_str());
        }
    }

    #[test]
    fn system_prompt_mentions_every_tool() {
        let host = fixture_host();
        let provider = fixture_provider();
        let prompt = system_prompt(&host, &provider, "m", PermissionMode::Smart);
        for tool in Tool::ALL {
            assert!(prompt.contains(tool.as_str()), "缺少 {}", tool.as_str());
        }
        assert!(prompt.contains(
            "可用工具：exec_command（执行命令）、read_file（读文件）、list_dir（列目录）、resource_usage（资源占用）、query_history（查询历史指标趋势）、update_task_plan（更新任务台账）、recall_context（回查上下文原文）。"
        ));
        assert!(prompt.contains(
            "工具名称必须是以下之一——exec_command、read_file、list_dir、resource_usage、query_history、update_task_plan、recall_context；"
        ));
    }

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

    #[test]
    fn infer_tool_name_infers_update_task_plan_from_goal() {
        let args = serde_json::json!({"goal": "部署 nginx", "pending": ["启动服务"]});
        assert_eq!(infer_tool_name("", &args), "update_task_plan");
    }

    /// recall_context 的解析链路：规范名、空名+reference 推断、
    /// schema 里出现且与目录一致、不会在 resolve 处被误判为未知工具。
    #[test]
    fn recall_context_resolves_infers_and_appears_in_schema() {
        assert_eq!(
            Tool::resolve(
                "recall_context",
                &serde_json::json!({"reference": "round:0"})
            ),
            Some(Tool::RecallContext)
        );
        assert_eq!(
            Tool::resolve("", &serde_json::json!({"reference": "output:abc"})),
            Some(Tool::RecallContext),
            "空名 + reference 参数应推断为 recall_context"
        );
        // reference 推断优先级高于其他形态
        assert_eq!(
            infer_tool_name(
                "",
                &serde_json::json!({"reference": "round:1", "path": "/x"})
            ),
            "recall_context"
        );
        let schema = tools_schema();
        let schema_names: Vec<&str> = schema
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        for tool in Tool::ALL {
            assert!(
                schema_names.contains(&tool.as_str()),
                "tools_schema 缺少 {}",
                tool.as_str()
            );
        }
    }

    /// recall_context / update_task_plan 是本地工具：execute_tool 对它们直接报错，
    /// 不会触达 SSH 执行层。
    #[test]
    fn local_only_tools_never_reach_remote_execute() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = std::env::temp_dir().join(format!(
                "buffterm-tools-test-{}.sqlite",
                uuid::Uuid::new_v4()
            ));
            let db = Db::open(&path).unwrap();
            let russh = RusshManager::new();
            let host = fixture_host();
            let cancel = CancellationToken::new();
            for tool in [Tool::RecallContext, Tool::UpdateTaskPlan] {
                let err = execute_tool(
                    &db,
                    &russh,
                    &host,
                    tool,
                    &serde_json::json!({"reference": "round:0"}),
                    &cancel,
                )
                .await
                .unwrap_err();
                assert!(
                    err.contains("本地") || err.contains("locally"),
                    "{} 应报本地工具错误: {err}",
                    tool.as_str()
                );
            }
            drop(db);
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn parse_args_accepts_empty_and_objects() {
        assert_eq!(parse_args("").unwrap(), serde_json::json!({}));
        assert_eq!(parse_args("  ").unwrap(), serde_json::json!({}));
        assert_eq!(parse_args(r#"{"command":"df -h"}"#).unwrap(), serde_json::json!({"command":"df -h"}));
    }

    /// 回归：截断 / 裸字符串参数此前会被包成 {"command": raw} 当 shell 命令执行
    #[test]
    fn parse_args_rejects_malformed_instead_of_treating_as_command() {
        assert!(parse_args(r#"{"path": "/etc/ho"#).is_err());
        assert!(parse_args(r#""df -h""#).is_err());
        assert!(parse_args("rm -rf /tmp/x").is_err());
        assert!(parse_args("[1,2]").is_err());
    }
}
