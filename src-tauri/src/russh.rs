use crate::models::{AuthType, Host};
use russh::client::{
    self, AuthResult, Config, Handle, KeyboardInteractiveAuthResponse,
};
use russh::keys::{self, key::PrivateKeyWithHashAlg, HashAlg};
#[cfg(unix)]
use russh::keys::agent::{client::AgentClient, AgentIdentity};
use russh::{Channel, ChannelMsg, MethodKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Emitter, Manager};

pub struct ExecResult {
    /// stdout / stderr 按到达顺序合并的文本（兼容现有调用方）
    pub text: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<u32>,
    pub timed_out: bool,
}

// ---------- 首次连接主机密钥确认 ----------

/// 全局 AppHandle（在 lib.rs setup 中初始化），供非 Tauri 上下文（SSH handler）发事件。
static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
pub fn init_app_handle(app: &tauri::AppHandle) {
    let _ = APP_HANDLE.set(app.clone());
}

/// 主机密钥确认的等待结果：std Mutex 存决策（同步读写），Notify 唤醒等待中的连接。
#[derive(Default)]
struct HostKeyDecision {
    decision: Mutex<Option<bool>>,
    notify: tokio::sync::Notify,
}

impl HostKeyDecision {
    fn set(&self, v: bool) {
        *self.decision.lock().unwrap() = Some(v);
        self.notify.notify_waiters();
    }
    fn get(&self) -> Option<bool> {
        *self.decision.lock().unwrap()
    }
}

/// 首次连接（TOFU）弹窗确认注册表：key = address:port。
/// 并发连接同一新主机时共享同一个决策，避免重复弹窗；
/// 用户确认/拒绝/超时后所有等待者得到同一结论。
#[derive(Default)]
pub struct HostKeyRegistry {
    pending: Mutex<HashMap<String, Arc<HostKeyDecision>>>,
}

impl HostKeyRegistry {
    /// 注册（或复用）一个待确认项。返回 (决策句柄, 是否首次注册)。
    fn register(&self, key: &str) -> (Arc<HostKeyDecision>, bool) {
        let mut map = self.pending.lock().unwrap();
        if let Some(d) = map.get(key) {
            return (d.clone(), false);
        }
        let d = Arc::new(HostKeyDecision::default());
        map.insert(key.to_string(), d.clone());
        (d, true)
    }

    /// 用户决策：信任 → 记录 known_hosts；拒绝 → 拒绝连接。
    pub fn resolve(&self, key: &str, trust: bool) -> Result<(), String> {
        let decision = self
            .pending
            .lock()
            .unwrap()
            .remove(key)
            .ok_or_else(|| "确认请求不存在或已超时".to_string())?;
        decision.set(trust);
        Ok(())
    }

    /// 移除一个待确认项（超时清理）。
    fn remove(&self, key: &str) {
        self.pending.lock().unwrap().remove(key);
    }
}

static APP_REGISTRY: std::sync::OnceLock<HostKeyRegistry> = std::sync::OnceLock::new();

fn host_key_id(address: &str, port: u16) -> String {
    format!("{address}:{port}")
}

/// 等待用户对未知主机密钥的确认（超时按拒绝处理）。
/// 与各连接入口的外层超时统一为 60s，保证「弹窗倒计时 = 后端确认等待 =
/// 外层连接超时」，避免用户在弹窗剩余时间内确认却已被外层超时杀掉。
const HOST_KEY_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

async fn wait_host_key_confirmation(
    registry: &HostKeyRegistry,
    key: &str,
    decision: Arc<HostKeyDecision>,
    is_new: bool,
    fingerprint: &str,
    key_type: &str,
) -> Result<bool, ()> {
    // 仅首次注册的连接发弹窗事件，并发连接复用同一决策、不重复弹窗
    if is_new {
        if let Some(app) = APP_HANDLE.get() {
            let _ = app.emit(
                "ssh:host-key-confirm",
                serde_json::json!({
                    "key": key,
                    "host": key.rsplit_once(':').map(|(h, _)| h).unwrap_or(key),
                    "port": key.rsplit_once(':').map(|(_, p)| p).unwrap_or("22"),
                    "fingerprint": fingerprint,
                    "key_type": key_type,
                }),
            );
        }
    }
    tokio::time::timeout(HOST_KEY_CONFIRM_TIMEOUT, async {
        loop {
            if let Some(d) = decision.get() {
                return d;
            }
            decision.notify.notified().await;
        }
    })
    .await
    .map_err(|_| {
        // 超时：移除 pending，由调用方按超时取消处理
        registry.remove(key);
    })
}

