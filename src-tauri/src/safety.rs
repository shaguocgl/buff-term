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
        /// 替换模板：保留 key 前缀的规则用捕获组 ${1}/${2}（含引号，保证闭合）
        template: &'static str,
    }
    static RE: OnceLock<Vec<Rule>> = OnceLock::new();
    let res = RE.get_or_init(|| {
        vec![
            // 常见密钥/口令键值对：password=xxx / token: "xxx"（支持引号内带空格的值，
            // 替换时保留引号闭合，避免输出残留未闭合引号；regex crate 不支持
            // 反向引用，单/双引号分别匹配）
            Rule {
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*")([^"]{1,})""#,
                )
                .unwrap(),
                template: "${1}${2}***\"",
            },
            Rule {
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*')([^']{1,})'"#,
                )
                .unwrap(),
                template: "${1}${2}***'",
            },
            Rule {
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*)[^\s"',;]{6,}"#,
                )
                .unwrap(),
                template: "${1}${2}***",
            },
            // AWS Access Key
            Rule {
                re: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                template: "***",
            },
            // OpenAI / DeepSeek 风格 sk- 密钥（大小写不敏感）
            Rule {
                re: Regex::new(r"(?i)sk-[A-Za-z0-9_\-]{16,}").unwrap(),
                template: "***",
            },
            // PEM 私钥块
            Rule {
                re: Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                template: "***",
            },
            // Bearer / Basic 认证头
            Rule {
                re: Regex::new(r"(?i)authorization:\s*(basic|bearer)\s+[^\r\n]+").unwrap(),
                template: "***",
            },
            Rule {
                re: Regex::new(r#"(?i)\bbearer\s+["']?[A-Za-z0-9._\-]{20,}["']?"#).unwrap(),
                template: "***",
            },
            // 常见云厂商密钥片段
            Rule {
                re: Regex::new(
                    r#"(?i)(access[_-]?key[_-]?id|secret[_-]?access[_-]?key)(\s*[:=]\s*["']?)[^\s"',;]{10,}"#,
                )
                .unwrap(),
                template: "${1}${2}***",
            },
        ]
    });
    let mut out = text.to_string();
    for rule in res {
        out = rule.re.replace_all(&out, rule.template).to_string();
    }
    out
}

/// 命令风险分级：Write（修改状态，通常可恢复）< Dangerous（破坏性 / 不可逆 / 影响服务可用性）。
/// `is_dangerous` 与 `is_write_operation` 全部从这张表派生判断，避免同一动作在多处分别
/// 维护子串列表导致行为漂移（例如 `kill -9` 在一处是 Dangerous，在另一处却漏判）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum RiskTier {
    Write,
    Dangerous,
}

