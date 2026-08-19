//! 终端危险命令拦截（方案 A：扣住回车 + 本地行重建）。
//!
//! 核心：`GuardEngine` 是一个**纯状态机**，不依赖 Tauri，便于单元测试。
//! 行缓冲只依赖“应用自己转发的字节”，不解析远端回显，因此不触碰 IME 输入路径。
//!
//! v1 策略：
//! - 仅“智能拦截”：命中规则 → 扣住 Enter → 弹窗审批；批准放行 Enter，拒绝发 Ctrl-U 清行；
//! - `Suspended`（方向键 / Tab / ESC 等导致行不可信）→ 一律放行（不打扰用户）；
//! - 审批超时按拒绝处理；Holding 期间收到 Ctrl-C 视为取消审批。

use std::sync::Arc;
use crate::db::Db;
use crate::models::{AuditLog, TerminalGuardSettings, TerminalRule};
use crate::safety::sanitize;
use crate::util::{now, truncate};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

/// 预置危险命令（子串匹配，大小写不敏感）。首次建库时写入，可在配置中删除或恢复。
pub const PRESET_TERMINAL_RULES: &[&str] = &[
    // 删除
    "rm -rf", "rm -fr", "rm -r", "rm --recursive",
    // 磁盘 / 分区 / 格式化
    "mkfs", "fdisk", "parted", "pvremove", "vgremove", "lvremove",
    "dd if=", ">/dev/sd", "of=/dev/sd", "shred", "wipefs",
    // 关机 / 重启
    "shutdown", "reboot", "poweroff", "halt", "init 0", "init 6",
    // 服务管理
    "systemctl stop", "systemctl restart", "systemctl disable", "systemctl mask",
    // 防火墙 / 网络
    "iptables", "ufw", "firewall-cmd",
    // 账户 / 权限
    "userdel", "groupdel", "chmod -r", "chown -r", "passwd -d",
    // 数据库
    "drop database", "drop table", "truncate table", "delete from",
    // 进程
    "kill -9", "pkill -9", "killall -9",
    // 文件属性
    "chattr -i", "chattr +i",
    // Git
    "git push --force", "git push -f", "git reset --hard",
    // 危险管道（下载即执行 / 管道给 shell）
    "| sh", "| bash", "| zsh",
    // 覆盖系统文件
    "> /etc/", "cat > /etc/", ": > /etc/",
    // 容器
    "docker rm -f", "docker rmi -f", "docker system prune", "docker volume rm",
];

// ---------- 配置快照 ----------

#[derive(Debug, Clone, Default)]
pub struct GuardConfig {
    pub enabled: bool,
    pub timeout_secs: u64,
    /// 全部小写化的启用规则 pattern
    pub patterns: Vec<String>,
}

impl GuardConfig {
    pub fn from_settings(settings: &TerminalGuardSettings, rules: &[TerminalRule]) -> Self {
        GuardConfig {
            enabled: settings.enabled,
            timeout_secs: settings.timeout_secs.max(10),
            patterns: rules
                .iter()
                .filter(|r| r.enabled)
                .map(|r| r.pattern.trim().to_ascii_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }
}

// ---------- 审批请求 ----------

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub command: String,
    pub matched_patterns: Vec<String>,
}

// ---------- 审计事件 ----------

#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub command: String,
    pub matched_patterns: Vec<String>,
    pub approved: bool,
    pub timed_out: bool,
}

// ---------- 处理结果 ----------

#[derive(Debug, Default)]
pub struct GuardOutcome {
    /// 需要立即转发给远端的字节
    pub forward: Vec<u8>,
    /// 需要弹窗审批的请求
    pub approval: Option<ApprovalRequest>,
    /// 需要写入审计的事件
    pub audit: Option<AuditEvent>,
}

// ---------- 状态机 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardState {
    /// 当前行内容可信（由应用转发的字节重建）
    Known,
    /// 行内容不可信（方向键 / Tab / ESC 等），v1 一律放行
    Suspended,
    /// 命中规则，Enter 被扣住，等待审批
    Holding,
}

/// 回显同步缓冲上限：方向键等触发 Suspended 后，readline 重绘的字节累积到这里，
/// 超出时截断保留尾部（判定只需要最后一行）。
const SYNC_BUF_MAX: usize = 4096;

pub struct GuardEngine {
    config: GuardConfig,
    /// alternate screen（vim/htop/less 等全屏应用）期间为 true：输入不经过判定，
    /// 避免把全屏应用内的文本输入/编辑误判为 shell 命令（原始应用内本就是盲区）。
    passthrough: bool,
    state: GuardState,
    /// Suspended 期间正在消费的 ESC 序列（0x1b 开头，终结符 0x40-0x7e）
    esc_pending: bool,
    /// Suspended 期间累积的远端回显（readline 重绘的“提示符 + 历史命令”），
    /// Enter 时提取最后一行参与判定，从而覆盖方向键历史回放 / Tab 补全等盲区。
    sync_buf: Vec<u8>,
    /// 当前行（UTF-8 字节）
    line: Vec<u8>,
    /// 进入 Suspended（Tab / 方向键等）瞬间的行快照：补全/历史只会在其上扩展，
    /// Enter 时用它和回显重建“补全后的完整命令”（本地输入 + 补全后缀）。
    line_at_suspend: Vec<u8>,
    /// 续行累积内容（`\` 结尾的上一行）
    accumulated: Vec<u8>,
    /// Holding 期间扣住的后续输入
    held_input: Vec<u8>,
    /// Suspended 后是否又按过 Tab：readline 可能已替换上次 Tab 后的输入
    /// （如 confcon → conf），此标记使这些“过期输入”不再拼进弹窗命令。
    tab_again: bool,
    request_id: String,
    matched: Vec<String>,
}

impl GuardEngine {
    pub fn new(config: GuardConfig) -> Self {
        GuardEngine {
            config,
            passthrough: false,
            esc_pending: false,
            sync_buf: Vec::new(),
            state: GuardState::Known,
            line: Vec::new(),
            line_at_suspend: Vec::new(),
            accumulated: Vec::new(),
            held_input: Vec::new(),
            tab_again: false,
            request_id: String::new(),
            matched: Vec::new(),
        }
    }

    pub fn set_config(&mut self, config: GuardConfig) {
        self.config = config;
    }

    /// 行内容是否处于不可信状态（Tab / 方向键 / 编辑键后），
    /// 此时回车需要先与远端 readline 补全/重绘回显同步。
    pub fn is_suspended(&self) -> bool {
        self.state == GuardState::Suspended
    }