/// 确认并信任某主机的指纹（Tauri 命令：前端弹窗按钮调用）。
#[tauri::command]
pub fn ssh_confirm_host_key(key: String, trust: bool) -> Result<(), String> {
    let registry = APP_REGISTRY.get_or_init(HostKeyRegistry::default);
    registry.resolve(&key, trust)
}

/// 主机密钥校验的失败原因：check_server_key 拒绝时记录，
/// connect 返回错误后据此生成精确的错误文案（替代按错误字符串猜测）。
enum HostKeyFailure {
    /// 用户在弹窗拒绝
    Rejected,
    /// 60s 内未确认
    Timeout,
    /// 已记录指纹变化（可能的中间人攻击）
    Changed {
        line: usize,
        key_type: String,
        recorded_fp: Option<String>,
        actual_fp: String,
    },
    /// known_hosts 读取/解析失败
    ReadError(String),
}

/// 自定义 Handler：用 ~/.ssh/known_hosts 校验服务器主机密钥；
/// 首次连接（未知主机）会弹窗让用户确认指纹（TOFU + 显式确认），
/// 已记录主机指纹变化时直接拒绝（严格校验）。
#[derive(Clone)]
pub struct ClientHandler {
    host: Host,
    failure: Arc<Mutex<Option<HostKeyFailure>>>,
}

impl ClientHandler {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            failure: Arc::new(Mutex::new(None)),
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let known_hosts = default_known_hosts_path();
        match keys::check_known_hosts_path(
            &self.host.address,
            self.host.port,
            server_public_key,
            &known_hosts,
        ) {
            Ok(true) => Ok(true),
            Ok(false) => {
                // 首次连接：弹窗确认指纹，用户信任后才记录
                let registry = APP_REGISTRY.get_or_init(HostKeyRegistry::default);
                let key = host_key_id(&self.host.address, self.host.port);
                let fingerprint = server_public_key
                    .fingerprint(ssh_key::HashAlg::Sha256)
                    .to_string();
                let key_type = server_public_key.algorithm().to_string();
                let (decision, is_new) = registry.register(&key);
                match wait_host_key_confirmation(
                    &registry,
                    &key,
                    decision,
                    is_new,
                    &fingerprint,
                    &key_type,
                )
                .await
                {
                    Ok(true) => {
                        append_known_host(
                            &known_hosts,
                            &self.host.address,
                            self.host.port,
                            server_public_key,
                        );
                        Ok(true)
                    }
                    Ok(false) => {
                        *self.failure.lock().unwrap() = Some(HostKeyFailure::Rejected);
                        Ok(false)
                    }
                    Err(()) => {
                        *self.failure.lock().unwrap() = Some(HostKeyFailure::Timeout);
                        Ok(false)
                    }
                }
            }
            Err(keys::Error::KeyChanged { line }) => {
                // 取同算法（优先该行）的已记录密钥指纹，供错误文案对比展示
                let recorded_fp = keys::known_hosts::known_host_keys_path(
                    &self.host.address,
                    self.host.port,
                    &known_hosts,
                )
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|(l, _)| *l == line)
                        .or_else(|| {
                            entries.iter().find(|(_, k)| {
                                k.algorithm() == server_public_key.algorithm()
                            })
                        })
                        .map(|(_, k)| k.fingerprint(ssh_key::HashAlg::Sha256).to_string())
                });
                *self.failure.lock().unwrap() = Some(HostKeyFailure::Changed {
                    line,
                    key_type: server_public_key.algorithm().to_string(),
                    recorded_fp,
                    actual_fp: server_public_key
                        .fingerprint(ssh_key::HashAlg::Sha256)
                        .to_string(),
                });
                Ok(false)
            }
            Err(e) => {
                *self.failure.lock().unwrap() = Some(HostKeyFailure::ReadError(e.to_string()));
                Ok(false)
            }
        }
    }
}

/// 进程内 known_hosts 追加锁：多个首次连接并发写同一文件时串行化，
/// 并用「临时文件 + rename」保证原子性，避免交错写坏文件。
static KNOWN_HOSTS_LOCK: Mutex<()> = Mutex::new(());