/// 唯一的风险规则源：新增/调整危险或写操作命令只改这一处。
/// 注意：docker/podman/systemctl/apt 等在这里是“整体阻止”粒度（服务于 MCP 只读模式，
/// 该模式下 exec_command 是任意 shell，必须整体拦截），巡检用的 `validate_readonly_command`
/// 需要更细粒度地放行这些工具的只读子命令，见下方 `READONLY_EXTRA_FORBIDDEN`。
const COMMAND_RULES: &[(&str, RiskTier)] = &[
    // ---- Dangerous：破坏性 / 不可逆 / 影响服务可用性 ----
    ("rm -rf", RiskTier::Dangerous),
    ("rm -fr", RiskTier::Dangerous),
    ("rm -r", RiskTier::Dangerous),
    ("mkfs", RiskTier::Dangerous),
    ("dd if=", RiskTier::Dangerous),
    (">/dev/sd", RiskTier::Dangerous),
    ("iptables", RiskTier::Dangerous),
    ("systemctl stop", RiskTier::Dangerous),
    ("systemctl restart", RiskTier::Dangerous),
    ("systemctl disable", RiskTier::Dangerous),
    ("systemctl mask", RiskTier::Dangerous),
    ("shutdown", RiskTier::Dangerous),
    ("reboot", RiskTier::Dangerous),
    ("poweroff", RiskTier::Dangerous),
    ("chmod -r", RiskTier::Dangerous),
    ("chown -r", RiskTier::Dangerous),
    ("fdisk", RiskTier::Dangerous),
    ("parted", RiskTier::Dangerous),
    ("pvremove", RiskTier::Dangerous),
    ("vgremove", RiskTier::Dangerous),
    ("lvremove", RiskTier::Dangerous),
    ("userdel", RiskTier::Dangerous),
    ("groupdel", RiskTier::Dangerous),
    ("drop database", RiskTier::Dangerous),
    ("truncate table", RiskTier::Dangerous),
    ("delete from", RiskTier::Dangerous),
    ("kill -9", RiskTier::Dangerous),

    // ---- Write：修改状态但通常可恢复 ----
    ("touch ", RiskTier::Write),
    ("mkdir ", RiskTier::Write),
    ("rmdir ", RiskTier::Write),
    ("rm ", RiskTier::Write),
    ("mv ", RiskTier::Write),
    ("cp ", RiskTier::Write),
    ("ln ", RiskTier::Write),
    ("tee ", RiskTier::Write),
    ("chmod", RiskTier::Write),
    ("chown", RiskTier::Write),
    ("chattr", RiskTier::Write),
    ("chgrp", RiskTier::Write),
    ("dd ", RiskTier::Write),
    ("mount ", RiskTier::Write),
    ("umount", RiskTier::Write),
    ("swapoff", RiskTier::Write),
    ("swapon", RiskTier::Write),
    ("useradd", RiskTier::Write),
    ("usermod", RiskTier::Write),
    ("groupadd", RiskTier::Write),
    ("passwd ", RiskTier::Write),
    ("chage", RiskTier::Write),
    ("systemctl", RiskTier::Write),
    ("service ", RiskTier::Write),
    ("kill ", RiskTier::Write),
    ("pkill", RiskTier::Write),
    ("killall", RiskTier::Write),
    ("nohup ", RiskTier::Write),
    ("halt ", RiskTier::Write),
    ("init ", RiskTier::Write),
    ("install ", RiskTier::Write),
    ("sed -i", RiskTier::Write),
    ("perl -i", RiskTier::Write),
    ("awk -i", RiskTier::Write),
    // 解释器直接执行代码（绕过子串检测的写操作入口）
    ("python3 -c", RiskTier::Write),
    ("python -c", RiskTier::Write),
    ("python2 -c", RiskTier::Write),
    ("perl -e", RiskTier::Write),
    ("ruby -e", RiskTier::Write),
    ("node -e", RiskTier::Write),
    ("php -r", RiskTier::Write),
    // find 的破坏性动作
    (" -delete", RiskTier::Write),
    (" -exec", RiskTier::Write),
    ("vim ", RiskTier::Write),
    ("vi ", RiskTier::Write),
    ("nano ", RiskTier::Write),
    ("ed ", RiskTier::Write),
    ("crontab", RiskTier::Write),
    ("batch ", RiskTier::Write),
    ("apt ", RiskTier::Write),
    ("apt-get", RiskTier::Write),
    ("yum ", RiskTier::Write),
    ("dnf ", RiskTier::Write),
    ("brew ", RiskTier::Write),
    ("zypper", RiskTier::Write),
    ("pacman ", RiskTier::Write),
    ("pip install", RiskTier::Write),
    ("pip3 install", RiskTier::Write),
    ("npm install", RiskTier::Write),
    ("pnpm install", RiskTier::Write),
    ("yarn add", RiskTier::Write),
    ("go install", RiskTier::Write),
    ("curl -o", RiskTier::Write),
    ("curl -o-", RiskTier::Write),
    // 短选项组合（-Lo / -sSo）会破坏 "-o " 的连续子串，按 "-l" 兜底拦截
    ("curl -l", RiskTier::Write),
    ("wget -o", RiskTier::Write),
    ("wget -q", RiskTier::Write),
    ("scp ", RiskTier::Write),
    ("rsync ", RiskTier::Write),
    ("sftp ", RiskTier::Write),
    ("tar -x", RiskTier::Write),
    ("tar -zxf", RiskTier::Write),
    ("tar -xjf", RiskTier::Write),
    ("unzip ", RiskTier::Write),
    ("zip ", RiskTier::Write),
    ("gzip ", RiskTier::Write),
    ("bzip2 ", RiskTier::Write),
    ("xz ", RiskTier::Write),
    ("git add", RiskTier::Write),
    ("git commit", RiskTier::Write),
    ("git push", RiskTier::Write),
    ("git reset", RiskTier::Write),
    ("git checkout", RiskTier::Write),
    ("git merge", RiskTier::Write),
    ("git rebase", RiskTier::Write),
    ("git clean", RiskTier::Write),
    ("git rm", RiskTier::Write),
    ("git mv", RiskTier::Write),
    ("git stash", RiskTier::Write),
    ("echo >", RiskTier::Write),
    ("printf >", RiskTier::Write),
    ("cat >", RiskTier::Write),
    ("tee >", RiskTier::Write),
    ("ufw ", RiskTier::Write),
    ("firewall-cmd --add", RiskTier::Write),
    ("docker ", RiskTier::Write),
    ("podman ", RiskTier::Write),
    ("kubectl ", RiskTier::Write),
    ("helm ", RiskTier::Write),
];

