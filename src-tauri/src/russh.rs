use crate::models::{AuthType, Host};
use russh::client::{self, Config, Handle};
use russh::keys::{self, key::PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tauri::Emitter;

pub struct ExecResult {
    pub text: String,
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
) -> bool {
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
    .unwrap_or_else(|_| {
        // 超时：移除 pending，按拒绝处理
        registry.remove(key);
        false
    })
}

/// 确认并信任某主机的指纹（Tauri 命令：前端弹窗按钮调用）。
#[tauri::command]
pub fn ssh_confirm_host_key(key: String, trust: bool) -> Result<(), String> {
    let registry = APP_REGISTRY.get_or_init(HostKeyRegistry::default);
    registry.resolve(&key, trust)
}

/// 自定义 Handler：用 ~/.ssh/known_hosts 校验服务器主机密钥；
/// 首次连接（未知主机）会弹窗让用户确认指纹（TOFU + 显式确认），
/// 已记录主机指纹变化时直接拒绝（严格校验）。
#[derive(Clone)]
pub struct ClientHandler {
    host: Host,
}

impl ClientHandler {
    pub fn new(host: Host) -> Self {
        Self { host }
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
                if wait_host_key_confirmation(
                    &registry,
                    &key,
                    decision,
                    is_new,
                    &fingerprint,
                    &key_type,
                )
                .await
                {
                    append_known_host(
                        &known_hosts,
                        &self.host.address,
                        self.host.port,
                        server_public_key,
                    );
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(_) => Ok(false),
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

type ConnSlot = Arc<tokio::sync::Mutex<Option<Handle<ClientHandler>>>>;

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
    /// 同时执行空闲清理与容量淘汰（try_lock 不阻塞正在使用的连接）。
    pub(crate) fn slot(&self, host: &Host) -> ConnSlot {
        let mut map = self.conns.lock().unwrap();
        let now = crate::util::now();
        // 空闲清理：关闭超过 IDLE_TIMEOUT_SECS 未使用的连接
        for entry in map.values() {
            let last = entry.last_used.load(Ordering::Relaxed);
            if now.saturating_sub(last) > IDLE_TIMEOUT_SECS {
                if let Ok(mut guard) = entry.slot.try_lock() {
                    if guard.is_some() {
                        *guard = None;
                    }
                }
            }
        }
        // 更新当前主机的 last_used
        let entry = map.entry(host.id.clone()).or_insert_with(|| ConnEntry {
            slot: Arc::new(tokio::sync::Mutex::new(None)),
            last_used: AtomicU64::new(now),
        });
        entry.last_used.store(now, Ordering::Relaxed);
        entry.slot.clone()
    }

    /// 在创建新连接前调用：若已达 MAX_CONNS，淘汰最久未使用且当前空闲的连接。
    /// `exclude_id` 为即将创建连接的主机 ID，不会被淘汰。
    pub(crate) fn evict_if_needed(&self, exclude_id: &str) {
        let map = self.conns.lock().unwrap();
        // 统计当前打开的连接数，并找出 LRU 候选
        let mut open: Vec<(&String, u64)> = Vec::new();
        for (id, entry) in map.iter() {
            if id == exclude_id {
                continue;
            }
            // try_lock：正在使用的连接（锁被持有）跳过，不淘汰
            let is_open = match entry.slot.try_lock() {
                Ok(guard) => guard.is_some(),
                Err(_) => continue, // 锁被持有 = 正在使用，跳过
            };
            if is_open {
                open.push((id, entry.last_used.load(Ordering::Relaxed)));
            }
        }
        if open.len() < MAX_CONNS {
            return;
        }
        // 淘汰最久未使用（last_used 最小）的空闲连接
        if let Some((lru_id, _)) = open.into_iter().min_by_key(|(_, t)| *t) {
            if let Some(entry) = map.get(lru_id) {
                if let Ok(mut guard) = entry.slot.try_lock() {
                    *guard = None;
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

    pub async fn exec(
        &self,
        host: &Host,
        command: &str,
        timeout: Duration,
    ) -> Result<ExecResult, String> {
        let slot = self.slot(host);
        let mut guard = slot.lock().await;

        if let Some(handle) = guard.as_mut() {
            match exec_on(handle, command, timeout).await {
                Ok(r) => {
                    self.touch(&host.id);
                    return Ok(r);
                }
                Err(_) => {
                    *guard = None;
                    // 写命令失败可能是「请求已提交后连接中断」：远端可能已执行，
                    // 重连重试会造成重复副作用，因此写操作一律不自动重试
                    if crate::safety::is_write_operation(command) {
                        return Err(
                            "连接在命令执行期间中断。该命令为写操作，为避免重复执行已停止，请人工确认远端状态后重试"
                                .to_string(),
                        );
                    }
                }
            }
        }

        // 即将创建新连接，先淘汰超额的空闲连接
        self.evict_if_needed(&host.id);
        let mut handle = connect(host).await?;
        let result = exec_on(&mut handle, command, timeout).await;
        match result {
            Ok(_) => {
                *guard = Some(handle);
                self.touch(&host.id);
            }
            Err(_) => {
                *guard = None;
            }
        }
        result
    }

    /// 测试主机连接：连接 + 认证，成功返回提示
    pub async fn test_connection(
        &self,
        host: &Host,
        password: Option<String>,
    ) -> Result<String, String> {
        let _handle = tokio::time::timeout(
            // 需覆盖首次连接的主机指纹确认窗口（60s）
            Duration::from_secs(60),
            do_connect(host, password),
        )
        .await
        .map_err(|_| "连接超时（60 秒）".to_string())??;
        Ok(format!(
            "连接成功（{}@{}:{}）",
            host.username, host.address, host.port
        ))
    }
}

pub(crate) async fn connect(host: &Host) -> Result<Handle<ClientHandler>, String> {
    // 需覆盖首次连接的主机指纹确认窗口（60s）
    tokio::time::timeout(Duration::from_secs(60), do_connect(host, None))
        .await
        .map_err(|_| "SSH 连接超时（60 秒）".to_string())?
}

pub(crate) async fn do_connect(
    host: &Host,
    password_override: Option<String>,
) -> Result<Handle<ClientHandler>, String> {
    let mut config = Config::default();
    config.nodelay = true; // 禁用 Nagle，降低交互输入回显延迟
    config.keepalive_interval = Some(Duration::from_secs(15));
    config.keepalive_max = 3;
    config.inactivity_timeout = None; // 禁用空闲回收，避免连接刚建立就被判定超时
    let config = Arc::new(config);

    let mut session = client::connect(
        config,
        (host.address.as_str(), host.port),
        ClientHandler::new(host.clone()),
    )
    .await
    .map_err(|e| {
        let lower = e.to_string().to_lowercase();
        if lower.contains("host key")
            || lower.contains("server key")
            || lower.contains("fingerprint")
            || lower.contains("unknown")
        {
            format!(
                "SSH 连接失败：主机指纹校验未通过（{e}）。首次连接会自动信任并记录指纹；\
                 若此前已连接过仍报此错，可能存在中间人风险，请检查服务器。"
            )
        } else {
            format!("SSH 连接失败: {e}")
        }
    })?;

    let success = if host.auth_type == AuthType::Password {
        let password = password_override
            .filter(|p| !p.trim().is_empty())
            .or_else(|| crate::credentials::get_password(&host.id))
            .ok_or_else(|| {
                "服务器要求密码认证，但未提供密码。请填写密码或先在主机中保存。".to_string()
            })?;
        session
            .authenticate_password(&host.username, password)
            .await
            .map_err(|e| format!("密码认证失败: {e}"))?
            .success()
    } else {
        let key_path = host
            .key_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(default_key_path);
        let key = keys::load_secret_key(&key_path, None)
            .map_err(|e| format!("读取私钥失败（{key_path}）: {e}"))?;
        session
            .authenticate_publickey(
                &host.username,
                PrivateKeyWithHashAlg::new(Arc::new(key), None),
            )
            .await
            .map_err(|e| format!("密钥认证失败: {e}"))?
            .success()
    };

    if !success {
        return Err("SSH 认证失败（用户名 / 密码 / 密钥不正确）".to_string());
    }
    Ok(session)
}

/// exec 输出累积上限（8MB）：超出后停止累积并在结果中标注截断，
/// 防止失控命令的输出撑爆内存。
const MAX_EXEC_OUTPUT: usize = 8 * 1024 * 1024;

async fn exec_on(
    handle: &mut Handle<ClientHandler>,
    command: &str,
    timeout: Duration,
) -> Result<ExecResult, String> {
    let mut channel: Channel<russh::client::Msg> = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开通道失败: {e}"))?;
    channel
        .exec(true, command)
        .await
        .map_err(|e| format!("执行命令失败: {e}"))?;

    let mut stdout = Vec::new();
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
        match msg {
            Some(ChannelMsg::Data { data }) => {
                if stdout.len() + data.len() > MAX_EXEC_OUTPUT {
                    let room = MAX_EXEC_OUTPUT.saturating_sub(stdout.len());
                    stdout.extend_from_slice(&data[..room]);
                    truncated = true;
                } else {
                    stdout.extend_from_slice(&data);
                }
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                if stdout.len() + data.len() > MAX_EXEC_OUTPUT {
                    let room = MAX_EXEC_OUTPUT.saturating_sub(stdout.len());
                    stdout.extend_from_slice(&data[..room]);
                    truncated = true;
                } else {
                    stdout.extend_from_slice(&data);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    if timed_out {
        // 超时：向远端发送 KILL 信号尽力终止命令，避免留下孤儿进程
        let _ = channel.signal(russh::Sig::KILL).await;
    }
    let _ = channel.close().await;
    let mut text = String::from_utf8_lossy(&stdout).to_string();
    if truncated {
        text.push_str("\n[输出超过 8MB，已截断]");
    }
    Ok(ExecResult {
        text,
        exit_code,
        timed_out,
    })
}

/// 跨平台用户主目录：优先 HOME（macOS/Linux），Windows 回退 USERPROFILE / HOMEDRIVE+HOMEPATH。
fn home_dir() -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return home;
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            return profile;
        }
    }
    match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        (Ok(drive), Ok(path)) => format!("{drive}{path}"),
        _ => String::new(),
    }
}

fn default_key_path() -> String {
    let home = home_dir();
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = format!("{home}/.ssh/{name}");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    format!("{home}/.ssh/id_ed25519")
}

fn default_known_hosts_path() -> PathBuf {
    PathBuf::from(home_dir()).join(".ssh/known_hosts")
}