    #[cfg(test)]
    pub fn is_holding(&self) -> bool {
        self.state == GuardState::Holding
    }

    #[cfg(test)]
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// 切换透传模式。每次按键都会以当前 buffer 状态调用，因此**幂等**：
    /// 值不变时不清空行缓冲（避免清掉正在输入的命令），仅在真正切换时清空。
    pub fn set_passthrough(&mut self, on: bool) {
        if self.passthrough == on {
            return;
        }
        self.passthrough = on;
        self.esc_pending = false;
        self.sync_buf.clear();
        self.line.clear();
        self.line_at_suspend.clear();
        self.accumulated.clear();
        self.held_input.clear();
        self.tab_again = false;
        self.matched.clear();
        self.state = GuardState::Known;
    }

    /// 接收远端回显字节（在 session 的 read 循环里调用）。
    /// 仅在 Suspended 时累积：readline 会用 `\r + 提示符 + 完整行` 重绘，
    /// 这些字节是方向键历史回放 / Tab 补全后“当前行真实内容”的唯一可靠来源。
    pub fn on_output(&mut self, data: &[u8]) {
        if self.state != GuardState::Suspended || self.passthrough {
            return;
        }
        if self.sync_buf.len() + data.len() > SYNC_BUF_MAX {
            let keep = SYNC_BUF_MAX.saturating_sub(data.len());
            if self.sync_buf.len() > keep {
                self.sync_buf.drain(..self.sync_buf.len() - keep);
            }
        }
        self.sync_buf.extend_from_slice(data);
    }

    /// 处理一段输入字节（测试入口；生产路径使用 process_with_console_line）。
    #[cfg(test)]
    pub fn process(&mut self, data: &[u8]) -> GuardOutcome {
        self.process_impl(data, None)
    }

    /// 处理一段输入字节，并携带前端读取的“当前控制台行”（xterm 光标行，含
    /// readline 补全 / 历史 / 编辑后的完整命令）。有该行时，判定与弹窗展示以它
    /// 为准，彻底规避本地追踪 + 回显重建在补全等场景的偏差。
    pub fn process_with_console_line(
        &mut self,
        data: &[u8],
        console_line: Option<&str>,
    ) -> GuardOutcome {
        self.process_impl(data, console_line)
    }