fn max_tier(command: &str) -> Option<RiskTier> {
    let c = command.to_ascii_lowercase();
    COMMAND_RULES
        .iter()
        .filter(|(pat, _)| c.contains(pat))
        .map(|(_, tier)| *tier)
        .max()
}

/// 智能审核模式下判断命令是否有风险（内置危险模式，子串匹配）。
pub(crate) fn is_dangerous(command: &str) -> bool {
    max_tier(command) == Some(RiskTier::Dangerous)
}

/// 判断重定向是否构成真实写操作（排除 /dev/null 丢弃、fd 复制等无害写法）。
fn has_unsafe_redirect(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    let cleaned = c
        .replace("2>/dev/null", "")
        .replace(">/dev/null", "")
        .replace("1>/dev/null", "")
        .replace("2>&1", "")
        .replace(">&2", "")
        .replace(">&1", "")
        .replace("2>&-", "");
    cleaned.contains('>')
}

/// 管道是否交给 shell 解释器执行（下载即执行 / base64 解码后执行等编码绕过）。
/// 词边界判断避免误伤 `cat x | shasum` 这类名字以 sh 开头的普通工具。
fn has_pipe_to_shell(command: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"\|\s*(sh|bash|zsh|dash)(\s|;|$)"#).unwrap()
    });
    re.is_match(&command.to_ascii_lowercase())
}

/// 只读模式下判断命令是否包含写操作（修改 / 删除 / 安装 / 网络传输 / 重定向写文件等）。
/// 用于 MCP 只读权限模式：该场景 exec_command 是任意 shell，docker/systemctl/apt 等
/// 一律整体拦截（宁可多拦），因此直接复用统一规则表的整体匹配粒度。
pub(crate) fn is_write_operation(command: &str) -> bool {
    if max_tier(command).is_some() {
        return true;
    }
    has_unsafe_redirect(command) || has_pipe_to_shell(command)
}