/// 首次连接：把主机指纹追加到 known_hosts（TOFU + 用户确认），
/// 之后再次连接会严格校验，指纹变化会立即被拒绝。
fn append_known_host(path: &PathBuf, host: &str, port: u16, key: &keys::PublicKey) {
    let Ok(openssh) = key.to_openssh() else {
        return;
    };
    let hostname = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let line = format!("{hostname} {openssh}\n");
    let _guard = KNOWN_HOSTS_LOCK.lock().unwrap();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 读取现有内容（不存在则视为空），追加后原子替换
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // 已记录过该主机（并发首次连接共享同一决策时可能重复触发）→ 跳过，避免重复行
    let prefix = format!("{hostname} ");
    if existing.lines().any(|l| l.starts_with(&prefix)) {
        return;
    }
    let tmp = path.with_extension("known_hosts.tmp");
    let content = format!("{existing}{line}");
    if std::fs::write(&tmp, content.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        // 临时文件写入失败时退化为直接追加（尽力而为）
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// 单连接最大并发 channel 数（低于 OpenSSH MaxSessions 默认值 10，留有余量）。
const MAX_CHANNELS_PER_CONN: usize = 6;

/// 池化连接：Handle 本体 + channel 并发信号量。
/// Handle 用 Arc 共享：租约持有期间连接不会被淘汰/回收（靠 strong_count 判断）。
#[derive(Clone)]
struct PooledConn {
    handle: Arc<Handle<ClientHandler>>,
    channels: Arc<tokio::sync::Semaphore>,
}

type ConnSlot = Arc<tokio::sync::Mutex<Option<PooledConn>>>;

/// 连接租约：持有期间该连接不会被淘汰（Arc 计数），permit 限制单连接并发 channel 数。
/// slot 锁只在「取/建连接」瞬间持有，channel 操作在锁外并发执行。
pub(crate) struct ConnLease {
    pub handle: Arc<Handle<ClientHandler>>,
    pub reused: bool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// 连接池最大同时打开的连接数，超出时按 LRU 淘汰最久未使用的连接。
const MAX_CONNS: usize = 16;
/// 连接空闲超时（秒）：超过该时长未使用的连接会被关闭，释放远端与会话资源。
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// 每个主机的连接槽位 + 最近使用时间（Unix 秒），供 LRU 淘汰与空闲清理使用。
struct ConnEntry {
    slot: ConnSlot,
    last_used: AtomicU64,
}

/// 按主机复用的 russh 连接池
pub struct RusshManager {
    conns: Mutex<HashMap<String, ConnEntry>>,
}

impl Default for RusshManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RusshManager {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// 获取（或创建）指定主机的连接槽位，并更新最近使用时间。
    fn slot(&self, host: &Host) -> ConnSlot {
        let mut map = self.conns.lock().unwrap();
        let now = crate::util::now();
        let entry = map.entry(host.id.clone()).or_insert_with(|| ConnEntry {
            slot: Arc::new(tokio::sync::Mutex::new(None)),
            last_used: AtomicU64::new(now),
        });
        entry.last_used.store(now, Ordering::Relaxed);
        entry.slot.clone()
    }

    /// 获取一个连接租约：已有存活连接则复用（reused = true），否则新建。
    /// slot 锁只覆盖「取/建连接」；新建连接在锁内完成（single-flight），
    /// channel 配额等待放在锁外，避免阻塞其它取连接的操作。
    pub(crate) async fn acquire(&self, host: &Host) -> Result<ConnLease, String> {
        let slot = self.slot(host);
        let (conn, reused) = {
            let mut guard = slot.lock().await;
            match guard.as_ref() {
                Some(conn) if !conn.handle.is_closed() => (conn.clone(), true),
                _ => {
                    // 即将创建新连接，先淘汰超额的空闲连接
                    self.evict_if_needed(&host.id);
                    let handle = connect(host).await?;
                    let conn = PooledConn {
                        handle: Arc::new(handle),
                        channels: Arc::new(tokio::sync::Semaphore::new(MAX_CHANNELS_PER_CONN)),
                    };
                    *guard = Some(conn.clone());
                    (conn, false)
                }
            }
        };
        let permit = conn
            .channels
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "连接已关闭".to_string())?;
        Ok(ConnLease {
            handle: conn.handle,
            reused,
            _permit: permit,
        })
    }

    /// 作废指定连接：仅当槽位中仍是同一连接时清空（Arc::ptr_eq 判断），
    /// 避免误删并发重建的新连接。
    pub(crate) fn invalidate(&self, host_id: &str, handle: &Arc<Handle<ClientHandler>>) {
        let map = self.conns.lock().unwrap();
        let Some(entry) = map.get(host_id) else {
            return;
        };
        let mut guard = match entry.slot.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if guard
            .as_ref()
            .is_some_and(|conn| Arc::ptr_eq(&conn.handle, handle))
        {
            *guard = None;
        }
    }

    /// 在创建新连接前调用：若已达 MAX_CONNS，淘汰最久未使用且当前空闲的连接。
    /// 「正在使用」以 Arc::strong_count > 1 判定（有租约持有即跳过）；
    /// `exclude_id` 为即将创建连接的主机 ID，不会被淘汰。
    fn evict_if_needed(&self, exclude_id: &str) {
        let map = self.conns.lock().unwrap();
        // 统计当前空闲连接数，并找出 LRU 候选
        let mut open: Vec<(&String, u64)> = Vec::new();
        for (id, entry) in map.iter() {
            if id == exclude_id {
                continue;
            }
            let idle_open = match entry.slot.try_lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_some_and(|conn| Arc::strong_count(&conn.handle) == 1),
                Err(_) => false, // 正在取/建连接，跳过
            };
            if idle_open {
                open.push((id, entry.last_used.load(Ordering::Relaxed)));
            }
        }
        if open.len() < MAX_CONNS {
            return;
        }
        // 淘汰最久未使用（last_used 最小）的空闲连接，淘汰前再次确认无人租用
        if let Some((lru_id, _)) = open.into_iter().min_by_key(|(_, t)| *t) {
            if let Some(entry) = map.get(lru_id) {
                if let Ok(mut guard) = entry.slot.try_lock() {
                    if guard
                        .as_ref()
                        .is_some_and(|conn| Arc::strong_count(&conn.handle) == 1)
                    {
                        *guard = None;
                    }
                }
            }
        }
    }

    /// 更新指定主机的最近使用时间（连接成功后调用）。
    pub(crate) fn touch(&self, host_id: &str) {
        let map = self.conns.lock().unwrap();
        if let Some(entry) = map.get(host_id) {
            entry.last_used.store(crate::util::now(), Ordering::Relaxed);
        }
    }

    /// 空闲清理：回收超过 IDLE_TIMEOUT_SECS 未使用的空闲连接，
    /// 以及已断开（is_closed）的死连接。有租约持有的连接不会被回收。
    /// 由 spawn_idle_reaper 周期性调用。
    pub(crate) fn reap_idle(&self) {
        let now = crate::util::now();
        let map = self.conns.lock().unwrap();
        for entry in map.values() {
            let idle_expired =
                now.saturating_sub(entry.last_used.load(Ordering::Relaxed)) > IDLE_TIMEOUT_SECS;
            if let Ok(mut guard) = entry.slot.try_lock() {
                let should_drop = guard.as_ref().is_some_and(|conn| {
                    conn.handle.is_closed()
                        || (idle_expired && Arc::strong_count(&conn.handle) == 1)
                });
                if should_drop {
                    *guard = None;
                }
            }
        }
    }

    pub async fn exec(
        &self,
        host: &Host,
        command: &str,
        timeout: Duration,
    ) -> Result<ExecResult, String> {
        let lease = self.acquire(host).await?;
        match exec_on(&lease.handle, command, timeout).await {
            Ok(r) => {
                self.touch(&host.id);
                Ok(r)
            }
            Err(e) => {
                self.invalidate(&host.id, &lease.handle);
                if !lease.reused {
                    // 新建连接上执行失败：连接或命令本身有问题，直接报错
                    return Err(e);
                }
                // 写命令失败可能是「请求已提交后连接中断」：远端可能已执行，
                // 重连重试会造成重复副作用，因此写操作一律不自动重试
                if crate::safety::is_write_operation(command) {
                    return Err(
                        "连接在命令执行期间中断。该命令为写操作，为避免重复执行已停止，请人工确认远端状态后重试"
                            .to_string(),
                    );
                }
                // 复用连接上执行失败：换新连接重试一次（只读操作可安全重试）
                let lease = self.acquire(host).await?;
                match exec_on(&lease.handle, command, timeout).await {
                    Ok(r) => {
                        self.touch(&host.id);
                        Ok(r)
                    }
                    Err(e) => {
                        self.invalidate(&host.id, &lease.handle);
                        Err(e)
                    }
                }
            }
        }
    }
}

/// 测试主机连接：连接 + 认证，成功返回提示。
/// passphrase 仅用于本地解密加密的私钥文件，不参与远端认证。
pub async fn test_connection(
    host: &Host,
    password: Option<String>,
    passphrase: Option<String>,
) -> Result<String, String> {
    let _handle = tokio::time::timeout(
        // 需覆盖首次连接的主机指纹确认窗口（60s）
        Duration::from_secs(60),
        do_connect(host, password, passphrase),
    )
    .await
    .map_err(|_| "连接超时（60 秒）".to_string())??;
    Ok(format!(
        "连接成功（{}@{}:{}）",
        host.username, host.address, host.port
    ))
}

/// 启动空闲连接回收任务：每 60s 扫描一次连接池，回收超时/已断开的连接。
/// 在 setup 中调用；try_state 容错（state 在 builder 链上注册，晚于 setup 也不 panic）。
pub fn spawn_idle_reaper(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.tick().await; // interval 首次 tick 立即返回，跳过
        loop {
            ticker.tick().await;
            if let Some(manager) = app.try_state::<RusshManager>() {
                manager.reap_idle();
            }
        }
    });
}

