export interface Host {
  id: string;
  name: string;
  address: string;
  port: number;
  username: string;
  auth_type: 'key' | 'password';
  key_path?: string | null;
  notes?: string | null;
  created_at: number;
}

export interface HostInput {
  name: string;
  address: string;
  port: number;
  username: string;
  auth_type: 'key' | 'password';
  key_path?: string;
  notes?: string;
}

export interface ImportResult {
  /** 本次导入（dryRun 预览时表示「将要导入」）的主机数 */
  imported: number;
  /** 因与已有主机重名而跳过的主机数 */
  skipped: number;
  /** 因通配规则或空 Host 行被忽略的规则块数 */
  ignored: number;
}

export interface AiProvider {
  id: string;
  name: string;
  base_url: string;
  protocol: string;
  enabled: boolean;
  created_at: number;
  models: AiModel[];
}

export interface AiModel {
  id: string;
  label: string;
  model: string;
  is_active: boolean;
  /** 该模型支持的上下文窗口（token），由用户在 AI 配置中填写 */
  context_window: number;
  /** 该模型是否支持图片输入（多模态），由用户在 AI 配置中勾选 */
  supports_vision: boolean;
}

export interface AiModelInput {
  label: string;
  model: string;
  is_active?: boolean;
  /** 该模型支持的上下文窗口（token），必填 */
  context_window: number;
  /** 该模型是否支持图片输入（多模态） */
  supports_vision?: boolean;
}

export interface AiProviderInput {
  name: string;
  base_url: string;
  protocol?: string;
  enabled?: boolean;
  models: AiModelInput[];
  api_key?: string;
}

export interface TestResult {
  ok: boolean;
  message: string;
}

export interface RemoteAiModel {
  id: string;
  owned_by?: string | null;
}

export interface UpdateInfo {
  current_version: string;
  latest_version: string;
  update_available: boolean;
  release_url: string;
  release_found: boolean;
}

/** 启动计数与 Star 引导状态（recordLaunch 返回） */
export interface LaunchState {
  /** 本机累计启动次数（含本次） */
  launches: number;
  /** 本次启动是否应展示 GitHub Star 引导弹窗 */
  show_star_prompt: boolean;
}

export interface AiRule {
  id: string;
  pattern: string;
  enabled: boolean;
  created_at: number;
}

/** AI Agent 的结构化任务台账，跨上下文压缩窗口保留。 */
export interface TaskPlan {
  goal: string;
  constraints: string[];
  completed: string[];
  pending: string[];
  failed: string[];
  current_step: string;
  updated_at: number;
}

export type CompressionStrategy =
  | 'none'
  | 'extractive'
  | 'reactive_reduce'
  | 'hard_reset';

/** 每次模型调用前的上下文用量与压缩状态。 */
export interface ContextUsage {
  session_id: number;
  /** 本次真正发送给模型的视图估算（含压缩后） */
  used_tokens: number;
  /** 当前完整历史（未压缩）的估算，用于说明“历史已经有多大” */
  history_tokens: number;
  budget_tokens: number;
  window_tokens: number;
  compressed_rounds: number;
  strategy: CompressionStrategy;
  estimated: boolean;
  /** 估算值是否已用平台实测值校准过 */
  calibrated: boolean;
  warning?: string | null;
}

export interface AuditLog {
  id: string;
  ts: number;
  session_id: number | null;
  host_id: string;
  host_label: string;
  tool_name: string;
  /** 操作来源：agent（AI Agent）/ guard（终端防护）/ mcp（MCP 服务）/ remediation（修复执行） */
  source: string;
  summary: string;
  permission_mode: string;
  approval: string;
  status: string;
  result: string | null;
  duration_ms: number | null;
}

export interface DiskInfo {
  /** 挂载点 */
  mount: string;
  /** 文件系统/设备名 */
  fs: string;
  /** 人类可读总容量，如 "40G" */
  total: string;
  /** 人类可读已用量 */
  used: string;
  /** 总容量（KB），供磁盘卡片头部汇总 */
  total_kb: number;
  /** 已用容量（KB） */
  used_kb: number;
  /** 使用率 0-100 */
  percent: number;
}

export interface MemInfo {
  total_mb: number;
  used_mb: number;
  percent: number;
}

export interface TopProc {
  user: string;
  cpu: string;
  mem: string;
  cmd: string;
}

/** 单块物理网卡的累计收发字节数（自系统启动起） */
export interface NetIface {
  name: string;
  rx_bytes: number;
  tx_bytes: number;
}

/** 网络流量：rx/tx 为所有物理网卡累计值之和，ifaces 为逐网卡明细 */
export interface NetInfo {
  rx_bytes: number;
  tx_bytes: number;
  ifaces: NetIface[];
}

export interface MonitorSnapshot {
  ts: number;
  host_label: string;
  load: string;
  cpu_percent: number;
  mem: MemInfo;
  swap: MemInfo;
  disks: DiskInfo[];
  net: NetInfo;
  /** 按 CPU 排序的 TOP10 进程 */
  top_cpu: TopProc[];
  /** 按内存排序的 TOP10 进程 */
  top_mem: TopProc[];
}

/** 服务器静态信息：面板打开时一次性采集，不随 5s 轮询刷新 */
export interface HostInfo {
  /** 主机名 */
  hostname: string;
  /** 系统发行版名，如 Ubuntu 24.04.2 LTS */
  os: string;
  /** 内核版本 */
  kernel: string;
  /** CPU 架构，如 x86_64 */
  arch: string;
  /** CPU 型号 */
  cpu_model: string;
  /** 物理核心数（Socket × 每 socket 核数），取不到时=线程数 */
  cores: number;
  /** 逻辑线程数（vCPU） */
  threads: number;
  /** 内存总大小（MB） */
  mem_total_mb: number;
  /** 系统已运行秒数 */
  uptime_secs: number;
  /** 服务器出口公网 IP，获取失败为空串 */
  public_ip: string;
  /** 国家 / 城市，如「美国 · 洛杉矶」，获取失败为空串 */
  location: string;
}

