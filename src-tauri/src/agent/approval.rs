//! 工具调用的审批判定：按权限模式与危险规则决定某次调用是否需要用户批准，
//! 以及需要时随审批卡片展示的原因。

use super::tools::Tool;
use crate::i18n::t;
use crate::models::PermissionMode;
use crate::safety::{command_has_sensitive_path, is_dangerous, is_sensitive_path, is_write_operation};

/// 返回 Some(原因)（随审批卡片展示）表示需要审批；None 表示直接执行。
/// 基于已解析的 Tool 判定，与实际执行同一个值，
/// 避免模型用别名/空名调用 exec_command 绕过审批门。
pub(super) fn approval_reason(
    mode: PermissionMode,
    tool: Tool,
    args: &serde_json::Value,
    danger_rules: &[String],
) -> Option<String> {
    approval_reason_with_sensitive(mode, tool, args, danger_rules, false)
}

/// 与 `approval_reason` 相同，但允许调用方先通过远端 realpath 判断
/// `cat/grep` 的参数是否指向敏感文件（例如符号链接）。
pub(super) fn approval_reason_with_sensitive(
    mode: PermissionMode,
    tool: Tool,
    args: &serde_json::Value,
    danger_rules: &[String],
    resolved_sensitive: bool,
) -> Option<String> {
    // 任务台账与上下文回查只动本地状态，不访问远程主机，任何模式都不需要审批
    if matches!(tool, Tool::UpdateTaskPlan | Tool::RecallContext) {
        return None;
    }
    match mode {
        PermissionMode::None => None,
        PermissionMode::All => Some(
            t(
                "全部审核：所有命令执行前都需要批准",
                "Approve all: every command needs approval before running",
            )
            .to_string(),
        ),
        PermissionMode::Smart => {
            // 读取敏感路径（私钥、凭据、cron/登录脚本等）即使在智能审核下
            // 也需要用户确认，避免模型把秘密直接送进上下文或外部平台。
            if tool == Tool::ReadFile {
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                return if is_sensitive_path(path) {
                    Some(
                        t(
                            "读取敏感路径，需要批准",
                            "Reading a sensitive path needs approval",
                        )
                        .to_string(),
                    )
                } else {
                    None
                };
            }
            if tool != Tool::ExecCommand {
                return None;
            }
            let marked = args
                .get("requires_approval")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let c = command.to_ascii_lowercase();
            if marked {
                Some(
                    t(
                        "模型标记该命令需要审批",
                        "The model flagged this command as needing approval",
                    )
                    .to_string(),
                )
            } else if is_dangerous(command) {
                Some(
                    t(
                        "命中内置危险命令规则",
                        "Matched a built-in dangerous command rule",
                    )
                    .to_string(),
                )
            } else if let Some(p) = danger_rules
                .iter()
                .find(|p| !p.trim().is_empty() && c.contains(&p.to_ascii_lowercase()))
            {
                Some(format!(
                    "{}{p}",
                    t("命中自定义规则：", "Matched custom rule: ")
                ))
            } else if resolved_sensitive || command_has_sensitive_path(command) {
                Some(
                    t(
                        "命令包含敏感路径，需要批准",
                        "The command contains a sensitive path and needs approval",
                    )
                    .to_string(),
                )
            } else if is_write_operation(command) {
                // 默认拒绝：只有白名单内的简单只读命令才自动执行；其余
                // （含解释器、脚本、管道、重定向和未知命令）一律审批。
                Some(
                    t(
                        "命令不是可自动执行的安全只读命令，需要批准",
                        "The command is not a proven safe read-only command and needs approval",
                    )
                    .to_string(),
                )
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smart_approval(args: serde_json::Value, rules: &[&str]) -> Option<String> {
        let rules: Vec<String> = rules.iter().map(|s| s.to_string()).collect();
        approval_reason(PermissionMode::Smart, Tool::ExecCommand, &args, &rules)
    }

    #[test]
    fn none_mode_never_needs_approval() {
        assert!(approval_reason(
            PermissionMode::None,
            Tool::ExecCommand,
            &serde_json::json!({"command": "rm -rf /"}),
            &[],
        )
        .is_none());
    }

    #[test]
    fn all_mode_approves_every_tool() {
        let reason = approval_reason(
            PermissionMode::All,
            Tool::ReadFile,
            &serde_json::json!({"path": "/etc/hosts"}),
            &[],
        )
        .expect("all 模式应需要审批");
        assert!(reason.contains("全部审核"));
    }

    #[test]
    fn update_task_plan_never_needs_approval() {
        assert!(approval_reason(
            PermissionMode::All,
            Tool::UpdateTaskPlan,
            &serde_json::json!({"goal": "x"}),
            &[],
        )
        .is_none());
    }

    /// 本地只读回查工具：即使「全部审核」模式也不走远程审批。
    #[test]
    fn recall_context_never_needs_approval() {
        for mode in [
            PermissionMode::All,
            PermissionMode::Smart,
            PermissionMode::None,
        ] {
            assert!(
                approval_reason(
                    mode,
                    Tool::RecallContext,
                    &serde_json::json!({"reference": "round:0"}),
                    &[],
                )
                .is_none(),
                "{mode:?} 模式下 recall_context 不应需要审批"
            );
        }
    }

    #[test]
    fn smart_mode_skips_non_exec_tools() {
        assert!(approval_reason(
            PermissionMode::Smart,
            Tool::ReadFile,
            &serde_json::json!({"path": "/var/log/syslog"}),
            &[],
        )
        .is_none());
    }

    #[test]
    fn smart_mode_flags_sensitive_read_and_unknown_commands() {
        let reason = approval_reason(
            PermissionMode::Smart,
            Tool::ReadFile,
            &serde_json::json!({"path": "/etc/shadow"}),
            &[],
        )
        .expect("读取敏感路径需要审批");
        assert!(reason.contains("敏感") || reason.contains("sensitive"));

        for command in [
            "cat /etc/passwd",
            "awk 'BEGIN{system(\"unlink /tmp/x\")}'",
            "r''m -rf /tmp/x",
            "bash -c 'unlink /tmp/x'",
        ] {
            assert!(
                approval_reason(
                    PermissionMode::Smart,
                    Tool::ExecCommand,
                    &serde_json::json!({"command": command}),
                    &[],
                )
                .is_some(),
                "{command} 应要求审批"
            );
        }
    }

    #[test]
    fn smart_mode_allows_readonly_command() {
        assert!(smart_approval(serde_json::json!({"command": "ls -la"}), &[]).is_none());
    }

    #[test]
    fn smart_mode_flags_dangerous_command() {
        let reason = smart_approval(serde_json::json!({"command": "rm -rf /tmp/x"}), &[]).unwrap();
        assert!(reason.contains("危险"));
    }

    #[test]
    fn smart_mode_flags_write_operation() {
        let reason =
            smart_approval(serde_json::json!({"command": "echo hi > /etc/motd"}), &[]).unwrap();
        assert!(
            reason.contains("写操作") || reason.contains("安全只读") || reason.contains("read-only"),
            "{reason}"
        );
    }

    #[test]
    fn smart_mode_flags_model_marked_command() {
        let reason = smart_approval(
            serde_json::json!({"command": "ls", "requires_approval": true}),
            &[],
        )
        .unwrap();
        assert!(reason.contains("模型标记"));
    }

    #[test]
    fn smart_mode_flags_custom_rule() {
        let reason = smart_approval(
            serde_json::json!({"command": "cat /opt/SECRET.conf"}),
            &["secret"],
        )
        .unwrap();
        assert!(reason.contains("自定义规则"));
    }

    #[test]
    fn smart_mode_ignores_blank_custom_rules() {
        assert!(smart_approval(serde_json::json!({"command": "ls"}), &["  "]).is_none());
    }

    #[test]
    fn model_mark_takes_precedence_over_builtin_rules() {
        // requires_approval 与内置危险规则同时命中时，原因按判定顺序取「模型标记」
        let reason = smart_approval(
            serde_json::json!({"command": "rm -rf /", "requires_approval": true}),
            &[],
        )
        .unwrap();
        assert!(reason.contains("模型标记"));
    }

    #[test]
    fn alias_and_empty_name_cannot_bypass_approval() {
        // 审批基于 resolve 后的 Tool：别名/空名都会解析成 exec_command 走同一道门
        let args = serde_json::json!({"command": "rm -rf /"});
        for raw in ["shell", ""] {
            let tool = Tool::resolve(raw, &args).expect("应能解析为 exec_command");
            assert!(
                approval_reason(PermissionMode::Smart, tool, &args, &[]).is_some(),
                "原始名 {raw:?} 不应绕过审批"
            );
        }
        assert!(Tool::resolve("bash", &args).is_none());
    }
}