    fn process_impl(&mut self, data: &[u8], console_line: Option<&str>) -> GuardOutcome {
        let mut out = GuardOutcome::default();
        if !self.config.enabled || self.passthrough {
            out.forward = data.to_vec();
            return out;
        }

        // Holding：扣住所有输入，直到审批解决；Ctrl-C 视为取消审批
        if self.state == GuardState::Holding {
            if data.contains(&0x03) {
                let r = self.cancel_by_ctrl_c();
                out.forward.extend_from_slice(&r.forward);
                out.audit = r.audit;
            } else {
                self.held_input.extend_from_slice(data);
            }
            return out;
        }

        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            match self.state {
                GuardState::Known => match b {
                    0x0d | 0x0a => {
                        // 行终止（远端 tty ICRNL 会把 CR 也当作行结束）；合并 \r\n
                        if b == 0x0d && i + 1 < data.len() && data[i + 1] == 0x0a {
                            i += 1;
                        }
                        self.handle_enter(&mut out, console_line);
                    }
                    0x1b => {
                        // ESC（方向键 / 功能键 / 组合键）→ 行内容可能被远端改动，
                        // **保留本地已输入内容**：补全/历史只会在此基础上追加，
                        // 危险片段通常在用户敲入的部分里；即使回显重绘未及时到达，
                        // Enter 时本地输入仍参与判定（时序兜底）。
                        // 清空回显同步缓冲，开始累积 readline 重绘的完整行；
                        // 置 esc_pending，避免后续 CSI 尾巴字节（[ A）漏进行缓冲。
                        self.state = GuardState::Suspended;
                        self.tab_again = false;
                        self.line_at_suspend = self.line.clone();
                        self.sync_buf.clear();
                        self.esc_pending = true;
                        out.forward.push(b);
                    }
                    0x09 => {
                        // Tab 补全 → 行内容可能被远端改动（同上），但 Tab 是单字节，
                        // 无后续序列，esc_pending 保持 false。
                        self.state = GuardState::Suspended;
                        self.tab_again = false;
                        self.line_at_suspend = self.line.clone();
                        self.sync_buf.clear();
                        out.forward.push(b);
                    }
                    0x7f | 0x08 => {
                        pop_char(&mut self.line);
                        out.forward.push(b);
                    }
                    0x15 => {
                        // Ctrl-U：清行
                        self.line.clear();
                        self.line_at_suspend.clear();
                        out.forward.push(b);
                    }
                    0x17 => {
                        // Ctrl-W：删最后一个词
                        pop_word(&mut self.line);
                        out.forward.push(b);
                    }
                    0x03 => {
                        // Ctrl-C：重置
                        self.line.clear();
                        self.line_at_suspend.clear();
                        self.accumulated.clear();
                        out.forward.push(b);
                    }
                    // 其他控制键（Ctrl-A/E/K/Y、Delete 等）可能改动行 → Suspended
                    b if b < 0x20 || b == 0x7f => {
                        self.state = GuardState::Suspended;
                        self.tab_again = false;
                        self.line_at_suspend = self.line.clone();
                        self.line.clear();
                        out.forward.push(b);
                    }
                    _ => {
                        self.line.push(b);
                        out.forward.push(b);
                    }
                },
                GuardState::Suspended => match b {
                    0x0d | 0x0a => {
                        if b == 0x0d && i + 1 < data.len() && data[i + 1] == 0x0a {
                            i += 1;
                        }
                        // 用回显重同步的行（提示符 + 历史命令）+ 本地追踪内容判定：
                        // 命中规则 → 弹窗（覆盖方向键历史回放 / Tab 补全 / 编辑键场景）
                        self.state = GuardState::Known;
                        self.handle_enter_suspended(&mut out, console_line);
                    }
                    0x03 => {
                        self.esc_pending = false;
                        self.sync_buf.clear();
                        self.line.clear();
                        self.line_at_suspend.clear();
                        self.accumulated.clear();
                        self.tab_again = false;
                        self.state = GuardState::Known;
                        out.forward.push(b);
                    }
                    0x09 => {
                        // 再次 Tab：readline 可能已把上次 Tab 后的输入替换/纠正
                        // （如 confcon → conf），标记旧 post 失效，避免把已被
                        // readline 处理的输入拼进弹窗命令。保留已有补全后缀回显，
                        // 后续重绘/追加会继续累积到同步缓冲。
                        self.tab_again = true;
                        out.forward.push(b);
                    }
                    0x1b => {
                        // 再次按下方向键 / 功能键 → 行又变了，重新累积重绘
                        self.sync_buf.clear();
                        self.esc_pending = true;
                        out.forward.push(b);
                    }
                    b if self.esc_pending => {
                        out.forward.push(b);
                        if b == 0x5b {
                            // CSI 序列：继续等参数 / 终结符
                        } else if (0x20..=0x3f).contains(&b) {
                            // CSI 参数 / 中间字节
                        } else {
                            // 终结符（0x40-0x7e）或 SS3 / Alt 组合的单个字节
                            self.esc_pending = false;
                        }
                    }
                    b if b < 0x20 || b == 0x7f => {
                        // 其他控制键：不追踪（行内容可能已变化）
                        out.forward.push(b);
                    }
                    _ => {
                        // 可打印字符：尽力追踪，Enter 时参与判定
                        self.line.push(b);
                        out.forward.push(b);
                    }
                },
                GuardState::Holding => {
                    self.held_input.push(b);
                }
            }
            i += 1;
        }
        out
    }

    fn handle_enter(&mut self, out: &mut GuardOutcome, console_line: Option<&str>) {
        // 前端提供当前控制台行（xterm 光标行，含 readline 补全后的最终命令）
        // → 直接以它为准，本地追踪仅作为无前端行时的兜底。
        let authoritative = console_line
            .map(extract_command_from_console_line)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let full: Vec<u8> = match &authoritative {
            Some(cmd) => cmd.as_bytes().to_vec(),
            None => {
                let mut f = self.accumulated.clone();
                f.extend_from_slice(&self.line);
                f
            }
        };
        let text = String::from_utf8_lossy(&full).to_string();
        let trimmed = text.trim_end();

        // 空行：放行，顺便清空陈旧缓冲
        if trimmed.is_empty() {
            out.forward.push(b'\r');
            self.line.clear();
            self.line_at_suspend.clear();
            self.accumulated.clear();
            return;
        }

        // 续行：整行去掉尾部空白后以 \ 结尾（无尾随空白）→ 放行 Enter，内容累积到下一行
        if authoritative.is_none() {
            if let Some(stripped) = trimmed.strip_suffix('\\') {
                out.forward.push(b'\r');
                self.accumulated.extend_from_slice(stripped.as_bytes());
                self.line.clear();
                self.line_at_suspend.clear();
                return;
            }
        }

        // 规则判定（子串匹配，大小写不敏感）
        let lower = text.to_ascii_lowercase();
        let matched: Vec<String> = self
            .config
            .patterns
            .iter()
            .filter(|p| lower.contains(p.as_str()))
            .cloned()
            .collect();

        if !matched.is_empty() {
            // 命中 → 扣住 Enter，进入 Holding
            self.state = GuardState::Holding;
            self.request_id = uuid::Uuid::new_v4().to_string();
            self.matched = matched.clone();
            self.held_input.clear();
            out.approval = Some(ApprovalRequest {
                request_id: self.request_id.clone(),
                command: text,
                matched_patterns: matched,
            });
        } else {
            // 安全 → 放行 Enter
            out.forward.push(b'\r');
            self.line.clear();
            self.line_at_suspend.clear();
            self.accumulated.clear();
        }
    }

    /// Suspended 状态下按 Enter：从回显同步缓冲提取 readline 重绘的最后一行
    /// （已剥离 ANSI），与本地追踪内容拼接后做规则判定。
    /// 提示符在行首、命令在行尾，子串匹配整行不会漏掉命令中的危险片段。
    /// 注意回显的最后一段可能是**补全候选列表**（readline 显示候选后 prompt 行
    /// 未及时重绘），因此弹窗展示优先用干净的本地输入，避免候选列表混入显示。
    fn handle_enter_suspended(&mut self, out: &mut GuardOutcome, console_line: Option<&str>) {
        // 前端提供了当前控制台行（权威，含 readline 补全 / 历史 / 编辑后的最终命令）
        // → 直接以它判定与展示，不再依赖回显重建。
        if let Some(cmd) = console_line
            .map(extract_command_from_console_line)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            let display = cmd.clone();
            self.sync_buf.clear();
            let lower = display.to_ascii_lowercase();
            let matched: Vec<String> = self
                .config
                .patterns
                .iter()
                .filter(|p| lower.contains(p.as_str()))
                .cloned()
                .collect();
            if !matched.is_empty() {
                self.state = GuardState::Holding;
                self.request_id = uuid::Uuid::new_v4().to_string();
                self.matched = matched.clone();
                self.held_input.clear();
                out.approval = Some(ApprovalRequest {
                    request_id: self.request_id.clone(),
                    command: display,
                    matched_patterns: matched,
                });
            } else {
                out.forward.push(b'\r');
                self.line.clear();
                self.line_at_suspend.clear();
                self.accumulated.clear();
                self.tab_again = false;
            }
            return;
        }

        let sync_line = extract_last_line(&self.sync_buf);
        let tracked = String::from_utf8_lossy(&self.line).to_string();
        // 判定用拼接（候选列表混入只导致多拦，方向安全）
        let text = format!("{sync_line}{tracked}");
        let trimmed = text.trim().to_string();
        // 展示：以本地输入为准；仅当回显行（去提示符后）以本地输入开头
        // （readline 已把补全后的完整命令重绘出来）时展示完整行，
        // 否则回显可能是候选列表 / 延迟回显拼接碎片，不得混入弹窗。
        let display = display_command(
            &sync_line,
            &self.line_at_suspend,
            &self.line,
            !self.tab_again,
        );
        self.sync_buf.clear();

        if trimmed.is_empty() {
            out.forward.push(b'\r');
            self.line.clear();
            self.line_at_suspend.clear();
            self.accumulated.clear();
            return;
        }

        let lower = trimmed.to_ascii_lowercase();
        let matched: Vec<String> = self
            .config
            .patterns
            .iter()
            .filter(|p| lower.contains(p.as_str()))
            .cloned()
            .collect();

        if !matched.is_empty() {
            self.state = GuardState::Holding;
            self.request_id = uuid::Uuid::new_v4().to_string();
            self.matched = matched.clone();
            self.held_input.clear();
            out.approval = Some(ApprovalRequest {
                request_id: self.request_id.clone(),
                command: display,
                matched_patterns: matched,
            });
        } else {
            out.forward.push(b'\r');
            self.line.clear();
            self.line_at_suspend.clear();
            self.accumulated.clear();
            self.tab_again = false;
        }
    }

    /// 处理审批结果：批准放行 Enter，拒绝发 Ctrl-U 清行；随后转发 Holding 期间扣住的输入。
    pub fn resolve(&mut self, request_id: &str, allow: bool, timed_out: bool) -> GuardOutcome {
        let mut out = GuardOutcome::default();
        if self.state != GuardState::Holding || self.request_id != request_id {
            return out;
        }
        let command = self.current_command();
        let matched = std::mem::take(&mut self.matched);
        if allow {
            out.forward.push(b'\r');
        } else {
            out.forward.push(0x15);
        }
        out.forward.extend_from_slice(&self.held_input);
        self.line.clear();
        self.line_at_suspend.clear();
        self.accumulated.clear();
        self.sync_buf.clear();
        self.held_input.clear();
        self.tab_again = false;
        self.state = GuardState::Known;
        out.audit = Some(AuditEvent {
            command,
            matched_patterns: matched,
            approved: allow,
            timed_out,
        });
        out
    }

    /// Holding 状态下收到 Ctrl-C：视为取消审批，转发 Ctrl-C 并清行。
    fn cancel_by_ctrl_c(&mut self) -> GuardOutcome {
        let mut out = GuardOutcome::default();
        if self.state != GuardState::Holding {
            return out;
        }
        let command = self.current_command();
        let matched = std::mem::take(&mut self.matched);
        out.forward.push(0x03);
        out.forward.extend_from_slice(&self.held_input);
        self.line.clear();
        self.line_at_suspend.clear();
        self.accumulated.clear();
        self.sync_buf.clear();
        self.held_input.clear();
        self.state = GuardState::Known;
        out.audit = Some(AuditEvent {
            command,
            matched_patterns: matched,
            approved: false,
            timed_out: false,
        });
        out
    }

    fn current_command(&self) -> String {
        let mut full: Vec<u8> = self.accumulated.clone();
        full.extend_from_slice(&self.line);
        String::from_utf8_lossy(&full).to_string()
    }
}

