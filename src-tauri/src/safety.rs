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
                // 不设最短长度：短口令同样是秘密；宁可多打码
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*)[^\s"',;]+"#,
                )
                .unwrap(),
                template: "${1}${2}***",
            },
            // CLI 常见写法：mysql -pSecret / ssh -p22 / --password Secret
            Rule {
                re: Regex::new(r#"(?i)(\s-p)([^\s"',;]{2,})"#).unwrap(),
                template: "${1}***",
            },
            Rule {
                re: Regex::new(r#"(?i)(--password)(\s+|=)([^\s"',;]+)"#).unwrap(),
                template: "${1}${2}***",
            },
            // URL 中携带的用户名/口令：postgres://user:pass@host
            Rule {
                re: Regex::new(r#"(?i)([a-z][a-z0-9+.-]*://)([^/\s:@]+):([^@\s/]+)@"#).unwrap(),
                template: "${1}***:***@",
            },
            // curl -u user:password
            Rule {
                re: Regex::new(r#"(?i)(\s-u\s+)([^:\s]+):([^\s]+)"#).unwrap(),
                template: "${1}***:***",
            },
            // AWS Access Key
            Rule {
                re: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                template: "***",
            },
            // OpenAI / DeepSeek / Stripe 风格 sk- 密钥（大小写不敏感）
            Rule {
                re: Regex::new(r"(?i)sk-[A-Za-z0-9_\-]{16,}").unwrap(),
                template: "***",
            },
            Rule {
                re: Regex::new(r"(?i)sk_(live|test)_[A-Za-z0-9]{16,}").unwrap(),
                template: "***",
            },
            // GitHub / Slack / Google 常见 token
            Rule {
                re: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap(),
                template: "***",
            },
            Rule {
                re: Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap(),
                template: "***",
            },
            Rule {
                re: Regex::new(r"\bAIza[0-9A-Za-z_\-]{20,}\b").unwrap(),
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
/// `is_dangerous` 从这张表派生；`is_write_operation` 已改为默认拒绝的安全白名单判定，
/// 不再依赖本表是否收录某个命令。这样 `awk`、`sed`、`r''m` 之类的绕过不会因漏配而自动执行。
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
    // 这些命令同样具有破坏性，但以前没有收录；新增后至少能在审计/审批原因
    // 与修复步骤风险标记里显示为 Dangerous。
    ("unlink ", RiskTier::Dangerous),
    ("shred", RiskTier::Dangerous),
    ("truncate -s", RiskTier::Dangerous),
    ("wipefs", RiskTier::Dangerous),
    ("systemd-run", RiskTier::Dangerous),
    ("nsenter", RiskTier::Dangerous),
    ("chroot", RiskTier::Dangerous),

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

/// 检查引号外的 shell 元字符。单引号内一切都是字面量；双引号内仍拒绝
/// `$`、反引号和反斜杠（它们在双引号里仍会展开）。这样 `grep -E 'a|b'`
/// 这类常见只读命令可以自动执行，而 `cat a | sh` 仍会被拒绝。
fn has_unsafe_shell_syntax(command: &str) -> bool {
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if q == '"' && matches!(ch, '$' | '`' | '\\') {
                    return true;
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                ';' | '|' | '&' | '$' | '`' | '>' | '<' | '(' | ')' | '{' | '}' | '\\'
                | '*' | '?' | '[' | ']' | '\n' | '\r' => return true,
                _ => {}
            },
        }
    }
    quote.is_some()
}

/// 取第一个不以 `-` 开头的参数（用于工具子命令判断）。
fn first_non_flag(tokens: &[String]) -> Option<&str> {
    tokens
        .iter()
        .map(String::as_str)
        .find(|token| !token.starts_with('-'))
}

/// 判断命令是否可以 **自动执行** 的只读命令。
///
/// 这里采用默认拒绝：只有首词在安全白名单、子命令/参数满足约束，且引号外
/// 不含 shell 元字符时才返回 true。任何无法确定语义的写法都返回 false，
/// 由审批层交给用户确认。
///
/// 这是安全边界，不是“猜写操作”的启发式：`r''m -rf /`、`bash -c 'unlink x'`、
/// `awk 'BEGIN{system(...)}'`、`sed 'w file'` 等都不会被自动放行。
pub(crate) fn is_safe_readonly_command(command: &str) -> bool {
    let c = command.trim();
    if c.is_empty() || c.len() > 16_384 {
        return false;
    }
    if has_unsafe_shell_syntax(c) || c.contains("..") {
        return false;
    }
    let tokens: Vec<&str> = c.split_whitespace().collect();
    let Some(first_raw) = tokens.first().copied() else {
        return false;
    };
    // 首词必须是裸命令名：带路径、引号或转义都可能改变实际执行的程序。
    if first_raw.contains('/') || first_raw.contains('"') || first_raw.contains('\'') {
        return false;
    }
    let first = first_raw.to_ascii_lowercase();
    let lower: Vec<String> = tokens.iter().map(|token| token.to_ascii_lowercase()).collect();

    match first.as_str() {
        // 纯读取 / 输出过滤类命令
        "cat" | "head" | "tail" | "grep" | "egrep" | "fgrep" | "ls" | "stat" | "df"
        | "du" | "free" | "uptime" | "uname" | "id" | "whoami" | "pwd" | "locale"
        | "last" | "lastb" | "who" | "w" | "getent" | "ps" | "netstat" | "lsof"
        | "echo" | "printf" | "true" | "false" | "wc" | "cut" | "tr" | "nl"
        | "column" | "sha256sum" | "sha1sum" | "md5sum" => true,

        // 有写模式的读取工具：只放行明确的只读形式
        "hostname" => lower.iter().skip(1).all(|token| {
            matches!(
                token.as_str(),
                "-f"
                    | "-s"
                    | "-d"
                    | "-i"
                    | "--fqdn"
                    | "--short"
                    | "--domain"
                    | "--ip-address"
                    | "--all-ip-addresses"
                    | "--version"
            )
        }),
        "date" => !lower.iter().skip(1).any(|token| {
            token.starts_with("-s")
                || token.starts_with("--set")
                || (!token.starts_with('-') && !token.starts_with('+'))
        }),
        "sysctl" => {
            !lower.iter().any(|token| {
                token.starts_with("-w")
                    || token.starts_with("--write")
                    || token.starts_with("-p")
                    || token.starts_with("--load")
                    || token.starts_with("--system")
                    || token.contains('=')
            })
        }

        "ss" => !lower
            .iter()
            .any(|token| token.starts_with("-k") || token.starts_with("--kill")),

        "journalctl" => !lower.iter().any(|token| {
            token.starts_with("--vacuum")
                || token.starts_with("--rotate")
                || token.starts_with("--flush")
                || token.starts_with("--sync")
                || token.starts_with("--setup-keys")
                || token.starts_with("--update-catalog")
                || token.starts_with("--relinquish-var")
                || token.starts_with("--smart-relinquish-var")
        }),

        "systemctl" => first_non_flag(&lower[1..]).is_some_and(|command| {
            matches!(
                command,
                "status"
                    | "show"
                    | "list-units"
                    | "list-unit-files"
                    | "list-timers"
                    | "list-sockets"
                    | "list-dependencies"
                    | "get-default"
                    | "is-active"
                    | "is-enabled"
                    | "is-failed"
                    | "cat"
                    | "help"
            )
        }),

        "docker" | "podman" => match first_non_flag(&lower[1..]) {
            Some("system") => first_non_flag(&lower[2..]).is_some_and(|command| command == "df"),
            Some(command) => matches!(
                command,
                "ps"
                    | "inspect"
                    | "logs"
                    | "stats"
                    | "images"
                    | "version"
                    | "info"
                    | "top"
                    | "port"
                    | "diff"
                    | "history"
                    | "events"
                    | "search"
            ),
            None => false,
        },

        "apt" => first_non_flag(&lower[1..]).is_some_and(|command| command == "list"),
        "yum" | "dnf" => first_non_flag(&lower[1..]).is_some_and(|command| {
            matches!(
                command,
                "list" | "info" | "search" | "check-update" | "repolist" | "deplist"
            )
        }),
        "zypper" => first_non_flag(&lower[1..]).is_some_and(|command| {
            matches!(
                command,
                "search" | "info" | "list-updates" | "repos" | "packages" | "patterns"
            )
        }),
        "brew" => first_non_flag(&lower[1..]).is_some_and(|command| {
            matches!(command, "list" | "info" | "search" | "outdated")
        }),

        "rpm" => {
            let write = lower.iter().any(|token| {
                token.starts_with("-i")
                    || token.starts_with("-e")
                    || token.starts_with("-u")
                    || token.starts_with("-f")
                    || token == "--install"
                    || token == "--erase"
                    || token == "--upgrade"
                    || token == "--freshen"
            });
            let query = lower.iter().any(|token| {
                token.starts_with("-q") || token == "--query"
            });
            query && !write
        }

        "dpkg" => {
            let write = lower.iter().any(|token| {
                matches!(
                    token.as_str(),
                    "-i"
                        | "-r"
                        | "-p"
                        | "--install"
                        | "--remove"
                        | "--purge"
                        | "--configure"
                        | "--unpack"
                )
            });
            let query = lower.iter().any(|token| {
                matches!(
                    token.as_str(),
                    "-l" | "-L" | "-s" | "-S" | "--list" | "--status" | "--search" | "--print-avail"
                )
            });
            query && !write
        }

        "sshd" => lower.iter().any(|token| token == "-t"),
        "ufw" => first_non_flag(&lower[1..]).is_some_and(|command| matches!(command, "status" | "show")),
        "firewall-cmd" => {
            let write = lower.iter().any(|token| {
                token.starts_with("--add")
                    || token.starts_with("--remove")
                    || token.starts_with("--delete")
                    || token.starts_with("--set")
                    || token == "--reload"
                    || token == "--complete-reload"
                    || token == "--panic-on"
                    || token == "--panic-off"
            });
            let read = lower.iter().any(|token| {
                matches!(
                    token.as_str(),
                    "--state"
                        | "--list-all"
                        | "--list-services"
                        | "--list-ports"
                        | "--list-protocols"
                        | "--get-zones"
                        | "--get-default-zone"
                        | "--get-active-zones"
                        | "--info-zone"
                        | "--info-service"
                ) || token.starts_with("--query-")
            });
            read && !write
        }
        "fail2ban-client" => {
            first_non_flag(&lower[1..]).is_some_and(|command| command == "status")
        }

        "hostnamectl" => first_non_flag(&lower[1..]).is_none_or(|command| {
            matches!(command, "status" | "show")
        }),
        "timedatectl" => first_non_flag(&lower[1..]).is_none_or(|command| {
            matches!(
                command,
                "status" | "show" | "list-timezones" | "timesync-status" | "show-timesync"
            )
        }),
        "ip" => {
            const WRITE: &[&str] = &[
                "add", "del", "delete", "set", "flush", "change", "replace", "up", "down",
                "save", "restore", "append", "prepend", "update", "exec", "attach",
            ];
            const READ: &[&str] = &[
                "addr", "a", "route", "r", "link", "l", "neigh", "n", "maddr", "mroute",
                "rule", "tunnel", "tuntap", "netns",
            ];
            // `ip netns exec <ns> <command>` 会执行任意命令，必须按写操作处理。
            !lower.iter().any(|token| WRITE.contains(&token.as_str()))
                && lower.iter().any(|token| READ.contains(&token.as_str()))
        }

        // 解释器、脚本执行器、编辑器、网络传输、包安装等一律不自动执行
        _ => false,
    }
}

/// 默认拒绝的写操作判定：只要不能证明是安全只读命令，就视为需要审批。
/// 名称保留是为了兼容现有调用方；语义已从“命中危险词表”升级为“非安全白名单”。
pub(crate) fn is_write_operation(command: &str) -> bool {
    !is_safe_readonly_command(command)
}

/// 敏感路径判定：命中时读取/执行需要用户确认（SFTP 编辑器与 AI 工具共用）。
pub(crate) fn is_sensitive_path(path: &str) -> bool {
    // 去掉 shell 引号后再匹配：/etc/sh''adow 与 /etc/shadow 是同一个目标。
    let raw = path
        .replace('\\', "/")
        .replace('\'', "")
        .replace('"', "")
        .to_ascii_lowercase();
    // 文本归一化 .. / .，避免 /etc/../etc/shadow 绕过标记匹配。
    // 这里只做路径语义归一化；符号链接目标由 SFTP 层另行解析。
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let p = format!("/{}", parts.join("/"));
    const MARKERS: &[&str] = &[
        "/.ssh/",
        ".ssh/",
        "/.aws/",
        ".aws/",
        "/.azure/",
        ".azure/",
        "/.gnupg/",
        ".gnupg/",
        "/.kube/",
        ".kube/",
        "/.docker/config.json",
        ".docker/config.json",
        "/.pgpass",
        ".pgpass",
        "/.my.cnf",
        ".my.cnf",
        "/.netrc",
        ".netrc",
        "/.git-credentials",
        ".git-credentials",
        "/.bash_history",
        ".bash_history",
        "/.zsh_history",
        ".zsh_history",
        "/.mysql_history",
        ".mysql_history",
        "/.psql_history",
        ".psql_history",
        "/etc/environment",
        "/proc/self/environ",
        "/proc/1/environ",
        "/.config/gcloud/",
        ".config/gcloud/",
        "/.config/gh/",
        ".config/gh/",
        "/.npmrc",
        ".npmrc",
        "/.pypirc",
        ".pypirc",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "/etc/shadow",
        "/etc/gshadow",
        "/etc/passwd",
        "/etc/group",
        "/etc/sudoers",
        "/etc/pam.d/",
        "/etc/ld.so.preload",
        "/etc/ssh/",
        "/etc/ssl/private/",
        "/etc/krb5.keytab",
        "/etc/crontab",
        "/etc/cron.",
        "/var/spool/cron",
        "/etc/systemd/",
        "/etc/init.d/",
        "/etc/rc.local",
        "/etc/profile.d/",
        "/.bashrc",
        ".bashrc",
        "/.bash_profile",
        ".bash_profile",
        "/.profile",
        ".profile",
        "/.zshrc",
        ".zshrc",
        "/.zprofile",
        ".zprofile",
        "/.config/fish/",
        ".config/fish/",
    ];
    MARKERS.iter().any(|marker| p.contains(marker))
}

/// 命令字符串里是否出现敏感路径（用于 AI/MCP 执行前加一道审批）。
pub(crate) fn command_has_sensitive_path(command: &str) -> bool {
    is_sensitive_path(command)
}

/// 巡检动态命令白名单校验：只允许可证明安全的只读命令。
/// 与 MCP 只读/Agent 智能审核共用同一个默认拒绝分类器，避免三处规则漂移。
pub(crate) fn validate_readonly_command(command: &str) -> Result<(), String> {
    let c = command.trim();
    if c.is_empty() || c.chars().count() > 500 {
        return Err(crate::i18n::t("命令为空或过长", "The command is empty or too long").to_string());
    }
    if !is_safe_readonly_command(c) {
        return Err(crate::i18n::t(
            "命令不在只读白名单内或包含不允许的 shell 语法",
            "The command is not in the read-only allowlist or contains disallowed shell syntax",
        )
        .to_string());
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
        assert_eq!(sanitize("password=short"), "password=***", "短口令也必须脱敏");
    }

    #[test]
    fn sanitize_redacts_cli_and_url_credentials() {
        assert_eq!(sanitize("mysql -uroot -pS3cret"), "mysql -uroot -p***");
        assert_eq!(sanitize("mysql --password S3cret"), "mysql --password ***");
        assert_eq!(
            sanitize("psql postgres://user:pass@db.local/app"),
            "psql postgres://***:***@db.local/app"
        );
        assert_eq!(sanitize("curl -u user:pass https://x"), "curl -u ***:*** https://x");
        assert_eq!(sanitize("token ghp_abcdefghijklmnopqrstuvwxyz012345"), "token ***");
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
        // 默认拒绝策略下，管道本身就是 shell 组合语法；即使目标是 shasum，
        // 也要求用户确认一次，而不是自动执行。
        assert!(is_write_operation("cat x | shasum"));
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

    #[test]
    fn rejects_shell_quote_and_interpreter_bypasses() {
        for cmd in [
            "r''m -rf /",
            "\"r\"m -rf /",
            "bash -c 'unlink /tmp/buffterm_bypass_check'",
            "sh -c 'shred -u /tmp/buffterm_bypass_check'",
            "awk 'BEGIN{system(\"unlink /tmp/buffterm_bypass_check\")}'",
            "sed -n 'w /tmp/buffterm_bypass_check' somefile",
            "unlink /tmp/buffterm_bypass_check",
            "truncate -s 0 /tmp/buffterm_bypass_check",
            "shred -u /tmp/buffterm_bypass_check",
            "cat /etc/sh*",
            "cat /etc/../etc/shadow",
            "ip netns exec prod sh -c 'unlink /tmp/x'",
        ] {
            assert!(!is_safe_readonly_command(cmd), "{cmd} 不应被当成只读");
            assert!(is_write_operation(cmd), "{cmd} 应要求审批");
            assert!(validate_readonly_command(cmd).is_err(), "{cmd} 应被巡检拒绝");
        }
    }

    #[test]
    fn safe_allowlist_keeps_common_readonly_queries() {
        for cmd in [
            "cat /etc/hosts",
            "grep -n error /var/log/syslog",
            "df -h",
            "ps aux",
            "systemctl status nginx",
            "docker ps",
            "apt list --upgradable",
            "ss -lntp",
            "ip addr show",
            "journalctl -n 50",
            "date +%s",
            "hostname -f",
            "grep -E 'error|warn' /var/log/syslog",
            "grep -E \"error|warn\" /var/log/syslog",
        ] {
            assert!(is_safe_readonly_command(cmd), "{cmd} 应视为安全只读");
            assert!(!is_write_operation(cmd), "{cmd} 不应要求审批");
        }
        for cmd in [
            "hostname new-name",
            "hostname --file=/etc/hostname",
            "date -s '2026-01-01 00:00:00'",
            "date --set='2026-01-01 00:00:00'",
            "sysctl -p /etc/sysctl.conf",
            "sysctl -p/etc/sysctl.conf",
            "brew install wget",
        ] {
            assert!(!is_safe_readonly_command(cmd), "{cmd} 有写入副作用");
        }
    }

    #[test]
    fn sensitive_paths_are_flagged() {
        for path in [
            "/root/.ssh/id_rsa",
            ".ssh/id_ed25519",
            "/etc/shadow",
            "/etc/sudoers",
            "/home/app/.kube/config",
            "/home/app/.pgpass",
        ] {
            assert!(is_sensitive_path(path), "{path} 应判定为敏感路径");
        }
        assert!(!is_sensitive_path("/var/log/syslog"));
        assert!(is_sensitive_path("/etc/../etc/shadow"), "路径穿越应归一化后命中");
        assert!(is_sensitive_path("/etc/sh''adow"), "引号拼接应归一化后命中");
        assert!(command_has_sensitive_path("cat /etc/shadow"));
        assert!(command_has_sensitive_path("cat /etc/sh''adow"));
        assert!(!command_has_sensitive_path("df -h"));
    }

    #[test]
    fn normalizes_tool_aliases() {
        assert_eq!(normalize_tool("ls"), "list_dir");
        assert_eq!(normalize_tool("exec"), "exec_command");
        assert_eq!(normalize_tool("exec_command"), "exec_command");
        assert_eq!(normalize_tool("unknown_tool"), "unknown_tool");
    }
}
