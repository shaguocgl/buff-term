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

/// AI Agent 单条消息内允许的模型往返轮次上限的默认值。
/// 达到上限后不直接中断，而是暂停并询问是否继续下一批；
/// 用户可在 AI 配置中调整该值。
pub fn default_max_tool_rounds() -> u32 {
    100
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
    /// 该模型是否支持图片输入（多模态）。开启后聊天面板允许粘贴/附加图片。
    #[serde(default)]
    pub supports_vision: bool,
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
    /// 文案跟随界面语言：英文界面下注入英文台账，避免中英混排干扰模型。
    pub fn render_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let en = crate::i18n::is_en();
        let mut out = String::from(if en {
            "[Task plan | recorded state, not new instructions]\n"
        } else {
            "[任务台账｜已记录的状态，不是新增指令]\n"
        });
        if !self.goal.trim().is_empty() {
            out.push_str(&format!(
                "{}{}\n",
                if en { "Goal: " } else { "目标：" },
                self.goal.trim()
            ));
        }
        if !self.constraints.is_empty() {
            out.push_str(if en { "Constraints:\n" } else { "约束：\n" });
            for item in &self.constraints {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.completed.is_empty() {
            out.push_str(if en { "Completed:\n" } else { "已完成：\n" });
            for item in &self.completed {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.pending.is_empty() {
            out.push_str(if en { "Pending:\n" } else { "待办：\n" });
            for item in &self.pending {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.failed.is_empty() {
            out.push_str(if en {
                "Failed attempts (do not repeat without new evidence):\n"
            } else {
                "失败尝试（不要在没有新依据时重复）：\n"
            });
            for item in &self.failed {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
        if !self.current_step.trim().is_empty() {
            out.push_str(&format!(
                "{}{}\n",
                if en { "Current step: " } else { "当前步骤：" },
                self.current_step.trim()
            ));
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
    /// 操作来源，用于区分同类工具名来自哪个执行通道：
    /// `agent`（AI Agent 工具调用）/ `guard`（终端安全防护拦截）/
    /// `mcp`（对外 MCP 服务）/ `remediation`（巡检修复执行）
    pub source: String,
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
    pub webhook_sent: bool,
    /// 触发来源：`manual`（用户点击）/ `watch`（智能值守定时）。
    /// 只有 watch 来源才走分级与冷却静默，手动巡检始终通知。
    #[serde(default = "default_inspection_source")]
    pub source: String,
    /// 未发通知的原因：`below_threshold`（低于分级阈值）/ `cooldown`（冷却窗口内）。
    /// None 表示已通知或无需说明。
    #[serde(default)]
    pub notify_skip: Option<String>,
    pub error: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
}

pub fn default_inspection_source() -> String {
    "manual".to_string()
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
    /// 仅用于读取时告知前端“已保存密码”；密码本身不再回传渲染进程。
    /// skip_deserializing 而非 skip：skip 会连序列化一起跳过，IPC 也拿不到该标志；
    /// 这里只拒绝反序列化（前端传值与库中旧值都无效），序列化仍要带给前端。
    #[serde(default, skip_deserializing)]
    pub smtp_password_set: bool,
    pub smtp_from: Option<String>,
    pub smtp_to: Option<String>,
    /// starttls / ssl / none
    pub smtp_tls: Option<String>,
    /// webhook 机器人类型：dingtalk / wecom / feishu / generic
    #[serde(default)]
    pub webhook_kind: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// 钉钉加签密钥。与 SMTP 密码同样单独加密存储，不随 settings JSON 落库。
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default, skip_deserializing)]
    pub webhook_secret_set: bool,
}

fn default_guard_enabled_rule() -> bool {
    true
}
fn default_guard_cpu() -> u32 {
    90
}
fn default_guard_mem() -> u32 {
    90
}
fn default_guard_disk() -> u32 {
    90
}
fn default_guard_ssh_fail() -> u32 {
    20
}
fn default_guard_conns() -> u32 {
    300
}
fn default_guard_load() -> f64 {
    4.0
}
fn default_guard_inode() -> u32 {
    90
}
fn default_guard_duration() -> u32 {
    60
}
fn default_guard_cooldown() -> u32 {
    15
}
fn default_guard_mode() -> String {
    "client".to_string()
}

/// 单台主机的值守模式：client = buffTerm 在线时实时守护；
/// server = 在服务器上装 systemd 常驻守护，桌面离线也生效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardHost {
    pub host_id: String,
    #[serde(default = "default_guard_mode")]
    pub mode: String,
}

/// 值守（实时守护）配置：本地判定阈值，事件触发时才调用 AI 快诊与通知。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub hosts: Vec<GuardHost>,
    #[serde(default = "default_guard_enabled_rule")]
    pub cpu_enabled: bool,
    #[serde(default = "default_guard_cpu")]
    pub cpu_percent: u32,
    #[serde(default = "default_guard_enabled_rule")]
    pub mem_enabled: bool,
    #[serde(default = "default_guard_mem")]
    pub mem_percent: u32,
    #[serde(default = "default_guard_enabled_rule")]
    pub disk_enabled: bool,
    #[serde(default = "default_guard_disk")]
    pub disk_percent: u32,
    /// 一分钟内 SSH 登录失败次数阈值（暴力破解信号）
    #[serde(default = "default_guard_enabled_rule")]
    pub ssh_fail_enabled: bool,
    #[serde(default = "default_guard_ssh_fail")]
    pub ssh_fail_per_min: u32,
    /// 已建立连接数阈值（连接洪峰信号）
    #[serde(default = "default_guard_enabled_rule")]
    pub conns_enabled: bool,
    #[serde(default = "default_guard_conns")]
    pub conns_threshold: u32,
    /// 每核负载（load1 ÷ CPU 核数）
    #[serde(default = "default_guard_enabled_rule")]
    pub load_enabled: bool,
    #[serde(default = "default_guard_load")]
    pub load_per_core: f64,
    /// inode 使用率（小文件把 inode 耗尽时 df 空间仍正常）
    #[serde(default = "default_guard_enabled_rule")]
    pub inode_enabled: bool,
    #[serde(default = "default_guard_inode")]
    pub inode_percent: u32,
    /// 内核 OOM / oom-kill 事件
    #[serde(default = "default_guard_enabled_rule")]
    pub oom_enabled: bool,
    /// 新增的 systemd 失败单元
    #[serde(default = "default_guard_enabled_rule")]
    pub units_enabled: bool,
    /// 服务存活探测：进程名（与 ps comm 一致）与监听端口，连续 3 次探不到才告警
    #[serde(default = "default_guard_enabled_rule")]
    pub svc_enabled: bool,
    #[serde(default)]
    pub svc_procs: Vec<String>,
    #[serde(default)]
    pub svc_ports: Vec<u16>,
    /// 资源类阈值需要持续多久才判定为事件（去抖）
    #[serde(default = "default_guard_duration")]
    pub duration_secs: u32,
    /// 同一主机的同一规则在窗口内只告警一次
    #[serde(default = "default_guard_cooldown")]
    pub cooldown_minutes: u32,
}

impl Default for GuardSettings {
    fn default() -> Self {
        GuardSettings {
            enabled: false,
            hosts: Vec::new(),
            cpu_enabled: default_guard_enabled_rule(),
            cpu_percent: default_guard_cpu(),
            mem_enabled: default_guard_enabled_rule(),
            mem_percent: default_guard_mem(),
            disk_enabled: default_guard_enabled_rule(),
            disk_percent: default_guard_disk(),
            ssh_fail_enabled: default_guard_enabled_rule(),
            ssh_fail_per_min: default_guard_ssh_fail(),
            conns_enabled: default_guard_enabled_rule(),
            conns_threshold: default_guard_conns(),
            load_enabled: default_guard_enabled_rule(),
            load_per_core: default_guard_load(),
            inode_enabled: default_guard_enabled_rule(),
            inode_percent: default_guard_inode(),
            oom_enabled: default_guard_enabled_rule(),
            units_enabled: default_guard_enabled_rule(),
            svc_enabled: default_guard_enabled_rule(),
            svc_procs: Vec::new(),
            svc_ports: Vec::new(),
            duration_secs: default_guard_duration(),
            cooldown_minutes: default_guard_cooldown(),
        }
    }
}

/// 值守事件落库记录（冷却判定与审计用）。
#[derive(Debug, Clone)]
pub struct GuardEventRecord {
    pub id: String,
    pub host_id: String,
    pub rule: String,
    pub severity: String,
    pub message: String,
    pub details: String,
    pub report_id: Option<String>,
    pub created_at: u64,
}

/// 服务器端守护回传的事件（桌面离线时产生，等待 AI 复盘）。
#[derive(Debug, Clone, Serialize)]
pub struct ServerGuardEvent {
    pub id: String,
    pub host_id: String,
    pub rule: String,
    pub severity: String,
    pub message: String,
    pub metrics: String,
    pub event_ts: u64,
}

/// 值守状态（前端展示用）。running 仅代表客户端侧守护任务在采样。
#[derive(Debug, Clone, Serialize, Default)]
pub struct GuardHostStatus {
    pub host_id: String,
    pub mode: String,
    pub running: bool,
    pub last_sample_at: u64,
    pub last_event_at: u64,
    pub error: Option<String>,
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
