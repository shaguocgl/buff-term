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

/// 模型上下文窗口的兜底默认值（token）。用户新增/编辑模型时必须显式填写，
/// 该值只用于旧数据迁移和批量导入预填，不做任何模型名推断。
pub fn default_context_window() -> u32 {
    128_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiModel {
    pub id: String,
    pub label: String,
    pub model: String,
    pub is_active: bool,
    /// 该模型支持的上下文窗口（token）。由用户在 AI 配置中显式填写。
    #[serde(default = "default_context_window")]
    pub context_window: u32,
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

/// AI Agent 的结构化任务台账：跨压缩窗口保存“目标/约束/进度/失败尝试”，
/// 每轮注入系统提示，永不参与裁剪。仅保存在内存中，随会话重置清空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPlan {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub current_step: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl TaskPlan {
    /// 是否为空台账（没有目标且没有进度）。空台账不注入系统提示。
    pub fn is_empty(&self) -> bool {
        self.goal.trim().is_empty()
            && self.current_step.trim().is_empty()
            && self.constraints.is_empty()
            && self.completed.is_empty()
            && self.pending.is_empty()
            && self.failed.is_empty()
    }

    /// 渲染成注入系统提示的固定文本块。内容保持确定性，便于 prompt 缓存。
    pub fn render_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("[任务台账｜必须遵守，禁止忽略]\n");
        if !self.goal.trim().is_empty() {
            out.push_str(&format!("目标：{}\n", self.goal.trim()));
        }
        if !self.constraints.is_empty() {
            out.push_str("约束：\n");
            for item in &self.constraints {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.completed.is_empty() {
            out.push_str("已完成：\n");
            for item in &self.completed {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.pending.is_empty() {
            out.push_str("待办：\n");
            for item in &self.pending {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.failed.is_empty() {
            out.push_str("失败尝试（不要在没有新依据时重复）：\n");
            for item in &self.failed {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.current_step.trim().is_empty() {
            out.push_str(&format!("当前步骤：{}\n", self.current_step.trim()));
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRule {
    pub id: String,
    pub pattern: String,
    pub enabled: bool,
    pub created_at: u64,
}

/// 终端危险命令拦截规则（交互终端回车前判定，子串匹配）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalRule {
    pub id: String,
    pub pattern: String,
    pub enabled: bool,
    /// 是否为预置规则（可删除，可通过“恢复预置”一键还原）
    pub builtin: bool,
    pub created_at: u64,
}

/// 终端危险命令拦截设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalGuardSettings {
    #[serde(default)]
    pub enabled: bool,
    /// 审批超时秒数（超时按拒绝处理）
    #[serde(default = "default_guard_timeout")]
    pub timeout_secs: u64,
}

fn default_guard_timeout() -> u64 {
    60
}

impl Default for TerminalGuardSettings {
    fn default() -> Self {
        TerminalGuardSettings {
            enabled: false,
            timeout_secs: default_guard_timeout(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationStep {
    pub id: String,
    pub description: String,
    pub command: String,
    pub timeout_secs: u64,
    pub dangerous: bool,
    pub status: String,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationStepInput {
    pub description: String,
    pub command: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remediation {
    pub id: String,
    pub report_id: String,
    pub host_id: String,
    pub host_label: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub intervention: String,
    pub plan_markdown: String,
    pub steps: Vec<RemediationStep>,
    pub status: String,
    pub error: Option<String>,
    pub created_at: u64,
    pub started_at: Option<u64>,
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

/// 主机历史指标快照中单个磁盘的信息（用于趋势展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricDisk {
    pub mount: String,
    pub percent: f64,
}

/// 主机历史指标快照中 TOP 进程的精简信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricTop {
    pub cmd: String,
    pub cpu: String,
    pub mem: String,
}

/// 写入 host_metrics 表时使用的参数聚合体（对应 `Db::insert_metric`）。
/// 此前该方法是 10 个位置参数，字段类型高度相似（多个 u64/f64/&str 挤在一起），
/// 顺序传错编译器也无法察觉，改为具名字段的结构体传参。
pub struct NewMetric<'a> {
    pub host_id: &'a str,
    pub ts: u64,
    pub cpu_percent: f64,
    pub load1: f64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_percent: f64,
    pub disks_json: &'a str,
    pub top_json: &'a str,
    pub source: &'a str,
}

/// 一条主机历史指标记录，对应 host_metrics 表的一行。
/// 由 monitor.rs 的 MonitorSnapshot 转换而来，按时间累积形成趋势序列。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostMetric {
    pub id: i64,
    pub host_id: String,
    pub ts: u64,
    pub cpu_percent: f64,
    pub load1: f64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_percent: f64,
    pub disks: Vec<MetricDisk>,
    pub top: Vec<MetricTop>,
    pub source: String,
}
