use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub username: String,
    /// "key" 或 "password"，M1 阶段认证由 ssh 进程交互完成
    pub auth_type: String,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub proxy_jump: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at: u64,
}

fn default_protocol() -> String {
    "openai-compatible".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiModel {
    pub id: String,
    pub label: String,
    pub model: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub models: Vec<AiModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRule {
    pub id: String,
    pub pattern: String,
    pub enabled: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub id: String,
    pub ts: u64,
    pub session_id: Option<u32>,
    pub host_id: String,
    pub host_label: String,
    pub tool_name: String,
    pub summary: String,
    pub permission_mode: String,
    pub approval: String,
    pub status: String,
    pub result: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub metric: String,
    pub operator: String,
    pub threshold: f64,
    pub channel: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default = "default_cooldown")]
    pub cooldown_min: u64,
    pub enabled: bool,
    pub created_at: u64,
}

fn default_cooldown() -> u64 {
    10
}

impl Host {
    pub fn label_address(&self) -> String {
        format!("{}@{}:{}", self.username, self.address, self.port)
    }

    /// 构造系统 OpenSSH 参数（M1 阶段）
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-tt".to_string(),
            "-o".to_string(),
            // 不转发本地 locale 变量，避免远端缺少 C.UTF-8 等 locale 时产生警告
            "SendEnv -LC_* -LANG".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            "-p".to_string(),
            self.port.to_string(),
        ];
        if let Some(key) = &self.key_path {
            if !key.trim().is_empty() {
                args.push("-i".to_string());
                args.push(key.clone());
            }
        }
        if let Some(jump) = &self.proxy_jump {
            if !jump.trim().is_empty() {
                args.push("-J".to_string());
                args.push(jump.clone());
            }
        }
        args.push(format!("{}@{}", self.username, self.address));
        args
    }
}