/// 巡检白名单场景下，`ALLOWED` 里部分工具（docker/systemctl/apt/ufw/firewall-cmd 等）本意
/// 只放行其只读查询子命令（如 `docker ps`、`systemctl status`），但 `is_write_operation`
/// 对这些工具是整体拦截粒度，不能直接复用。这里单独列出这些工具中真正有破坏性/绕过只读
/// 意图的子命令，做精确拦截，而不是拦截整个工具。
///
/// 这里同时修复了旧版 `validate_readonly_command` 的一个安全缺口：旧版只按“首个词是否在白名单”
/// 做粗粒度判断，`docker exec`、`apt install` 等子命令未被拦截，AI 可借助已放行的工具名逃出
/// “只读检查”的意图（例如 `docker exec -it web bash` 或 `apt install netcat`）。
const READONLY_EXTRA_FORBIDDEN: &[&str] = &[
    "docker exec", "docker run", "docker rm", "docker rmi", "docker stop", "docker kill",
    "docker restart", "docker build", "docker push", "docker pause", "docker unpause",
    "docker compose", "docker-compose",
    "podman exec", "podman run", "podman rm", "podman rmi", "podman stop", "podman kill",
    "podman restart",
    "apt install", "apt remove", "apt purge", "apt update", "apt upgrade", "apt autoremove",
    "apt-get install", "apt-get remove", "apt-get purge", "apt-get update", "apt-get upgrade",
    "apt-get autoremove",
    "yum install", "yum remove", "yum erase", "yum update", "yum upgrade",
    "dnf install", "dnf remove", "dnf update", "dnf upgrade",
    "zypper install", "zypper remove", "zypper update",
    "pacman -s", "pacman -r", "pacman -u",
    "dpkg -i", "dpkg -r", "dpkg --purge",
    "rpm -i", "rpm -e", "rpm -u",
    "systemctl start", "systemctl stop", "systemctl restart", "systemctl enable",
    "systemctl disable", "systemctl mask", "systemctl unmask", "systemctl reload",
    "systemctl try-restart", "systemctl daemon-reload", "systemctl kill",
    "systemctl set-default", "systemctl rescue", "systemctl emergency",
    "systemctl halt", "systemctl reboot", "systemctl poweroff", "systemctl isolate",
    "ufw allow", "ufw deny", "ufw reject", "ufw limit", "ufw insert", "ufw delete",
    "ufw enable", "ufw disable", "ufw reset",
    "firewall-cmd --add", "firewall-cmd --remove", "firewall-cmd --reload",
    // 解释器直接执行代码 / 编辑类写操作（env 已移出白名单，这里兜底解释器入口）
    "python3 -c", "python -c", "python2 -c", "perl -e", "ruby -e", "node -e", "php -r",
    "sed -i", "perl -i", "awk -i",
    // find 的破坏性动作
    " -delete", " -exec",
];