/// 从前端 xterm 光标行（含提示符，如 `root@host:~# cat docker-compose.yml`）
/// 提取命令部分：取第一个提示符标记（# $ % >）之后的文本。命令中的同类字符
/// 通常出现在提示符之后，首个标记即提示符结尾。
fn extract_command_from_console_line(line: &str) -> String {
    let t = line.trim();
    if let Some(pos) = t.find(['#', '$', '%', '>']) {
        let rest = t[pos + 1..].trim_start();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    t.to_string()
}

/// 弹窗展示文本：用“Suspended 时的本地输入快照 + 回显中的补全后缀 +
/// Tab 后继续输入的字节”重建补全后的完整命令。回显可能被延迟/分片污染
/// （真实 bash 补全后缀直接追加、无 `\r` 重绘，延迟回显会拼出
/// `cat gtail.conf` / `gtail.confcat locon` 等碎片），恢复顺序：
/// - 回显行包含本地输入快照 → 后缀 = 其后内容（readline 重绘完整行）；
/// - 回显行以快照的前缀开头（回显分片丢了部分字符）→ 后缀 = 其后内容；
/// - 回显行以快照末词开头（重绘了整个词）→ 后缀 = 其后内容；
/// - 回显行是单个词（bash 补全后缀直接追加）→ 后缀 = 整行；
/// - 回显行带提示符（历史回放 / 编辑后的完整重绘）→ 用回显行；
/// - 其余（候选列表等不可信内容）→ 用当前本地输入。
///
/// 后缀拼到快照后，再追加 Tab 后继续输入的字节（如 `con`）。
fn display_command(
    sync_line: &str,
    line_at_suspend: &[u8],
    line: &[u8],
    post_valid: bool,
) -> String {
    let t = String::from_utf8_lossy(line_at_suspend).trim().to_string();
    if t.is_empty() {
        let s = sync_line.trim();
        if !s.is_empty() {
            return s.to_string();
        }
        return String::from_utf8_lossy(line).trim().to_string();
    }

    let mut sync_raw = sync_line.trim();
    if sync_raw.is_empty() {
        return String::from_utf8_lossy(line).trim().to_string();
    }
    // 去掉尾部“延迟回显”的本地输入（回显 = [后缀/碎片] + 延迟回显的整行）
    let line_str = String::from_utf8_lossy(line);
    let line_trim = line_str.trim();
    if !line_trim.is_empty() && sync_raw.ends_with(line_trim) && sync_raw.len() > line_trim.len() {
        sync_raw = sync_raw[..sync_raw.len() - line_trim.len()].trim_end();
    } else if sync_raw.ends_with(&t) && sync_raw.len() > t.len() {
        sync_raw = sync_raw[..sync_raw.len() - t.len()].trim_end();
    }
    match find_completion_suffix(sync_raw, &t) {
        Some(suffix) => {
            let mut out = format!("{t}{suffix}");
            // Tab 后继续输入的字节（如 `con`）；后缀已含则不再追加，避免重复。
            // 若 Suspended 后又按过 Tab（post_valid=false），readline 可能已替换这些
            // 输入，直接丢弃，避免把过期输入拼进弹窗。
            let post = if post_valid && line_str.len() >= t.len() && line_str.starts_with(&t) {
                &line_str[t.len()..]
            } else {
                ""
            };
            let post = post.trim();
            if !post.is_empty() && !out.ends_with(post) {
                out.push_str(post);
            }
            out
        }
        None => {
            // 回显带提示符 → readline 完整重绘（历史回放 / 编辑）→ 用回显行
            if sync_raw.contains(['#', '$', '%', '>']) {
                sync_raw.to_string()
            } else {
                line_str.trim().to_string()
            }
        }
    }
}

/// 从回显行中提取补全后缀（见 display_command 的恢复顺序 1-4）。
fn find_completion_suffix(sync_raw: &str, t: &str) -> Option<String> {
    // 1. 包含本地输入快照 → 后缀在其后（readline 重绘了完整行）
    if let Some(pos) = sync_raw.find(t) {
        let s = sync_raw[pos + t.len()..].trim();
        if is_plausible_suffix(s) {
            return Some(s.to_string());
        }
    }
    // 2. 以快照的前缀开头（回显分片丢了部分字符，如 `cat gtail.conf`）
    let cplen = common_prefix_len(sync_raw, t);
    if cplen > 0 {
        let s = sync_raw[cplen..].trim();
        if is_plausible_suffix(s) {
            return Some(s.to_string());
        }
    }
    // 3. 以快照末词开头（readline 重绘了整个词）
    if let Some(t_last) = last_word(t) {
        if sync_raw.len() > t_last.len() && sync_raw.starts_with(t_last) {
            let s = sync_raw[t_last.len()..].trim();
            if is_plausible_suffix(s) {
                return Some(s.to_string());
            }
        }
    }
    // 4. 单个词且不是本地输入的一部分（bash 补全后缀直接追加）
    if is_single_token(sync_raw) && is_plausible_suffix(sync_raw) && !t.contains(sync_raw) {
        return Some(sync_raw.to_string());
    }
    None
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes()
        .zip(b.bytes())
        .take_while(|(x, y)| x == y)
        .count()
}

fn last_word(s: &str) -> Option<&str> {
    s.split_whitespace().next_back()
}

fn is_single_token(s: &str) -> bool {
    !s.is_empty() && !s.contains(char::is_whitespace)
}

/// 后缀必须是无控制字符、且含至少一个字母/数字的“词”，
/// 避免铃声（\x07）、纯符号等被当成补全后缀拼进弹窗。
fn is_plausible_suffix(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b >= 0x20 && b != 0x7f)
        && s.bytes().any(|b| b.is_ascii_alphanumeric())
}