pub(crate) async fn connect(host: &Host) -> Result<Handle<ClientHandler>, String> {
    // 需覆盖首次连接的主机指纹确认窗口（60s）
    tokio::time::timeout(Duration::from_secs(60), do_connect(host, None, None))
        .await
        .map_err(|_| "SSH 连接超时（60 秒）".to_string())?
}

pub(crate) async fn do_connect(
    host: &Host,
    password_override: Option<String>,
    passphrase_override: Option<String>,
) -> Result<Handle<ClientHandler>, String> {
    let mut config = Config::default();
    config.nodelay = true; // 禁用 Nagle，降低交互输入回显延迟
    config.keepalive_interval = Some(Duration::from_secs(15));
    config.keepalive_max = 3;
    config.inactivity_timeout = None; // 禁用空闲回收，避免连接刚建立就被判定超时
    let config = Arc::new(config);

    let handler = ClientHandler::new(host.clone());
    // 与 handler 共享的主机密钥失败原因：connect 出错时据此生成精确错误文案
    let host_key_failure = handler.failure.clone();
    let mut session = client::connect(
        config,
        (host.address.as_str(), host.port),
        handler,
    )
    .await
    .map_err(|e| match host_key_failure.lock().unwrap().take() {
        Some(HostKeyFailure::Rejected) => format!(
            "已取消连接：你拒绝了主机 {}:{} 的指纹",
            host.address, host.port
        ),
        Some(HostKeyFailure::Timeout) => {
            "连接已取消：主机指纹确认超时（60 秒内未确认）".to_string()
        }
        Some(HostKeyFailure::Changed {
            line,
            key_type,
            recorded_fp,
            actual_fp,
        }) => {
            let recorded = match recorded_fp {
                Some(fp) => {
                    format!("已记录：{key_type} {fp}（~/.ssh/known_hosts 第 {line} 行）")
                }
                None => format!("已记录：{key_type}（~/.ssh/known_hosts 第 {line} 行）"),
            };
            format!(
                "主机指纹已变化，已拒绝连接（可能存在中间人攻击）。\n{recorded}\n当前：{key_type} {actual_fp}\n如确认服务器已重装或更换了密钥，请删除 known_hosts 中该行后重试。"
            )
        }
        Some(HostKeyFailure::ReadError(e)) => format!("读取 ~/.ssh/known_hosts 失败：{e}"),
        None => format!("SSH 连接失败: {e}"),
    })?;

    if host.auth_type == AuthType::Password {
        authenticate_with_password(&mut session, host, password_override).await?;
    } else {
        authenticate_with_key(&mut session, host, passphrase_override).await?;
    }
    Ok(session)
}