/** 历史指标点（host_metrics 表的精简投影，供监控面板趋势图回填） */
export interface HostMetricPoint {
  ts: number;
  cpu_percent: number;
  mem_percent: number;
}

export interface AlertSettings {
  smtp_host?: string | null;
  smtp_port?: number | null;
  smtp_username?: string | null;
  smtp_password?: string | null;
  smtp_from?: string | null;
  smtp_to?: string | null;
  smtp_tls?: string | null;
}

export interface McpService {
  enabled: boolean;
  host_ids: string[];
  permission_mode: 'readonly' | 'confirm' | 'allow';
  token?: string | null;
  port?: number | null;
  updated_at: number;
  running: boolean;
}

export interface McpServiceInput {
  enabled: boolean;
  host_ids: string[];
  permission_mode: McpService['permission_mode'];
}

export interface McpRule {
  id: string;
  pattern: string;
  enabled: boolean;
  created_at: number;
}

export interface TerminalRule {
  id: string;
  pattern: string;
  enabled: boolean;
  builtin: boolean;
  created_at: number;
}

export interface TerminalGuardSettings {
  enabled: boolean;
  timeout_secs: number;
}

export interface TerminalGuardApproval {
  session_id: number;
  request_id: string;
  host_label: string;
  command: string;
  matched_patterns: string[];
  /** 审批倒计时秒数，超时按拒绝处理 */
  timeout_secs: number;
}

export interface McpApprovalRequest {
  request_id: string;
  host: string;
  host_label: string;
  command: string;
  /** 审批倒计时秒数，超时按拒绝处理 */
  timeout_secs?: number;
}

export interface HostKeyConfirmRequest {
  key: string;
  host: string;
  port: string;
  fingerprint: string;
  key_type: string;
}

export type InspectionStatus =
  | 'running'
  | 'success'
  | 'failed'
  | 'cancelled';

export type InspectionRiskLevel = 'low' | 'medium' | 'high' | 'unknown';

export interface InspectionReport {
  id: string;
  host_id: string;
  host_label: string;
  provider_id: string;
  provider_name: string;
  model: string;
  status: InspectionStatus;
  risk_level: InspectionRiskLevel;
  summary: string;
  markdown: string;
  html: string;
  email_sent: boolean;
  error: string | null;
  created_at: number;
  finished_at: number | null;
  duration_ms: number | null;
}

export interface InspectionProgressPayload {
  report_id: string;
  phase: 'collect' | 'analyze' | 'exec' | 'render' | 'email';
  message: string;
}

export interface InspectionDonePayload {
  report_id: string;
  status: InspectionStatus;
}

export interface InspectionErrorPayload {
  report_id: string;
  message: string;
}

export type RemediationStatus =
  | 'draft'
  | 'planning'
  | 'plan_ready'
  | 'executing'
  | 'success'
  | 'failed'
  | 'cancelled';

export type RemediationStepStatus = 'pending' | 'running' | 'success' | 'error';

export interface RemediationStep {
  id: string;
  description: string;
  command: string;
  timeout_secs: number;
  dangerous: boolean;
  status: RemediationStepStatus;
  output: string | null;
}

export interface RemediationStepInput {
  description: string;
  command: string;
  timeout_secs: number;
}

export interface Remediation {
  id: string;
  report_id: string;
  host_id: string;
  host_label: string;
  provider_id: string;
  provider_name: string;
  model: string;
  intervention: string;
  plan_markdown: string;
  steps: RemediationStep[];
  status: RemediationStatus;
  error: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  duration_ms: number | null;
}

export interface RemediationProgressPayload {
  remediation_id: string;
  phase: 'planning' | 'step_start' | 'step_success' | 'step_error';
  message: string;
  step_index?: number | null;
  total?: number | null;
}

export interface RemediationDonePayload {
  remediation_id: string;
  status: RemediationStatus;
}

export interface SftpEntry {
  name: string;
  is_dir: boolean;
  is_symlink: boolean;
  size: number;
  mtime: number;
  perms: string;
  user: string;
  group: string;
}

/** 远端文本文件内容：中间区内置编辑器打开时返回 */
export interface SftpFileContent {
  /** 文件文本内容（UTF-8） */
  content: string;
  /** 文件字节数 */
  size: number;
  /** 最近修改时间（unix 秒），保存时回传用于并发修改检测 */
  mtime: number;
  /** 是否命中敏感路径（保存需二次确认） */
  sensitive: boolean;
}

/** 远端文件保存结果 */
export interface SftpWriteOutcome {
  ok: boolean;
  text: string;
  /** 命中敏感路径且本次未确认：本次未执行写入 */
  requires_confirm: boolean;
  /** 写入后远端文件的 mtime（unix 秒），0 表示未取到 */
  mtime: number;
}

export interface RemediationErrorPayload {
  remediation_id: string;
  message: string;
}

export interface HistoryToolCall {
  id: string;
  type: string;
  function: { name: string; arguments: string };
}

export interface HistoryEntry {
  role: string;
  content?: string;
  /** 多模态 user 消息拆出的图片（data URL），仅 user 角色可能出现 */
  images?: string[];
  tool_calls?: HistoryToolCall[];
  tool_call_id?: string;
}