/// 巡检动态命令白名单校验：只允许白名单内的只读命令，拒绝管道/重定向/命令替换/写操作。
pub(crate) fn validate_readonly_command(command: &str) -> Result<(), String> {
    let c = command.trim();
    if c.is_empty() || c.chars().count() > 500 {
        return Err("命令为空或过长".to_string());
    }
    const META_CHARS: &[&str] = &[";", "&&", "||", "|", ">", ">>", "<", "<<", "$(", "`", "\n", "\r"];
    for token in META_CHARS {
        if c.contains(token) {
            return Err(format!("包含不允许的字符: {token}"));
        }
    }
    let lower = c.to_ascii_lowercase();
    if is_dangerous(&lower) {
        return Err("命令属于高危操作，只读巡检不允许执行".to_string());
    }
    // 基础写操作动词：不依赖工具白名单，任何情况下都拦截
    const BASIC_WRITE: &[&str] = &[
        "rm ", "mv ", "cp ", "touch ", "mkdir ", "chmod", "chown", "mount ", "umount",
    ];
    for token in BASIC_WRITE {
        if lower.contains(token) {
            return Err(format!("包含不允许的操作: {token}"));
        }
    }
    for token in READONLY_EXTRA_FORBIDDEN {
        if lower.contains(token) {
            return Err(format!("包含不允许的操作: {token}"));
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
        "dpkg", "apt", "yum", "dnf", "zypper", "pacman", "brew", "locale",
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

    /// 引号包裹、大小写变体、带空格值的脱敏回归测试
    #[test]
    fn sanitize_handles_quoted_and_case_variants() {
        assert_eq!(sanitize(r#"password="My Secret Value""#), r#"password="***""#);
        assert_eq!(sanitize("token = 'abc123def456'"), "token = '***'");
        assert_eq!(sanitize("Sk-abcdefghijklmnopqrst"), "***");
        assert_eq!(sanitize(r#"Authorization: Bearer "eyJhbGciOiJIUzI1NiJ9""#), "***");
        assert_eq!(sanitize("password=short"), "password=short", "过短值不匹配（避免误伤）");
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

    /// 编码/解释器/短选项组合绕过静态子串检测的回归测试
    #[test]
    fn detects_encoded_and_interpreter_writes() {
        assert!(is_write_operation("echo 'cm0gLXJmIC8K' | base64 -d | sh"));
        assert!(is_write_operation("echo hi | bash"));
        assert!(is_write_operation("cat x | zsh"));
        assert!(!is_write_operation("cat x | shasum"), "| shasum 不应误判为管道 shell");
        assert!(is_write_operation("python3 -c \"open('/tmp/x','w').write('x')\""));
        assert!(is_write_operation("perl -e 'print 1'"));
        assert!(is_write_operation("curl -Lo /tmp/malware http://x"));
        assert!(is_write_operation("wget -qO /tmp/malware http://x"));
        assert!(is_write_operation("find /tmp -delete"));
        assert!(is_write_operation("find / -name x -exec rm {} \\;"));
    }

    #[test]
    fn validates_readonly_commands() {
        assert!(validate_readonly_command("df -h").is_ok());
        assert!(validate_readonly_command("cat /etc/passwd").is_ok());
        assert!(validate_readonly_command("rm -rf /").is_err());
        assert!(validate_readonly_command("cat a | grep b").is_err());
        assert!(validate_readonly_command("$(whoami)").is_err());
    }

    /// 只读白名单里的工具（docker/systemctl/ufw/apt 等）本意是放行只读查询子命令，
    /// 回归测试确保重构后这些查询仍然放行。
    #[test]
    fn validates_readonly_commands_allows_readonly_subcommands() {
        assert!(validate_readonly_command("docker ps").is_ok());
        assert!(validate_readonly_command("podman ps").is_ok());
        assert!(validate_readonly_command("systemctl status nginx").is_ok());
        assert!(validate_readonly_command("systemctl list-units --type=service").is_ok());
        assert!(validate_readonly_command("ufw status verbose").is_ok());
        assert!(validate_readonly_command("firewall-cmd --state").is_ok());
        assert!(validate_readonly_command("apt list --upgradable").is_ok());
        // 旧版会把 "kill" 当裸子串拦截，误伤日志内容里含 "kill" 的只读查询
        assert!(validate_readonly_command("grep kill /var/log/syslog").is_ok());
    }

    /// 修复点：旧版 validate_readonly_command 只检查首个词是否在白名单，
    /// docker exec / apt install 等借助已放行工具名的破坏性子命令未被拦截。
    #[test]
    fn validates_readonly_commands_blocks_escape_subcommands() {
        assert!(validate_readonly_command("docker exec -it web bash").is_err());
        assert!(validate_readonly_command("docker run --rm -v /:/host alpine").is_err());
        assert!(validate_readonly_command("docker compose up -d").is_err());
        assert!(validate_readonly_command("apt install netcat").is_err());
        assert!(validate_readonly_command("apt update").is_err());
        assert!(validate_readonly_command("systemctl start nginx").is_err());
        assert!(validate_readonly_command("systemctl stop nginx").is_err());
        assert!(validate_readonly_command("systemctl enable nginx").is_err());
        assert!(validate_readonly_command("systemctl try-restart nginx").is_err());
        assert!(validate_readonly_command("ufw allow 22").is_err());
        assert!(validate_readonly_command("firewall-cmd --add-port=80/tcp").is_err());
        assert!(validate_readonly_command("env python3 -c 'open(\"/x\",\"w\")'").is_err());
        assert!(validate_readonly_command("python3 -c 'open(\"/x\",\"w\")'").is_err());
        assert!(validate_readonly_command("sed -i 's/a/b/' /etc/hosts").is_err());
        assert!(validate_readonly_command("find /tmp -delete").is_err());
        assert!(validate_readonly_command("find / -exec rm {} \\;").is_err());
    }

    /// is_dangerous / is_write_operation 统一从同一张风险表派生，验证 Dangerous 蕴含 Write。
    #[test]
    fn dangerous_implies_write_operation() {
        for cmd in ["rm -rf /", "mkfs /dev/sda1", "shutdown -h now", "iptables -F"] {
            assert!(is_dangerous(cmd), "{cmd} 应判定为 dangerous");
            assert!(is_write_operation(cmd), "{cmd} 应判定为 write_operation");
        }
    }

    #[test]
    fn normalizes_tool_aliases() {
        assert_eq!(normalize_tool("ls"), "list_dir");
        assert_eq!(normalize_tool("exec"), "exec_command");
        assert_eq!(normalize_tool("exec_command"), "exec_command");
        assert_eq!(normalize_tool("unknown_tool"), "unknown_tool");
    }
}
