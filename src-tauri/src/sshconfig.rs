use crate::hosts::HostInput;
use crate::models::AuthType;

/// 解析 ~/.ssh/config 的常用字段，返回可导入的主机列表。
pub fn parse(content: &str) -> Vec<HostInput> {
    let mut hosts: Vec<HostInput> = Vec::new();
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
                if let Some(h) = current.take() {
                    if is_importable(&h) {
                        hosts.push(h);
                    }
                }
                let mut names = value.split_whitespace();
                let first = names.next().unwrap_or("").to_string();
                if first.contains('*') || first.contains('?') {
                    current = None; // 通配符块不导入
                } else {
                    current = Some(HostInput {
                        name: first,
                        address: String::new(),
                        port: 22,
                        username: String::new(),
                        auth_type: AuthType::Key,
                        key_path: None,
                        notes: Some("来自 ~/.ssh/config".to_string()),
                    });
                }
            }
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
                    h.key_path = Some(expand_tilde(value));
                }
            }
            _ => {}
        }
    }

    if let Some(h) = current.take() {
        if is_importable(&h) {
            hosts.push(h);
        }
    }
    hosts
}

fn is_importable(h: &HostInput) -> bool {
    !h.name.is_empty() && (!h.address.is_empty() || h.name.contains('.'))
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
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
        let hosts = parse(content);
        assert_eq!(hosts.len(), 2);

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
    fn ignores_comments_and_blank_lines() {
        let content = "# a comment\n\nHost simple\n    HostName 10.0.0.1\n";
        let hosts = parse(content);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "simple");
        assert_eq!(hosts[0].address, "10.0.0.1");
    }
}
