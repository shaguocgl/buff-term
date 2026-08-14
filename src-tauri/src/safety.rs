//! 安全相关判定与脱敏：工具名归一化、危险命令检测、只读/写操作判定、输出脱敏、巡检命令白名单。

use regex::Regex;
use std::sync::OnceLock;

/// 常见工具名别名归一化，降低模型幻觉导致的调用失败。
pub(crate) fn normalize_tool(name: &str) -> &str {
    match name {
        "exec" | "shell" | "run_command" | "run" | "command" | "execute" => "exec_command",
        "read" | "cat" | "readfile" | "read_file_content" => "read_file",
        "ls" | "list" | "listdir" | "dir" | "list_directory" => "list_dir",
        "resources" | "usage" | "system_status" | "monitor" | "resource_usage_show" => {
            "resource_usage"
        }
        _ => name,
    }
}

/// 命令输出进入模型上下文前过滤敏感信息。
pub(crate) fn sanitize(text: &str) -> String {
    struct Rule {
        re: Regex,
        keep_key: bool,
    }
    static RE: OnceLock<Vec<Rule>> = OnceLock::new();
    let res = RE.get_or_init(|| {
        vec![
            // 常见密钥/口令键值对：password=xxx / token: "xxx"
            Rule {
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*["']?)[^\s"',;]{6,}"#,
                )
                .unwrap(),
                keep_key: true,
            },
            // AWS Access Key
            Rule { re: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(), keep_key: false },
            // OpenAI / DeepSeek 风格 sk- 密钥
            Rule { re: Regex::new(r"sk-[A-Za-z0-9_\-]{16,}").unwrap(), keep_key: false },
            // PEM 私钥块
            Rule {
                re: Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                keep_key: false,
            },
            // Bearer / Basic 认证头
            Rule {
                re: Regex::new(r"(?i)authorization:\s*(basic|bearer)\s+[^\r\n]+").unwrap(),
                keep_key: false,
            },
            Rule {
                re: Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._\-]{20,}").unwrap(),
                keep_key: false,
            },
            // 常见云厂商密钥片段
            Rule {
                re: Regex::new(
                    r#"(?i)(access[_-]?key[_-]?id|secret[_-]?access[_-]?key)(\s*[:=]\s*["']?)[^\s"',;]{10,}"#,
                )
                .unwrap(),
                keep_key: true,
            },
        ]
    });
    let mut out = text.to_string();
    for rule in res {
        out = if rule.keep_key {
            rule.re.replace_all(&out, "${1}${2}***").to_string()
        } else {
            rule.re.replace_all(&out, "***").to_string()
        };
    }
    out
}

/// 智能审核模式下判断命令是否有风险（内置危险模式，子串匹配）。
pub(crate) fn is_dangerous(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "rm -r",
        "mkfs",
        "dd if=",
        "iptables",
        "ufw ",
        "systemctl stop",
        "systemctl restart",
        "systemctl disable",
        "systemctl mask",
        "shutdown",
        "reboot",
        "poweroff",
        "chmod -r",
        "chown -r",
        "fdisk",
        "parted",
        "pvremove",
        "vgremove",
        "lvremove",
        "userdel",
        "groupdel",
        "drop database",
        "truncate table",
        "delete from",
        "kill -9",
        ">/dev/sd",
    ];
    PATTERNS.iter().any(|p| c.contains(p))
}

/// 只读模式下判断命令是否包含写操作（修改 / 删除 / 安装 / 网络传输 / 重定向写文件等）。
pub(crate) fn is_write_operation(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    // 预置危险命令（删除、格式化、关机等）一律视为写操作
    if is_dangerous(&c) {
        return true;
    }
    // 重定向写文件：排除 /dev/null 丢弃与 fd 复制等无害写法
    let cleaned = c
        .replace("2>/dev/null", "")
        .replace(">/dev/null", "")
        .replace("1>/dev/null", "")
        .replace("2>&1", "")
        .replace(">&2", "")
        .replace(">&1", "")
        .replace("2>&-", "");
    if cleaned.contains('>') {
        return true;
    }
    // 常见写类命令关键词（子串匹配，宁可多拦）
    const WRITE_CMDS: &[&str] = &[
        "touch ", "mkdir ", "rmdir ", "rm ", "mv ", "cp ", "ln ", "tee ",
        "chmod", "chown", "chattr", "chgrp", "dd ",
        "mkfs", "mount ", "umount", "fdisk", "parted", "swapoff", "swapon",
        "useradd", "userdel", "usermod", "groupadd", "groupdel", "passwd ", "chage",
        "systemctl", "service ", "kill ", "pkill", "killall", "nohup ",
        "reboot", "shutdown", "poweroff", "halt ", "init ",
        "install ", "sed -i", "perl -i", "awk -i", "vim ", "vi ", "nano ", "ed ",
        "crontab", "batch ",
        "apt ", "apt-get", "yum ", "dnf ", "brew ", "zypper", "pacman ",
        "pip install", "pip3 install", "npm install", "pnpm install", "yarn add", "go install",
        "curl -o", "curl -o-", "wget -o", "scp ", "rsync ", "sftp ",
        "tar -x", "tar -zxf", "tar -xjf", "unzip ", "zip ", "gzip ", "bzip2 ", "xz ",
        "git add", "git commit", "git push", "git reset", "git checkout", "git merge",
        "git rebase", "git clean", "git rm", "git mv", "git stash",
        "echo >", "printf >", "cat >", "tee >", "dd if=",
        "docker ", "podman ", "kubectl ", "helm ",
    ];
    WRITE_CMDS.iter().any(|w| c.contains(w))
}

