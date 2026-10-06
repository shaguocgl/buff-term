use std::path::Path;

use crate::hosts::HostInput;
use crate::models::AuthType;

/// 解析结果：可导入的主机 + 被忽略的规则块数量。
pub struct ParsedConfig {
    pub hosts: Vec<HostInput>,
    /// 被忽略的规则块数（通配 / 空 Host 行），用于向前端解释"为什么少导了几台"
    pub ignored: usize,
}

/// 解析 ~/.ssh/config 的常用字段。
/// 缺失 HostName / User 时按 ssh 的隐式语义补齐（别名当主机名、本机用户名登录），
/// 避免导入出地址或用户名为空、连接必然失败的主机。
pub fn parse(content: &str) -> ParsedConfig {
    parse_with_user(content, crate::util::local_username())
}

fn parse_with_user(content: &str, fallback_user: String) -> ParsedConfig {
    // 记事本等 Windows 编辑器会给文件加 UTF-8 BOM，BOM 不是空白字符，
    // trim() 去不掉：不剥掉会让首行变成 "\u{feff}host" 而静默漏掉第一台主机
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut out = ParsedConfig {
        hosts: Vec::new(),
        ignored: 0,
    };
    let mut current: Option<HostInput> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };

        match key.as_str() {
            "host" => {
                finish(&mut current, &mut out, &fallback_user);
                let first = value.split_whitespace().next().unwrap_or("");
                if first.is_empty() || first.contains('*') || first.contains('?') {
                    // 空 Host 行与通配规则块不导入
                    out.ignored += 1;
                } else {
                    current = Some(HostInput {
                        name: first.to_string(),
                        address: String::new(),
                        port: 22,
                        username: String::new(),
                        auth_type: AuthType::Key,
                        key_path: None,
                        notes: Some("来自 ~/.ssh/config".to_string()),
                    });
                }
            }
            // Match 是条件规则块，其字段不能并进上一个 Host，遇到即结算当前块
            "match" => finish(&mut current, &mut out, &fallback_user),
            "hostname" => {
                if let Some(h) = current.as_mut() {
                    h.address = value.to_string();
                }
            }
            "user" => {
                if let Some(h) = current.as_mut() {
                    h.username = value.to_string();
                }
            }
            "port" => {
                if let (Some(h), Ok(p)) = (current.as_mut(), value.parse::<u16>()) {
                    h.port = p;
                }
            }
            "identityfile" => {
                if let Some(h) = current.as_mut() {
                    // ssh 支持多行 IdentityFile 并按顺序尝试，本应用只保存一个：取第一个；
                    // 值可能带引号（路径含空格），需剥离
                    if h.key_path.is_none() {
                        h.key_path = Some(expand_tilde(unquote(value)));
                    }
                }
            }
            _ => {}
        }
    }

    finish(&mut current, &mut out, &fallback_user);
    out
}

/// 结算一个 Host 块：按 ssh 语义补齐缺失的隐式默认值后收集。
fn finish(current: &mut Option<HostInput>, out: &mut ParsedConfig, fallback_user: &str) {
    let Some(mut host) = current.take() else {
        return;
    };
    // 没有 HostName 时，ssh 直接把 Host 别名当主机名
    if host.address.is_empty() {
        host.address = host.name.clone();
    }
    // 没有 User 时，ssh 回退到本机用户名
    if host.username.is_empty() {
        host.username = fallback_user.to_string();
    }
    out.hosts.push(host);
}

/// 剥离配置值两端成对的引号（如 `IdentityFile "~/my keys/id_rsa"`）。
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn expand_tilde(path: &str) -> String {
    expand_tilde_with_home(path, crate::util::user_home_dir().as_deref())
}