/// 密码认证：先走 password 方法；服务器仅接受 keyboard-interactive 时
/// 自动用同一密码应答密码类 prompt（部分设备/PAM 只开 KI 认证）。
async fn authenticate_with_password(
    session: &mut Handle<ClientHandler>,
    host: &Host,
    password_override: Option<String>,
) -> Result<(), String> {
    let password = password_override
        .filter(|p| !p.trim().is_empty())
        .or_else(|| crate::credentials::get_password(&host.id))
        .ok_or_else(|| {
            "服务器要求密码认证，但未提供密码。请填写密码或先在主机中保存。".to_string()
        })?;
    let (remaining_methods, partial_success) = match session
        .authenticate_password(&host.username, password.as_str())
        .await
        .map_err(|e| format!("密码认证失败: {e}"))?
    {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => (remaining_methods, partial_success),
    };
    if partial_success {
        let remaining = remaining_methods
            .iter()
            .map(String::from)
            .collect::<Vec<_>>()
            .join(",");
        return Err(format!(
            "服务器要求多因素认证（{remaining}），当前版本暂不支持"
        ));
    }
    if remaining_methods.contains(&MethodKind::KeyboardInteractive) {
        let mut response = session
            .authenticate_keyboard_interactive_start(&host.username, None::<String>)
            .await
            .map_err(|e| format!("交互式认证失败: {e}"))?;
        // 服务器可能连续下发多轮 prompt，最多应答 5 轮防止死循环
        for _ in 0..5 {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => break,
                KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                    // echo=false 或提示语含 password/密码 → 回应密码，其余 prompt 回空串
                    let answers = prompts
                        .iter()
                        .map(|p| {
                            let lower = p.prompt.to_lowercase();
                            if !p.echo || lower.contains("password") || p.prompt.contains("密码") {
                                password.clone()
                            } else {
                                String::new()
                            }
                        })
                        .collect();
                    response = session
                        .authenticate_keyboard_interactive_respond(answers)
                        .await
                        .map_err(|e| format!("交互式认证失败: {e}"))?;
                }
            }
        }
    }
    Err("SSH 认证失败（用户名 / 密码不正确）".to_string())
}