/// 从回显同步缓冲中提取最后一行（最后一个 \r / \n 之后的内容），并剥离 ANSI 序列。
/// readline 重绘格式为 `\r + 提示符 + 命令`，最后一段即当前行的完整内容（含提示符）。
fn extract_last_line(buf: &[u8]) -> String {
    let text = strip_ansi(&String::from_utf8_lossy(buf));
    let idx = text.rfind(['\r', '\n']).map(|i| i + 1).unwrap_or(0);
    // readline 补全/响铃会在行首输出 BEL（\x07）等控制字节；控制字符不影响
    // 命令内容，却会破坏补全后缀的匹配（is_plausible_suffix 拒绝含控制字符的串），
    // 统一剥离后再参与后缀重建与弹窗展示。
    text[idx..]
        .chars()
        .filter(|c| *c >= ' ' && *c != '\x7f')
        .collect::<String>()
        .trim()
        .to_string()
}

/// 剥离 ANSI 转义序列（CSI / OSC / 字符集选择 / 单字节序列），保留可见文本。
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut cleaned: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            if i + 1 >= bytes.len() {
                break;
            }
            match bytes[i + 1] {
                0x5b => {
                    // CSI: ESC [ 参数(0x20-0x3f) 中间(0x20-0x2f) 终结符(0x40-0x7e)
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                }
                0x5d => {
                    // OSC: ESC ] ... BEL(0x07)
                    let mut j = i + 2;
                    while j < bytes.len() && bytes[j] != 0x07 {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                }
                0x28..=0x2b => {
                    // 字符集选择: ESC ( B
                    i = (i + 3).min(bytes.len());
                }
                _ => {
                    // 单字节序列 / SS3 等
                    i = (i + 2).min(bytes.len());
                }
            }
        } else {
            cleaned.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&cleaned).into_owned()
}

/// 从行尾弹出一个字符（UTF-8 感知：多字节字符整体弹出；不完整序列按字节弹出）。
fn pop_char(buf: &mut Vec<u8>) {
    if let Some(&last) = buf.last() {
        if last & 0xC0 != 0x80 {
            // ASCII 或 UTF-8 起始字节（可能不完整）→ 弹 1 字节
            buf.pop();
        } else {
            // 连续字节 → 回退到起始字节
            let mut n = 1;
            while n < buf.len() && buf[buf.len() - 1 - n] & 0xC0 == 0x80 {
                n += 1;
            }
            buf.truncate(buf.len() - n);
        }
    }
}

/// 删除最后一个空白分隔的词（近似 readline 的 Ctrl-W）。
fn pop_word(buf: &mut Vec<u8>) {
    while matches!(buf.last(), Some(b) if *b == b' ' || *b == b'\t') {
        buf.pop();
    }
    while let Some(&b) = buf.last() {
        if b == b' ' || b == b'\t' {
            break;
        }
        pop_char(buf);
    }
}

// ---------- 审批事件 ----------

#[derive(Clone, Serialize)]
pub struct TerminalGuardApproval {
    pub session_id: u32,
    pub request_id: String,
    pub host_label: String,
    pub command: String,
    pub matched_patterns: Vec<String>,
}

// ---------- Tauri 命令 ----------

#[tauri::command]
pub fn get_terminal_guard_settings(db: State<'_, Arc<Db>>) -> Result<TerminalGuardSettings, String> {
    db.get_terminal_guard_settings()
        .map_err(|e| format!("读取终端防护配置失败: {e}"))
}

#[tauri::command]
pub fn save_terminal_guard_settings(
    db: State<'_, Arc<Db>>,
    settings: TerminalGuardSettings,
) -> Result<TerminalGuardSettings, String> {
    db.save_terminal_guard_settings(&settings)
        .map_err(|e| format!("保存终端防护配置失败: {e}"))?;
    Ok(settings)
}

#[tauri::command]
pub fn list_terminal_rules(db: State<'_, Arc<Db>>) -> Result<Vec<TerminalRule>, String> {
    db.list_terminal_rules()
        .map_err(|e| format!("读取终端防护规则失败: {e}"))
}

#[tauri::command]
pub fn add_terminal_rule(db: State<'_, Arc<Db>>, pattern: String) -> Result<TerminalRule, String> {
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return Err("危险命令不能为空".to_string());
    }
    let rule = TerminalRule {
        id: uuid::Uuid::new_v4().to_string(),
        pattern,
        enabled: true,
        builtin: false,
        created_at: now(),
    };
    db.insert_terminal_rule(&rule)
        .map_err(|e| format!("保存规则失败: {e}"))?;
    Ok(rule)
}

#[tauri::command]
pub fn delete_terminal_rule(db: State<'_, Arc<Db>>, id: String) -> Result<(), String> {
    db.delete_terminal_rule(&id)
        .map_err(|e| format!("删除规则失败: {e}"))
}