/// 巡检动态命令白名单校验：只允许白名单内的只读命令，拒绝管道/重定向/命令替换/写操作。
pub(crate) fn validate_readonly_command(command: &str) -> Result<(), String> {
    let c = command.trim();
    if c.is_empty() || c.chars().count() > 500 {
        return Err("命令为空或过长".to_string());
    }
    const FORBIDDEN: &[&str] = &[
        ";", "&&", "||", "|", ">", ">>", "<", "<<", "$(", "`", "\n", "\r", "rm ", "mv ",
        "cp ", "touch ", "mkdir ", "chmod", "chown", "systemctl start", "systemctl stop",
        "systemctl restart", "systemctl enable", "systemctl disable", "shutdown", "reboot",
        "kill", "dd ", "iptables", "ufw ", "firewall-cmd --add", "mount ", "umount",
    ];
    for token in FORBIDDEN {
        if c.contains(token) {
            return Err(format!("包含不允许的操作或字符: {token}"));
        }
    }
    let first = c
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    const ALLOWED: &[&str] = &[
        "cat", "grep", "head", "tail", "awk", "sed", "find", "ls", "ps", "df", "du", "free",
        "uptime", "uname", "hostname", "ss", "netstat", "systemctl", "journalctl", "docker",
        "podman", "sshd", "ufw", "firewall-cmd", "fail2ban-client", "last", "lastb", "who",
        "w", "id", "getent", "stat", "lsof", "sysctl", "hostnamectl", "timedatectl", "rpm",
        "dpkg", "apt", "yum", "dnf", "zypper", "pacman", "brew", "locale", "env",
    ];
    if !ALLOWED.contains(&first) {
        return Err(format!("命令不在只读白名单内: {first}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_redacts_secrets() {
        assert_eq!(sanitize("password=secret123"), "password=***");
        assert_eq!(sanitize("sk-abcdefghijklmnopqrst"), "***");
        assert_eq!(sanitize("AKIAABCDEFGHIJKLMNOP"), "***");
    }

    #[test]
    fn sanitize_redacts_pem_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        assert_eq!(sanitize(pem), "***");
    }

    #[test]
    fn sanitize_keeps_benign_text() {
        let text = "total 42\n-rw-r--r-- 1 root root 0 Jan 1 file.txt";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn detects_dangerous_commands() {
        assert!(is_dangerous("rm -rf /"));
        assert!(is_dangerous("sudo systemctl restart nginx"));
        assert!(!is_dangerous("ls -la /var/log"));
        assert!(!is_dangerous("cat /etc/hosts"));
    }

    #[test]
    fn detects_write_operations() {
        assert!(!is_write_operation("cat /etc/hosts"));
        assert!(!is_write_operation("stat /var/log/syslog"));
        assert!(!is_write_operation("ls -la"));
        assert!(is_write_operation("rm /tmp/x"));
        assert!(is_write_operation("echo hi > /tmp/a"));
        assert!(is_write_operation("curl -o /tmp/f http://x"));
    }

    #[test]
    fn validates_readonly_commands() {
        assert!(validate_readonly_command("df -h").is_ok());
        assert!(validate_readonly_command("cat /etc/passwd").is_ok());
        assert!(validate_readonly_command("rm -rf /").is_err());
        assert!(validate_readonly_command("cat a | grep b").is_err());
        assert!(validate_readonly_command("$(whoami)").is_err());
    }

    #[test]
    fn normalizes_tool_aliases() {
        assert_eq!(normalize_tool("ls"), "list_dir");
        assert_eq!(normalize_tool("exec"), "exec_command");
        assert_eq!(normalize_tool("exec_command"), "exec_command");
        assert_eq!(normalize_tool("unknown_tool"), "unknown_tool");
    }
}