/// 密钥认证：先试本地私钥文件（支持口令解密），再试 ssh-agent 中的身份。
/// RSA 密钥按服务器通告的 server-sig-algs 选哈希，未通告时按 russh 文档建议用 sha2-256。
async fn authenticate_with_key(
    session: &mut Handle<ClientHandler>,
    host: &Host,
    passphrase_override: Option<String>,
) -> Result<(), String> {
    let rsa_hash: Option<HashAlg> = match session.best_supported_rsa_hash().await {
        Ok(Some(h)) => h,
        _ => Some(HashAlg::Sha256),
    };

    let explicit_path = host
        .key_path
        .clone()
        .filter(|p| !p.trim().is_empty());
    let key_path = explicit_path.clone().unwrap_or_else(default_key_path);
    // 口令只用于本地解密私钥：优先测试/表单传入，其次该主机已保存的口令
    let passphrase = passphrase_override
        .filter(|p| !p.trim().is_empty())
        .or_else(|| crate::credentials::get_key_passphrase(&host.id));

    let mut file_err: Option<String> = None;
    let mut file_pubkey: Option<keys::PublicKey> = None;
    // 未显式配置私钥路径且默认路径不存在时，不算错误，直接进入 agent 尝试
    if explicit_path.is_some() || std::path::Path::new(&key_path).exists() {
        match keys::load_secret_key(&key_path, passphrase.as_deref()) {
            Ok(key) => {
                file_pubkey = Some(key.public_key().clone());
                match session
                    .authenticate_publickey(
                        &host.username,
                        PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
                    )
                    .await
                {
                    Ok(AuthResult::Success) => return Ok(()),
                    Ok(AuthResult::Failure { .. }) => {
                        file_err = Some(format!("服务器拒绝了私钥 {key_path}"));
                    }
                    Err(e) => return Err(format!("密钥认证失败: {e}")),
                }
            }
            Err(e) => {
                file_err = Some(if matches!(e, keys::Error::KeyIsEncrypted)
                    && passphrase.is_none()
                {
                    format!(
                        "私钥 {key_path} 已加密，请在主机编辑中填写私钥口令（或将其加入 ssh-agent）"
                    )
                } else if passphrase.is_some() && !matches!(e, keys::Error::IO(_)) {
                    format!("私钥 {key_path} 解密失败：口令不正确或文件损坏")
                } else {
                    format!("读取私钥失败（{key_path}）: {e}")
                });
            }
        }
    }

    let (agent_tried, agent_available) =
        match try_agent_auth(session, &host.username, rsa_hash, file_pubkey.as_ref()).await {
            AgentAuth::Succeeded => return Ok(()),
            AgentAuth::Unavailable => (0, false),
            AgentAuth::Failed { tried } => (tried, true),
        };

    let agent_note = if agent_tried > 0 {
        format!("ssh-agent 中 {agent_tried} 个身份均被服务器拒绝")
    } else if agent_available {
        "ssh-agent 中没有可用身份".to_string()
    } else {
        "未检测到 ssh-agent（SSH_AUTH_SOCK）".to_string()
    };
    let detail = match file_err {
        Some(e) => format!("{e}；{agent_note}"),
        None => agent_note,
    };
    Err(format!("SSH 密钥认证失败：{detail}"))
}

/// ssh-agent 认证尝试结果
enum AgentAuth {
    Succeeded,
    Unavailable,
    Failed { tried: usize },
}

/// ssh-agent 最多尝试的身份数：避免 agent 里堆积的密钥逐一把服务器
/// MaxAuthTries 耗尽（OpenSSH 默认 6）。
#[cfg(unix)]
const MAX_AGENT_IDENTITIES: usize = 4;

/// 用 ssh-agent 中的身份尝试认证（仅 Unix；跳过与已加载私钥相同的公钥）。
#[cfg(unix)]
async fn try_agent_auth(
    session: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<HashAlg>,
    file_pubkey: Option<&keys::PublicKey>,
) -> AgentAuth {
    let Ok(mut agent) = AgentClient::connect_env().await else {
        return AgentAuth::Unavailable;
    };
    let identities = agent.request_identities().await.unwrap_or_default();
    let mut tried = 0usize;
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        // 比较 key_data 而非 PublicKey 整体：PartialEq 含 comment，
        // agent 与文件中的 comment 常不同，会误判为两把钥匙重复尝试
        if file_pubkey.is_some_and(|pk| pk.key_data() == key.key_data()) {
            continue;
        }
        if tried >= MAX_AGENT_IDENTITIES {
            break;
        }
        tried += 1;
        let hash = if key.algorithm().is_rsa() {
            rsa_hash
        } else {
            None
        };
        if let Ok(AuthResult::Success) = session
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            return AgentAuth::Succeeded;
        }
    }
    AgentAuth::Failed { tried }
}