#[tauri::command]
pub fn reset_terminal_rules(db: State<'_, Arc<Db>>) -> Result<Vec<TerminalRule>, String> {
    db.reset_terminal_rules()
        .map_err(|e| format!("恢复预置规则失败: {e}"))
}

#[tauri::command]
pub fn session_guard_approve(
    app: AppHandle,
    state: State<'_, crate::session::SessionManager>,
    session_id: u32,
    request_id: String,
    allow: bool,
) -> Result<(), String> {
    state.resolve_approval(&app, session_id, &request_id, allow, false)
}

/// 写入一条终端命令拦截审计。
pub(crate) fn write_guard_audit(
    app: &AppHandle,
    session_id: u32,
    host_id: &str,
    host_label: &str,
    event: &AuditEvent,
) {
    let db = match app.try_state::<Arc<Db>>() {
        Some(db) => db,
        None => return,
    };
    let approval = if event.timed_out {
        "timeout"
    } else if event.approved {
        "approved"
    } else {
        "denied"
    };
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: Some(session_id),
        host_id: host_id.to_string(),
        host_label: host_label.to_string(),
        tool_name: "terminal_command".to_string(),
        summary: truncate(&sanitize(&event.command), 500),
        permission_mode: "guard".to_string(),
        approval: approval.to_string(),
        status: "ok".to_string(),
        result: if event.matched_patterns.is_empty() {
            None
        } else {
            Some(event.matched_patterns.join(", "))
        },
        duration_ms: None,
    };
    let _ = db.insert_audit_log(&log);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(patterns: &[&str]) -> GuardConfig {
        GuardConfig {
            enabled: true,
            timeout_secs: 60,
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn forward(out: &GuardOutcome) -> String {
        String::from_utf8_lossy(&out.forward).to_string()
    }

    #[test]
    fn safe_line_forwards_enter() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let out = g.process(b"ls -la\r");
        assert_eq!(forward(&out), "ls -la\r");
        assert!(out.approval.is_none());
        assert!(!g.is_holding());
    }

    #[test]
    fn dangerous_line_holds_enter() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let out = g.process(b"rm -rf /");
        assert_eq!(forward(&out), "rm -rf /");
        let out = g.process(b"\r");
        assert!(out.forward.is_empty(), "Enter 不应被转发");
        let approval = out.approval.unwrap();
        assert_eq!(approval.command, "rm -rf /");
        assert_eq!(approval.matched_patterns, vec!["rm -rf".to_string()]);
        assert!(g.is_holding());
    }

    #[test]
    fn approve_forwards_enter_and_audits() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf /\r");
        let out = g.resolve("", true, false);
        // request_id 不匹配 → 不动作
        assert!(out.forward.is_empty());
        assert!(out.audit.is_none());

        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let first = g.process(b"rm -rf /\r");
        let req = first.approval.unwrap();
        let out = g.resolve(&req.request_id, true, false);
        assert_eq!(forward(&out), "\r");
        let audit = out.audit.unwrap();
        assert!(audit.approved);
        assert_eq!(audit.command, "rm -rf /");
        assert!(!g.is_holding());
    }

    #[test]
    fn deny_sends_ctrl_u_and_audits() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let first = g.process(b"rm -rf /\r");
        let req = first.approval.unwrap();
        let out = g.resolve(&req.request_id, false, false);
        assert_eq!(out.forward, vec![0x15]);
        let audit = out.audit.unwrap();
        assert!(!audit.approved);
        assert_eq!(audit.command, "rm -rf /");
    }

    #[test]
    fn holding_buffers_input_and_flushes_after_resolve() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let first = g.process(b"rm -rf /\r");
        let req = first.approval.unwrap();
        // 扣住期间的输入
        let held = g.process(b"echo hi\r");
        assert!(held.forward.is_empty());
        let out = g.resolve(&req.request_id, true, false);
        assert_eq!(forward(&out), "\recho hi\r");
    }

    #[test]
    fn ctrl_c_during_holding_cancels() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let first = g.process(b"rm -rf /\r");
        let req = first.approval.unwrap();
        let out = g.process(&[0x03]);
        assert_eq!(out.forward, vec![0x03]);
        assert!(out.audit.unwrap().approved == false);
        assert!(!g.is_holding());
        assert!(g.resolve(&req.request_id, true, false).forward.is_empty());
    }

    #[test]
    fn backspace_removes_char() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 退格三次后行变为 "rm -r"（不含 "rm -rf" 模式）
        let out = g.process(b"rm -rf /\x7f\x7f\x7f");
        assert_eq!(forward(&out), "rm -rf /\x7f\x7f\x7f");
        let out2 = g.process(b"\r");
        assert!(out2.approval.is_none(), "退格后不应命中");
    }

    #[test]
    fn ctrl_u_clears_line() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf /\x15");
        let out = g.process(b"\r");
        assert!(out.approval.is_none());
    }

    #[test]
    fn continuation_merges_lines_and_catches_dangerous() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // `rm \` 续行
        let out = g.process(b"rm \\\r");
        assert!(out.approval.is_none(), "续行不应触发判定");
        assert_eq!(forward(&out), "rm \\\r");
        // 下一行拼成 rm -rf / → 命中（可打印字符照常转发，仅 Enter 被扣住）
        let out2 = g.process(b"-rf /\r");
        assert_eq!(forward(&out2), "-rf /");
        assert_eq!(out2.approval.unwrap().command, "rm -rf /");
    }

    #[test]
    fn arrow_key_esc_sequence_not_tracked() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 方向键（ESC 序列）→ Suspended，序列尾巴不污染行缓冲
        let _ = g.process(b"rm -r");
        let suspended = g.process(b"\x1b[A");
        assert!(suspended.approval.is_none());
        // Suspended 期间输入安全命令 → 放行，行内容不受 ESC 尾巴污染
        let out = g.process(b"ls\r");
        assert!(out.approval.is_none());
        assert_eq!(forward(&out), "ls\r");
    }

    #[test]
    fn suspended_typed_dangerous_line_still_blocks() {
        // Ctrl-L 清屏（0x0c）进入 Suspended 后输入危险命令：
        // 追踪到的内容仍参与判定，Enter 时弹窗，避免“输几次后失效”
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"\x0c");
        let out = g.process(b"rm -rf /\r");
        assert_eq!(forward(&out), "rm -rf /");
        let approval = out.approval.unwrap();
        assert_eq!(approval.command, "rm -rf /");
        assert!(g.is_holding());
    }

    #[test]
    fn suspended_safe_line_allows() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"\x0c"); // Ctrl-L → Suspended
        let out = g.process(b"ls -la\r");
        assert!(out.approval.is_none());
        assert_eq!(forward(&out), "ls -la\r");
    }

    #[test]
    fn tab_then_typed_dangerous_line_blocks() {
        // Tab 补全后行内容不可信（Suspended），但之后敲入的内容仍参与判定
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -f\t");
        let out = g.process(b"rm -rf /\r");
        assert!(out.approval.is_some());
    }

    #[test]
    fn arrow_history_replay_blocked_via_echo_sync() {
        // ↑ 回放历史高危命令：readline 重绘“提示符 + 命令”到回显，Enter 时提取判定
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"\x1b[A");
        g.on_output(b"\r\x1b[32mroot@host\x1b[0m:~# rm -rf /");
        let out = g.process(b"\r");
        assert!(out.approval.is_some(), "历史回放的 rm -rf / 应被拦截");
        let approval = out.approval.unwrap();
        assert!(approval.command.contains("rm -rf /"), "command={}", approval.command);
        assert_eq!(approval.matched_patterns, vec!["rm -rf".to_string()]);
    }

    #[test]
    fn arrow_replay_safe_line_allows() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"\x1b[B");
        g.on_output(b"\rroot@host:~# ls -la");
        let out = g.process(b"\r");
        assert!(out.approval.is_none());
        assert_eq!(forward(&out), "\r");
    }

    #[test]
    fn multi_arrow_replay_takes_latest_echo() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"\x1b[A"); // ↑ 第一次
        g.on_output(b"\rroot@host:~# ls -la");
        let _ = g.process(b"\x1b[A"); // ↑ 第二次 → 清空同步缓冲
        g.on_output(b"\rroot@host:~# rm -rf /");
        let out = g.process(b"\r");
        assert!(out.approval.is_some(), "应取最新一次重绘的行");
    }

    #[test]
    fn tab_completion_blocked_via_echo_sync() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf");
        let _ = g.process(b"\t"); // Tab 补全 → Suspended
        g.on_output(b"\rroot@host:~# rm -rf /tmp");
        let out = g.process(b"\r");
        assert!(out.approval.is_some(), "补全后的完整行应参与判定");
    }

    #[test]
    fn tab_completion_without_echo_still_blocks() {
        // 回显未及时到达（用户 Tab 后立刻 Enter）时，用 Tab 前的本地输入兜底判定
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf /tm");
        let _ = g.process(b"\t"); // Tab → Suspended，本地行保留
        let out = g.process(b"\r"); // 无任何回显
        let approval = out.approval.expect("Tab 前的输入应参与判定");
        assert!(approval.command.contains("rm -rf"));
    }

    #[test]
    fn tab_then_more_typing_still_blocks() {
        // Tab 后继续输入，本地追踪 + 回显双路都参与判定
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf /tm");
        let _ = g.process(b"\t");
        g.on_output(b"\rroot@host:~# rm -rf /tmp/");
        let _ = g.process(b"x");
        let out = g.process(b"\r");
        assert!(out.approval.is_some());
    }

    #[test]
    fn echo_sync_only_active_when_suspended() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 非 Suspended 时不累积回显（不影响正常输入判定）
        g.on_output(b"\rroot@host:~# rm -rf /");
        let out = g.process(b"ls\r");
        assert!(out.approval.is_none());
        let out2 = g.process(b"rm -rf /\r");
        assert!(out2.approval.is_some());
    }

    #[test]
    fn candidate_list_not_shown_in_approval() {
        // 用户场景：配置 `cat` 规则，输入 `cat lo` + Tab，回显最后一段是
        // 补全候选列表（`gtail.conf` 等）而非 prompt 行 → 弹窗必须展示干净的本地输入
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"\r\ngtail.conf  localtime.conf");
        let _ = g.process(b"con");
        let out = g.process(b"\r");
        let approval = out.approval.expect("拼接判定应命中 cat 规则");
        assert!(
            !approval.command.contains("gtail.conf"),
            "候选列表不得混入展示: {}",
            approval.command
        );
        assert_eq!(approval.command, "cat locon");
    }

    #[test]
    fn completed_line_shown_in_approval() {
        // 补全后的完整行已重绘（`rm -rf /tmp/`）且本地输入是其前缀 → 展示完整行
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let _ = g.process(b"rm -rf /tm\t");
        g.on_output(b"\rroot@host:~# rm -rf /tmp/");
        let out = g.process(b"\r");
        let approval = out.approval.expect("应命中 rm -rf 规则");
        assert_eq!(approval.command, "rm -rf /tmp/");
    }

    #[test]
    fn candidate_suffix_does_not_leak_into_display() {
        // 回显是补全后缀 `gtail.conf`（单候选直接补全），Tab 后输入 `con`：
        // 重建为远端真实命令 `cat logtail.confcon`，不得丢失后缀或串位。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"\rgtail.conf");
        let _ = g.process(b"con");
        let out = g.process(b"\r");
        let approval = out.approval.expect("应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.confcon");
    }

    #[test]
    fn strip_ansi_removes_sequences() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi("\x1b]0;title\x07hi"), "hi");
        assert_eq!(strip_ansi("\x1b[Kplain"), "plain");
        assert_eq!(strip_ansi("中文\x1b[1;31m红"), "中文红");
    }

    #[test]
    fn extract_last_line_ignores_previous_output() {
        let buf = b"$ uptime\r\n 12:34 up 5 days\r\nroot@host:~# rm -rf /";
        assert_eq!(extract_last_line(buf), "root@host:~# rm -rf /");
    }

    #[test]
    fn passthrough_idempotent_keeps_line() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 每次按键都会以相同值调用 set_passthrough，不得清空正在输入的行
        let _ = g.process(b"rm -rf ");
        g.set_passthrough(false);
        g.set_passthrough(false);
        let out = g.process(b"/\r");
        assert!(out.approval.is_some(), "行缓冲应保留，仍能命中");
    }

    #[test]
    fn crlf_paste_judges_each_line() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let out = g.process(b"ls\r\nrm -rf /\r\n");
        // 第一行可打印字符 + Enter 转发；第二行可打印字符转发，但 Enter 被扣住
        assert_eq!(forward(&out), "ls\rrm -rf /");
        let approval = out.approval.unwrap();
        assert_eq!(approval.command, "rm -rf /");
        assert!(g.is_holding());
    }

    #[test]
    fn disabled_guard_passes_through() {
        let mut cfg = config(&["rm -rf"]);
        cfg.enabled = false;
        let mut g = GuardEngine::new(cfg);
        let out = g.process(b"rm -rf /\r");
        assert_eq!(forward(&out), "rm -rf /\r");
        assert!(out.approval.is_none());
    }

    #[test]
    fn passthrough_forwards_without_judging() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        g.set_passthrough(true);
        assert!(g.is_passthrough());
        // vim 插入模式下把 rm -rf / 当文本输入，不应拦截
        let out = g.process(b"rm -rf /\r");
        assert_eq!(forward(&out), "rm -rf /\r");
        assert!(out.approval.is_none());
        // 退出透传后恢复判定
        g.set_passthrough(false);
        let first = g.process(b"rm -rf /\r");
        assert!(first.approval.is_some());
    }

    #[test]
    fn passthrough_clears_holding() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        let first = g.process(b"rm -rf /\r");
        let req = first.approval.unwrap();
        g.set_passthrough(true);
        assert!(!g.is_holding());
        assert!(g.resolve(&req.request_id, true, false).forward.is_empty());
    }

    #[test]
    fn stale_buffer_only_false_positives_and_recovers() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 陈旧缓冲（例如 vim 内 :!rm -rf / 残留，未触发 reset）
        let _ = g.process(b":!rm -rf /");
        // 行非空时按 Enter 会误报为危险（陈旧缓冲只产生误报、不产生漏报）
        let out = g.process(b"\r");
        assert!(out.approval.is_some());
        // 拒绝后行被清空，后续安全命令不再被污染
        let req = out.approval.unwrap();
        let _ = g.resolve(&req.request_id, false, false);
        let out2 = g.process(b"ls\r");
        assert!(out2.approval.is_none());
    }

    #[test]
    fn empty_line_enter_clears_accumulated() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 续行累积
        let _ = g.process(b"rm \\\r");
        // 空行 Enter → 放行并清空累积，后续不再拼成危险命令
        let out = g.process(b"\r");
        assert!(out.approval.is_none());
        let out2 = g.process(b"-rf /\r");
        assert!(out2.approval.is_none(), "累积已清空，不应命中");
    }

    #[test]
    fn corrupted_echo_recovers_completion() {
        // 用户场景：cat lo + Tab。回显被延迟/分片污染成 `cat gtail.conf`（lo 丢失），
        // 弹窗必须重建出补全后的完整命令 `cat logtail.conf`。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"cat gtail.conf ");
        let out = g.process(b"\r");
        let approval = out.approval.expect("应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.conf", "display={}", approval.command);
    }

    #[test]
    fn delayed_echo_recovers_completion() {
        // 真实 bash 行为：补全后缀直接追加（无 \r 重绘）+ 延迟回显拼接成
        // `gtail.confcat lo` —— 重建时去掉尾部延迟回显，仍恢复 `cat logtail.conf`。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"gtail.conf ");
        g.on_output(b"cat lo");
        let out = g.process(b"\r");
        let approval = out.approval.expect("拼接判定应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.conf", "display={}", approval.command);
    }

    #[test]
    fn suffix_only_echo_recovers_completion() {
        // bash 把补全后缀直接追加、无 \r 重绘：本地输入回显在 Tab 前到达被忽略，
        // 回显缓冲只有后缀 `gtail.conf` → 重建为 `cat logtail.conf`。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"gtail.conf ");
        let out = g.process(b"\r");
        let approval = out.approval.expect("应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.conf", "display={}", approval.command);
    }

    #[test]
    fn typing_after_tab_appends_to_completed_line() {
        // cat lo + Tab 补全成 cat logtail.conf，随后继续输入 con：
        // 弹窗应显示远端真实命令 `cat logtail.confcon`（补全后缀在 con 之前）。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"gtail.conf ");
        let _ = g.process(b"con");
        let out = g.process(b"\r");
        let approval = out.approval.expect("应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.confcon", "display={}", approval.command);
        // 候选列表（多词）不得混入弹窗
        let mut g2 = GuardEngine::new(config(&["cat"]));
        let _ = g2.process(b"cat lo\t");
        g2.on_output(b"\r\ngtail.conf  localtime.conf");
        let _ = g2.process(b"con");
        let out2 = g2.process(b"\r");
        let a2 = out2.approval.expect("应命中 cat 规则");
        assert_eq!(a2.command, "cat locon", "display={}", a2.command);
    }

    #[test]
    fn utf8_line_reconstruction() {
        let mut g = GuardEngine::new(config(&["rm -rf"]));
        // 中文 + 退格（多字节字符整体弹出）
        let _ = g.process("echo 你好".as_bytes());
        let _ = g.process(&[0x7f, 0x7f]);
        let out = g.process(b"\r");
        assert!(out.approval.is_none());
    }

    #[test]
    fn second_tab_drops_replaced_post() {
        // 用户场景：cat lo + Tab 补全成 logtail.conf；输入 con；再 Tab 时 readline
        // 把 confcon 替换/纠正回 conf（最终行 cat logtail.conf）。本地追踪的 con
        // 属于“已被 readline 替换的输入”，弹窗不得再拼接它。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t"); // 第一次 Tab → Suspended
        g.on_output(b"\x07gtail.conf "); // 第一次补全后缀回显
        let _ = g.process(b"con"); // Tab 后输入 con（本地追踪）
        let _ = g.process(b"\t"); // 第二次 Tab → readline 替换，旧 post 失效
        // 第二次补全的回显未到达（或到达较晚），Enter 时不得拼入 con
        let out = g.process(b"\r");
        let approval = out.approval.expect("拼接判定应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.conf", "display={}", approval.command);
    }

    #[test]
    fn bel_prefix_in_echo_does_not_break_completion_suffix() {
        // 用户场景：cat lo + Tab，readline 在补全后缀前输出响铃 BEL（\x07），
        // 回显 = "\x07gtail.conf" → 弹窗必须恢复补全后的完整命令 "cat logtail.conf"，
        // 不得因 BEL 破坏后缀匹配而退化成本地输入 "cat lo..."。
        let mut g = GuardEngine::new(config(&["cat"]));
        let _ = g.process(b"cat lo\t");
        g.on_output(b"\x07gtail.conf ");
        let out = g.process(b"\r");
        let approval = out.approval.expect("拼接判定应命中 cat 规则");
        assert_eq!(approval.command, "cat logtail.conf", "display={}", approval.command);
    }
}