fn expand_tilde_with_home(path: &str, home: Option<&Path>) -> String {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    match (rest, home) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().to_string(),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_blocks_and_skips_wildcards() {
        let content = r#"
Host myserver
    HostName 192.168.1.10
    User root
    Port 2222
    IdentityFile ~/.ssh/id_ed25519

Host wildcard*
    HostName example.com

Host another
    HostName another.example.com
"#;
        let parsed = parse(content);
        let hosts = parsed.hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(parsed.ignored, 1, "通配块应计入 ignored");

        let a = &hosts[0];
        assert_eq!(a.name, "myserver");
        assert_eq!(a.address, "192.168.1.10");
        assert_eq!(a.username, "root");
        assert_eq!(a.port, 2222);
        assert_eq!(a.auth_type, AuthType::Key);
        assert!(a.key_path.as_deref().is_some_and(|p| p.ends_with("id_ed25519")));

        let b = &hosts[1];
        assert_eq!(b.name, "another");
        assert_eq!(b.address, "another.example.com");
    }

    #[test]
    fn expand_tilde_supports_unix_and_windows_separators() {
        let home = Path::new("/home/u");
        assert_eq!(
            expand_tilde_with_home("~/.ssh/id_ed25519", Some(home)),
            home.join(".ssh/id_ed25519").to_string_lossy()
        );
        assert_eq!(
            expand_tilde_with_home("~\\.ssh\\id_ed25519", Some(home)),
            home.join(".ssh\\id_ed25519").to_string_lossy()
        );
        assert_eq!(
            expand_tilde_with_home("~/.ssh/id_ed25519", None),
            "~/.ssh/id_ed25519"
        );
        assert_eq!(
            expand_tilde_with_home("/etc/ssh/key", Some(home)),
            "/etc/ssh/key"
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let content = "# a comment\n\nHost simple\n    HostName 10.0.0.1\n";
        let hosts = parse(content).hosts;
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "simple");
        assert_eq!(hosts[0].address, "10.0.0.1");
    }

    /// 没有 HostName 时用 Host 别名当主机名（ssh 语义），不能留空地址。
    #[test]
    fn falls_back_to_alias_when_hostname_missing() {
        let content = "Host 10.0.0.1\n    User root\n\nHost db.example.com\n";
        let hosts = parse_with_user(content, "localuser".into()).hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].address, "10.0.0.1");
        assert_eq!(hosts[1].address, "db.example.com");
    }

    /// 没有 User 时回退到本机用户名（ssh 语义），不能留空用户名。
    #[test]
    fn falls_back_to_local_username_when_user_missing() {
        let content = "Host legacy\n    HostName 10.0.0.9\n";
        let hosts = parse_with_user(content, "localuser".into()).hosts;
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].username, "localuser");
    }

    /// Match 块是条件规则，不能把它的字段并进上一个 Host。
    #[test]
    fn match_block_does_not_leak_into_previous_host() {
        let content = "\
Host prod
    HostName 10.1.1.1
    User deploy

Match host *.example.com
    User shared
    Port 2200

Host after
    HostName 10.2.2.2
";
        let hosts = parse_with_user(content, "localuser".into()).hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].name, "prod");
        assert_eq!(hosts[0].username, "deploy");
        assert_eq!(hosts[0].port, 22, "Match 块里的 Port 不能污染 prod");
        assert_eq!(hosts[1].name, "after");
        assert_eq!(hosts[1].username, "localuser");
    }

    /// 引号需要剥离；多个 IdentityFile 取第一个（与 ssh 尝试顺序一致）。
    #[test]
    fn identity_file_strips_quotes_and_keeps_first() {
        let content = "\
Host quoted
    HostName 10.4.4.4
    IdentityFile \"~/my keys/id_rsa\"
    IdentityFile ~/.ssh/id_ed25519
";
        let hosts = parse_with_user(content, "localuser".into()).hosts;
        let key = hosts[0].key_path.clone().unwrap();
        assert!(key.ends_with("my keys/id_rsa"), "应保留第一个且不带引号: {key}");
        assert!(!key.contains('"'), "引号应被剥离: {key}");
    }

    /// 通配块与空 Host 行计入 ignored，且不影响正常主机。
    #[test]
    fn counts_ignored_rule_blocks() {
        let content = "\
Host *
    User nobody

Host
    HostName 1.2.3.4

Host real
    HostName 10.5.5.5
";
        let parsed = parse_with_user(content, "localuser".into());
        assert_eq!(parsed.hosts.len(), 1);
        assert_eq!(parsed.hosts[0].name, "real");
        assert_eq!(parsed.ignored, 2);
    }

    /// 文件带 UTF-8 BOM 时，第一台主机不能被漏掉。
    #[test]
    fn strips_utf8_bom_before_first_host() {
        let content = "\u{feff}Host first\n    HostName 10.0.0.1\n\nHost second\n    HostName 10.0.0.2\n";
        let parsed = parse_with_user(content, "localuser".into());
        assert_eq!(parsed.hosts.len(), 2);
        assert_eq!(parsed.hosts[0].name, "first");
        assert_eq!(parsed.hosts[0].address, "10.0.0.1");
    }

    /// 一行多别名时只取第一个，其余别名不产生额外主机。
    #[test]
    fn takes_first_alias_of_multi_alias_host_line() {
        let content = "Host a b c\n    HostName 10.2.2.2\n";
        let parsed = parse_with_user(content, "localuser".into());
        assert_eq!(parsed.hosts.len(), 1);
        assert_eq!(parsed.hosts[0].name, "a");
        assert_eq!(parsed.hosts[0].address, "10.2.2.2");
    }
}