/// 非 Unix 平台无 ssh-agent（SSH_AUTH_SOCK），直接视为不可用。
#[cfg(not(unix))]
async fn try_agent_auth(
    _session: &mut Handle<ClientHandler>,
    _user: &str,
    _rsa_hash: Option<HashAlg>,
    _file_pubkey: Option<&keys::PublicKey>,
) -> AgentAuth {
    AgentAuth::Unavailable
}

/// exec 输出累积上限（8MB）：超出后停止累积并在结果中标注截断，
/// 防止失控命令的输出撑爆内存。
const MAX_EXEC_OUTPUT: usize = 8 * 1024 * 1024;

/// 通道打开 / 命令提交的超时：异常网络下避免 exec 无限期挂住。
pub(crate) const CHANNEL_OP_TIMEOUT: Duration = Duration::from_secs(15);

async fn exec_on(
    handle: &Handle<ClientHandler>,
    command: &str,
    timeout: Duration,
) -> Result<ExecResult, String> {
    let mut channel: Channel<russh::client::Msg> =
        tokio::time::timeout(CHANNEL_OP_TIMEOUT, handle.channel_open_session())
            .await
            .map_err(|_| "打开通道超时".to_string())?
            .map_err(|e| format!("打开通道失败: {e}"))?;
    tokio::time::timeout(CHANNEL_OP_TIMEOUT, channel.exec(true, command))
        .await
        .map_err(|_| "提交命令超时".to_string())?
        .map_err(|e| format!("执行命令失败: {e}"))?;
    // 提交后立即关闭 stdin：cat/read 等读取标准输入的命令得以正常结束，
    // 否则会一直挂到超时
    let _ = channel.eof().await;

    let mut merged = Vec::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let mut timed_out = false;
    let mut truncated = false;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        let msg = match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(m) => m,
            Err(_) => {
                timed_out = true;
                break;
            }
        };
        let (data, is_stderr) = match msg {
            Some(ChannelMsg::Data { data }) => (data, false),
            Some(ChannelMsg::ExtendedData { data, .. }) => (data, true),
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
                continue;
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            _ => continue,
        };
        // stdout / stderr 分开累积，同时维护按到达顺序合并的 text；
        // 三者共享同一上限（以合并长度为准），超限即停止累积并标记截断
        let room = MAX_EXEC_OUTPUT.saturating_sub(merged.len());
        let take = room.min(data.len());
        if take < data.len() {
            truncated = true;
        }
        if take > 0 {
            merged.extend_from_slice(&data[..take]);
            if is_stderr {
                stderr.extend_from_slice(&data[..take]);
            } else {
                stdout.extend_from_slice(&data[..take]);
            }
        }
    }
    if timed_out {
        // 超时：向远端发送 KILL 信号尽力终止命令，避免留下孤儿进程
        let _ = channel.signal(russh::Sig::KILL).await;
    }
    let _ = channel.close().await;
    let mut text = String::from_utf8_lossy(&merged).to_string();
    if truncated {
        text.push_str("\n[输出超过 8MB，已截断]");
    }
    Ok(ExecResult {
        text,
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
        exit_code,
        timed_out,
    })
}

fn default_key_path() -> String {
    let home = crate::util::user_home_dir().unwrap_or_default();
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = home.join(".ssh").join(name);
        if p.exists() {
            return p.to_string_lossy().to_string();
        }
    }
    home.join(".ssh")
        .join("id_ed25519")
        .to_string_lossy()
        .to_string()
}

