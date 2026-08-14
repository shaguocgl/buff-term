use serde::{Deserialize, Serialize};

/// SSH 认证方式：`key`（密钥）或 `password`（密码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    Key,
    Password,
}

impl AuthType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthType::Key => "key",
            AuthType::Password => "password",
        }
    }
}

impl std::str::FromStr for AuthType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "password" => Ok(AuthType::Password),
            _ => Ok(AuthType::Key),
        }
    }
}

/// 内置 AI Agent 的安全级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    All,
    Smart,
    None,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::All => "all",
            PermissionMode::Smart => "smart",
            PermissionMode::None => "none",
        }
    }
}

/// 对外 MCP 服务的权限模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpPermissionMode {
    Readonly,
    Confirm,
    Allow,
}

impl McpPermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            McpPermissionMode::Readonly => "readonly",
            McpPermissionMode::Confirm => "confirm",
            McpPermissionMode::Allow => "allow",
        }
    }
}

impl std::str::FromStr for McpPermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "readonly" => Ok(McpPermissionMode::Readonly),
            "allow" => Ok(McpPermissionMode::Allow),
            _ => Ok(McpPermissionMode::Confirm),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub username: String,
    /// 认证方式，凭据由 russh 从系统钥匙串注入
    pub auth_type: AuthType,
    #[serde(default)]
    pub key_path: Option<String>,
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
pub struct McpRule {
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
pub struct InspectionReport {
    pub id: String,
    pub host_id: String,
    pub host_label: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub status: String,
    pub risk_level: String,
    pub summary: String,
    pub markdown: String,
    pub html: String,
    pub email_sent: bool,
    pub error: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlertSettings {
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub smtp_from: Option<String>,
    pub smtp_to: Option<String>,
    /// starttls / ssl / none
    pub smtp_tls: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpService {
    pub enabled: bool,
    #[serde(default)]
    pub host_ids: Vec<String>,
    /// readonly（只读）/ confirm（危险命令需确认）/ allow（全部放行）
    #[serde(default = "default_mcp_permission")]
    pub permission_mode: McpPermissionMode,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub updated_at: u64,
}

fn default_mcp_permission() -> McpPermissionMode {
    McpPermissionMode::Confirm
}

impl Host {
    pub fn label_address(&self) -> String {
        format!("{}@{}:{}", self.username, self.address, self.port)
    }
}