fn default_known_hosts_path() -> PathBuf {
    // 测试可用 BUFFTERM_KNOWN_HOSTS 覆盖，避免读写真实 ~/.ssh/known_hosts
    #[cfg(test)]
    if let Ok(p) = std::env::var("BUFFTERM_KNOWN_HOSTS") {
        return PathBuf::from(p);
    }
    crate::util::user_home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("known_hosts")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_host(key_path: String) -> Host {
        Host {
            id: "test-host".to_string(),
            name: "local-sshd".to_string(),
            address: "127.0.0.1".to_string(),
            port: std::env::var("BUFFTERM_TEST_SSH_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(2222),
            username: std::env::var("BUFFTERM_TEST_SSH_USER")
                .ok()
                .or_else(|| std::env::var("USER").ok())
                .unwrap_or_default(),
            auth_type: AuthType::Key,
            key_path: Some(key_path),
            notes: None,
            created_at: 0,
        }
    }

    /// 写入测试用 known_hosts（[127.0.0.1]:port + 主机公钥），
    /// 路径取自 BUFFTERM_KNOWN_HOSTS，主机公钥文件取自 BUFFTERM_TEST_SSH_HOSTKEY。
    fn write_known_hosts(port: u16) {
        let kh = std::env::var("BUFFTERM_KNOWN_HOSTS").expect("BUFFTERM_KNOWN_HOSTS 未设置");
        let hostkey =
            std::env::var("BUFFTERM_TEST_SSH_HOSTKEY").expect("BUFFTERM_TEST_SSH_HOSTKEY 未设置");
        let line = std::fs::read_to_string(hostkey).expect("读取主机公钥失败");
        std::fs::write(&kh, format!("[127.0.0.1]:{port} {line}"))
            .expect("写测试 known_hosts 失败");
    }

    /// 集成测试（默认 ignore）：需要本机临时 sshd，例如：
    ///   /usr/sbin/sshd -D -e -f /tmp/buffterm-sshd-test/sshd_config
    ///   BUFFTERM_TEST_SSH_PORT=2222 BUFFTERM_TEST_SSH_USER=$USER \
    ///   BUFFTERM_TEST_SSH_KEY=/tmp/buffterm-sshd-test/id_rsa \
    ///   BUFFTERM_TEST_SSH_KEY_ENC=/tmp/buffterm-sshd-test/id_ed25519_enc \
    ///   BUFFTERM_TEST_SSH_PASSPHRASE=testpass123 \
    ///   BUFFTERM_TEST_SSH_HOSTKEY=/tmp/buffterm-sshd-test/ssh_host_ed25519_key.pub \
    ///   BUFFTERM_KNOWN_HOSTS=/tmp/buffterm-sshd-test/known_hosts \
    ///   cargo test russh -- --ignored
    #[test]
    #[ignore]
    fn local_sshd_end_to_end() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        rt.block_on(async {
            let host = test_host(
                std::env::var("BUFFTERM_TEST_SSH_KEY").expect("BUFFTERM_TEST_SSH_KEY 未设置"),
            );
            write_known_hosts(host.port);

            // RSA 密钥认证成功（测试 sshd 仅放行 rsa-sha2-*，证明 sha2 协商生效）
            do_connect(&host, None, None).await.expect("RSA 密钥认证失败");

            // 带口令的 ed25519 私钥：口令本地解密后认证成功
            let enc_host = test_host(
                std::env::var("BUFFTERM_TEST_SSH_KEY_ENC")
                    .expect("BUFFTERM_TEST_SSH_KEY_ENC 未设置"),
            );
            let passphrase = std::env::var("BUFFTERM_TEST_SSH_PASSPHRASE")
                .expect("BUFFTERM_TEST_SSH_PASSPHRASE 未设置");
            do_connect(&enc_host, None, Some(passphrase))
                .await
                .expect("加密私钥认证失败");

            let manager = RusshManager::new();

            // cat 应立即因 stdin EOF 返回，而不是挂到超时
            let t0 = std::time::Instant::now();
            let r = manager
                .exec(&host, "cat", Duration::from_secs(30))
                .await
                .expect("cat 执行失败");
            assert!(!r.timed_out, "cat 应在 stdin EOF 后立即返回");
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "cat 耗时异常: {:?}",
                t0.elapsed()
            );

            // stdout/stderr 分离 + 退出码
            let r = manager
                .exec(
                    &host,
                    "sh -c 'echo out; echo err 1>&2; exit 3'",
                    Duration::from_secs(15),
                )
                .await
                .expect("exec 失败");
            assert_eq!(r.stdout.trim(), "out");
            assert_eq!(r.stderr.trim(), "err");
            assert_eq!(r.exit_code, Some(3));

            // 同一 host 两个 exec 并发（sleep 2 × 2 总耗时应 < 3.5s，证明锁粒度修复）
            let t0 = std::time::Instant::now();
            let (a, b) = tokio::join!(
                manager.exec(&host, "sleep 2", Duration::from_secs(15)),
                manager.exec(&host, "sleep 2", Duration::from_secs(15)),
            );
            a.expect("并发 exec A 失败");
            b.expect("并发 exec B 失败");
            assert!(
                t0.elapsed() < Duration::from_millis(3500),
                "并发 exec 耗时 {:?}，疑似串行",
                t0.elapsed()
            );
        });
    }
}
