use crate::db::Db;
use crate::models::{AuditLog, Host};
use crate::russh::RusshManager;
use crate::safety::sanitize;
use crate::util::{now, truncate};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType};
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Serialize)]
pub struct SftpResult {
    pub ok: bool,
    pub text: String,
}

/// SFTP 目录条目（结构化列表，替代 ls -l 文本解析：
/// 支持符号链接 / 含空格文件名，字段不再需要前端反解析）。
#[derive(Serialize)]
pub struct SftpEntry {
    pub name: String,
    /// 符号链接时按解析后的目标判断
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// unix 秒
    pub mtime: u64,
    /// 如 "drwxr-xr-x"（file_type_char + permission_string）
    pub perms: String,
    pub user: String,
    pub group: String,
    /// 完整路径命中敏感路径规则：删除/重命名/打开时前端展示警告
    pub sensitive: bool,
}

/// 判断远端路径是否是敏感路径（供前端在上传/删除前提示）。
#[tauri::command]
pub fn sftp_check_sensitive(path: String) -> bool {
    is_sensitive_remote_path(&path)
}

fn success(text: impl Into<String>) -> SftpResult {
    SftpResult {
        ok: true,
        text: text.into(),
    }
}

/// 变更类 SFTP 操作统一写审计。删除/重命名/上传同样能绕过终端防护，
/// 不能只在编辑器保存时留痕。
#[allow(clippy::too_many_arguments)]
fn write_sftp_audit(
    db: &Db,
    host: &Host,
    tool: &str,
    summary: &str,
    approval: &str,
    status: &str,
    result: Option<&str>,
    duration_ms: u64,
) {
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: None,
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: tool.to_string(),
        source: "sftp".to_string(),
        summary: truncate(&sanitize(summary), 500),
        permission_mode: "sftp".to_string(),
        approval: approval.to_string(),
        status: status.to_string(),
        result: result.map(|text| truncate(&sanitize(text), 300)),
        duration_ms: Some(duration_ms),
    };
    let _ = db.insert_audit_log(&log);
}

/// 校验本地路径：必须为绝对路径，且拒绝凭据类敏感目录，
/// 防止恶意前端/注入把私钥、云凭据等本地文件上传到远端，
/// 或覆盖下载到敏感位置。
fn validate_local_path(local: &str) -> Result<(), String> {
    if !std::path::Path::new(local).is_absolute() {
        return Err(crate::i18n::t("本地路径必须是绝对路径", "The local path must be absolute").to_string());
    }
    // 与远端敏感路径共用规则，额外挡住 .ssh/.gnupg/.aws/.azure 等本地凭据目录。
    if crate::safety::is_sensitive_path(local) {
        return Err(if crate::i18n::is_en() {
            "The local path is a sensitive credential/system location; the operation was rejected"
                .to_string()
        } else {
            "本地路径属于敏感凭据/系统位置，已拒绝该操作".to_string()
        });
    }
    Ok(())
}

/// 进行中传输的取消标志（transfer_id -> flag），供前端随时取消。
/// 复用 inspection.rs / remediation.rs 的 CancelFlags 模式。
#[derive(Default)]
pub struct SftpTransferRegistry {
    flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// transfer_id → host_id：主机删除/连接变更时取消其进行中的传输
    hosts: Mutex<HashMap<String, String>>,
}

impl SftpTransferRegistry {
    fn register(&self, id: &str, host_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.flags
            .lock()
            .unwrap()
            .insert(id.to_string(), flag.clone());
        self.hosts
            .lock()
            .unwrap()
            .insert(id.to_string(), host_id.to_string());
        flag
    }
    fn cancel(&self, id: &str) -> bool {
        match self.flags.lock().unwrap().get(id) {
            Some(f) => {
                f.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
    /// 取消某主机的全部进行中传输（主机删除/连接信息变更时调用）。
    pub fn cancel_for_host(&self, host_id: &str) {
        let ids: Vec<String> = self
            .hosts
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, host)| host.as_str() == host_id)
            .map(|(id, _)| id.clone())
            .collect();
        let flags = self.flags.lock().unwrap();
        for id in ids {
            if let Some(flag) = flags.get(&id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }
    fn remove(&self, id: &str) {
        self.flags.lock().unwrap().remove(id);
        self.hosts.lock().unwrap().remove(id);
    }
}

/// 取消一个进行中的传输（按 transfer_id），返回是否存在该传输任务。
#[tauri::command]
pub fn sftp_cancel_transfer(
    registry: State<'_, SftpTransferRegistry>,
    transfer_id: String,
) -> Result<bool, String> {
    Ok(registry.cancel(&transfer_id))
}

/// 检查远端路径是否已存在（供上传前覆盖确认）。
#[tauri::command]
pub async fn sftp_exists(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
) -> Result<bool, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            match sftp.metadata(path.as_str()).await {
                Ok(_) => Ok(true),
                // 文件不存在是服务端 Status 响应——业务结果，返回 false
                Err(russh_sftp::client::error::Error::Status(_)) => Ok(false),
                // 会话级错误（closed/IO/timeout）不能吞成 false：
                // 否则上传预检把「查不到」当成「不存在」，跳过覆盖确认
                Err(e) => Err(format!("{}: {e}", crate::i18n::t("检查远端路径失败", "Failed to check the remote path"))),
            }
        })
    })
    .await
}

/// `sftp:progress` 事件负载：已传字节 / 总字节。
#[derive(Clone, Serialize)]
struct SftpProgress {
    transfer_id: String,
    kind: &'static str,
    transferred: u64,
    total: u64,
}

/// 传输分块大小（64 KiB）
const TRANSFER_CHUNK: usize = 64 * 1024;
/// 进度事件节流间隔
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// 传输类操作的外层超时：大文件传输可能远超 120s，放宽为 2 小时，
/// 实际中断由连接层 keepalive 与取消标志负责。
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(7200);

/// 分块读写循环：每块后检查取消标志，按节流间隔 emit `sftp:progress`。
/// 取消时返回 "传输已取消"，调用方据此清理半成品文件。
async fn copy_with_progress<R, W>(
    src: &mut R,
    dst: &mut W,
    total: u64,
    transfer_id: &str,
    kind: &'static str,
    app: &AppHandle,
    cancelled: &AtomicBool,
) -> Result<u64, String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut transferred: u64 = 0;
    let mut last_emit = Instant::now();
    let emit = |transferred: u64| {
        let _ = app.emit(
            "sftp:progress",
            SftpProgress {
                transfer_id: transfer_id.to_string(),
                kind,
                transferred,
                total,
            },
        );
    };
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Err(crate::i18n::t("传输已取消", "Transfer cancelled").to_string());
        }
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("{}: {e}", crate::i18n::t("读取数据失败", "Failed to read data")))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("{}: {e}", crate::i18n::t("写入数据失败", "Failed to write data")))?;
        transferred += n as u64;
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            emit(transferred);
            last_emit = Instant::now();
        }
    }
    emit(transferred);
    Ok(transferred)
}

/// 在已有 SSH 连接上建立一条 SFTP 会话：channel + subsystem + INIT 握手
/// （共约 3 个 RTT，是每次临时会话的主要开销）。
async fn open_sftp(
    handle: &russh::client::Handle<crate::russh::ClientHandler>,
) -> Result<SftpSession, String> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("打开 SFTP 通道失败", "Failed to open the SFTP channel")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("启动 SFTP 子系统失败", "Failed to start the SFTP subsystem")))?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("初始化 SFTP 失败", "Failed to initialize SFTP")))
}

/// 浏览类短操作专用的持久 SFTP 会话池：每主机保留一条 SftpSession，
/// 免去每次列表/stat 都要新开 channel + 启动 subsystem + INIT 握手的
/// ~3 个 RTT——这是文件面板点击目录「响应一段时间」的最大来源。
/// russh-sftp 的 &self API 内部按 request-id 多路复用，单会话即可并发
/// 处理 list/stat 等短请求；文件传输仍走独立 channel（with_sftp），
/// 保证大文件传输期间目录浏览依旧响应（与 electerm / Tabby 一致：
/// 持久 SFTP session 专供浏览操作）。
/// 底层 SSH 连接被池清理/断开后，会话内操作即报会话级错误——
/// 由 with_browse_sftp 探测活性、失效重建。
/// 浏览会话槽里的存活会话：持有连接租约——SFTP 会话活着，底层 SSH 连接
/// 就不能被连接池淘汰（淘汰判定看 Arc::strong_count>1 与 channel permit，
/// 只缓存 SftpSession 不持租约的话连接会被误判为空闲而关闭）。
/// 会话被 invalidate/drop_host 移除时租约一并释放，连接恢复空闲淘汰语义。
struct BrowseSession {
    sftp: Arc<SftpSession>,
    _lease: crate::russh::ConnLease,
}

/// 浏览会话槽：槽内最多一条可复用会话；tokio Mutex 把同主机建连串行化
type SessionSlot = Arc<tokio::sync::Mutex<Option<BrowseSession>>>;

#[derive(Default)]
pub struct SftpSessionManager {
    /// host_id → 会话槽
    sessions: Mutex<HashMap<String, SessionSlot>>,
}

impl SftpSessionManager {
    fn slot(&self, host_id: &str) -> SessionSlot {
        self.sessions
            .lock()
            .unwrap()
            .entry(host_id.to_string())
            .or_default()
            .clone()
    }

    /// 取可复用会话：槽位为空则建立（通道/子系统失败清理旧连接重试一次，
    /// 与 with_sftp 的建立策略一致）
    async fn get_or_create(
        &self,
        russh: &RusshManager,
        host: &Host,
        timeout: Duration,
    ) -> Result<Arc<SftpSession>, String> {
        let slot = self.slot(&host.id);
        let mut guard = slot.lock().await;
        if let Some(entry) = guard.as_ref() {
            return Ok(entry.sftp.clone());
        }
        for attempt in 0..2 {
            let lease = russh.acquire(host).await?;
            match tokio::time::timeout(timeout, open_sftp(&lease.handle)).await {
                Ok(Ok(s)) => {
                    let s = Arc::new(s);
                    // 租约随会话存入槽位：SFTP 会话存活期间连接不可被淘汰
                    *guard = Some(BrowseSession {
                        sftp: s.clone(),
                        _lease: lease,
                    });
                    return Ok(s);
                }
                Ok(Err(e)) => {
                    russh.invalidate(&host.id, &lease.handle);
                    if attempt == 0 {
                        continue;
                    }
                    return Err(e);
                }
                Err(_) => {
                    russh.invalidate(&host.id, &lease.handle);
                    if attempt == 0 {
                        continue;
                    }
                    return Err(crate::i18n::t("操作超时", "Operation timed out").to_string());
                }
            }
        }
        unreachable!()
    }

    /// 会话失效：仅当槽位里仍是同一条会话才清（并发重建的新会话不受影响）。
    /// try_lock 失败说明正在建连——新会话建立后即健康，无需处理
    fn invalidate(&self, host_id: &str, session: &Arc<SftpSession>) {
        let slot = self.sessions.lock().unwrap().get(host_id).cloned();
        let Some(slot) = slot else { return };
        let Ok(mut guard) = slot.try_lock() else { return };
        if guard
            .as_ref()
            .is_some_and(|entry| Arc::ptr_eq(&entry.sftp, session))
        {
            *guard = None;
        }
    }

    /// 主机删除/连接变更时整体作废
    pub fn drop_host(&self, host_id: &str) {
        self.sessions.lock().unwrap().remove(host_id);
    }
}

/// 操作失败后探测会话活性：收到服务端任意响应（含 Status 业务错误，
/// 如文件不存在）即视为会话健康；send 失败 / 超时 / 连接已关才算死。
/// 探测目标用 "."（用户家目录 lstat，恒存在）
async fn session_alive(sftp: &SftpSession) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), sftp.symlink_metadata(".")).await,
        Ok(Ok(_)) | Ok(Err(russh_sftp::client::error::Error::Status(_)))
    )
}

/// 浏览类操作入口：复用每主机的持久 SFTP 会话执行闭包。
/// 业务错误（服务端 Status 响应）原样返回；会话死亡（探测无响应）或
/// tokio 超时时作废会话并重建重试一次——重试后仍失败才报给前端。
/// 与 with_sftp 相同的策略：业务级错误不重试，避免重复副作用。
async fn with_browse_sftp<F, T>(
    manager: &SftpSessionManager,
    russh: &RusshManager,
    host: &Host,
    timeout: Duration,
    f: F,
) -> Result<T, String>
where
    F: for<'a> Fn(&'a SftpSession) -> Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>,
{
    for attempt in 0..2 {
        let sftp = manager.get_or_create(russh, host, timeout).await?;
        let result = tokio::time::timeout(timeout, f(&sftp)).await;
        russh.touch(&host.id);
        match result {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(msg)) => {
                if session_alive(&sftp).await {
                    // 会话健康：服务端明确拒绝（业务错误），不重试
                    return Err(msg);
                }
                manager.invalidate(&host.id, &sftp);
                if attempt == 0 {
                    continue;
                }
                return Err(msg);
            }
            Err(_) => {
                manager.invalidate(&host.id, &sftp);
                if attempt == 0 {
                    continue;
                }
                return Err(crate::i18n::t("操作超时", "Operation timed out").to_string());
            }
        }
    }
    unreachable!()
}

/// 通过 RusshManager 连接池执行 SFTP 操作：复用已有 SSH 连接，避免每次操作都重新
/// TCP 握手 + 认证。通道打开 / SFTP 初始化失败时清理旧连接并重连重试一次
/// （与 exec 的重连策略一致）；SFTP 操作本身的错误（文件不存在等）不重试。
/// 租约持有期间 slot 锁不占用，同一连接上可并发跑其它 exec / channel。
async fn with_sftp<F, T>(
    russh: &RusshManager,
    host: &Host,
    timeout: Duration,
    f: F,
) -> Result<T, String>
where
    F: for<'a> Fn(&'a SftpSession) -> Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>,
{
    for attempt in 0..2 {
        let lease = russh.acquire(host).await?;

        // 阶段一：打开通道 + 启动 SFTP 子系统（连接级操作，失败可重连重试）
        let sftp = match tokio::time::timeout(timeout, open_sftp(&lease.handle)).await {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                // 通道/子系统初始化失败：连接可能已失效，清理后重连重试
                russh.invalidate(&host.id, &lease.handle);
                if attempt == 0 {
                    continue;
                }
                return Err(e);
            }
            Err(_) => {
                russh.invalidate(&host.id, &lease.handle);
                if attempt == 0 {
                    continue;
                }
                return Err(crate::i18n::t("操作超时", "Operation timed out").to_string());
            }
        };

        // 阶段二：执行 SFTP 操作（业务级操作，失败不重试，避免重复副作用）
        let op = async {
            let result = f(&sftp).await;
            let _ = sftp.close().await;
            result
        };

        let result = tokio::time::timeout(timeout, op).await;
        russh.touch(&host.id);
        return match result {
            Ok(r) => r,
            Err(_) => {
                // 操作超时：SFTP session 可能处于不一致状态，清理连接避免下次复用出问题
                russh.invalidate(&host.id, &lease.handle);
                Err(crate::i18n::t("操作超时", "Operation timed out").to_string())
            }
        };
    }
    unreachable!()
}

/// 读取目录为结构化条目：跳过 . / ..；符号链接用 stat（follow）解析目标
/// 决定 is_dir / size，悬空链接按非目录处理（size 保留 lstat 值）。
/// 链接解析并发发出（russh-sftp 按 request-id 在同一 SFTP 会话上多路复用），
/// 不再逐链接串行等待——/usr/bin 这类链接密集目录省掉 N 个 RTT。
/// 排序：目录优先，再按名称不区分大小写。
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<SftpEntry>, String> {
    let dir = sftp
        .read_dir(path)
        .await
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取目录失败", "Failed to read the directory")))?;
    let mut entries = Vec::new();
    // (entries 下标, 链接完整路径)：并发解析任务
    let mut link_jobs: Vec<(usize, String)> = Vec::new();
    for entry in dir {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let meta = entry.metadata();
        let is_symlink = meta.file_type().is_symlink();
        let is_dir = meta.file_type().is_dir();
        if is_symlink {
            let joined = if path.ends_with('/') {
                format!("{path}{name}")
            } else {
                format!("{path}/{name}")
            };
            link_jobs.push((entries.len(), joined));
        }
        let full_path = if path.ends_with('/') {
            format!("{path}{name}")
        } else {
            format!("{path}/{name}")
        };
        entries.push(SftpEntry {
            name,
            is_dir,
            is_symlink,
            size: meta.len(),
            mtime: meta.mtime.unwrap_or(0) as u64,
            perms: format!("{}{}", file_type_char(&meta), permission_string(&meta)),
            user: attr_value(meta.user.as_deref(), meta.uid),
            group: attr_value(meta.group.as_deref(), meta.gid),
            sensitive: is_sensitive_remote_path(&full_path),
        });
    }
    // 并发解析所有链接目标；解析失败按悬空链接处理（非目录、保留 lstat size）
    let targets =
        futures_util::future::join_all(link_jobs.iter().map(|(_, p)| sftp.metadata(p)))
            .await;
    for ((idx, _), result) in link_jobs.iter().zip(targets) {
        match result {
            Ok(target) => {
                entries[*idx].is_dir = target.file_type().is_dir();
                entries[*idx].size = target.len();
            }
            Err(_) => entries[*idx].is_dir = false,
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[tauri::command]
pub async fn sftp_list(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
) -> Result<Vec<SftpEntry>, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move { list_dir(sftp, &path).await })
    })
    .await
}

#[tauri::command]
pub async fn sftp_download(
    app: AppHandle,
    registry: State<'_, SftpTransferRegistry>,
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    remote: String,
    local: String,
    transfer_id: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    validate_local_path(&local)?;
    let cancelled = registry.register(&transfer_id, &host_id);
    let result = with_sftp(&russh, &host, TRANSFER_TIMEOUT, |sftp| {
        let remote = remote.clone();
        let local = local.clone();
        let transfer_id = transfer_id.clone();
        let app = app.clone();
        let cancelled = cancelled.clone();
        Box::pin(async move {
            let meta = sftp
                .metadata(remote.as_str())
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("读取远程文件信息失败", "Failed to read remote file info")))?;
            let total = meta.len();
            let mut src = sftp
                .open(remote)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("打开远程文件失败", "Failed to open the remote file")))?;
            // 先写同目录临时文件，成功后再 rename 覆盖目标：
            // 下载失败/取消绝不能截断或删除用户已有的本地文件。
            let existing_perms = tokio::fs::metadata(&local)
                .await
                .ok()
                .map(|meta| meta.permissions());
            let temp = format!(
                "{local}.buffterm-{}.part",
                uuid::Uuid::new_v4().simple()
            );
            let mut dst = tokio::fs::File::create(&temp)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("创建本地临时文件失败", "Failed to create the local temporary file")))?;
            let count = copy_with_progress(
                &mut src,
                &mut dst,
                total,
                &transfer_id,
                "download",
                &app,
                &cancelled,
            )
            .await;
            if count.is_err() {
                let _ = tokio::fs::remove_file(&temp).await;
                return count;
            }
            if let Err(e) = dst.flush().await {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(format!("{}: {e}", crate::i18n::t("刷新本地文件失败", "Failed to flush the local file")));
            }
            let _ = dst.sync_all().await;
            drop(dst);
            let _ = src.close().await;
            if let Err(e) = tokio::fs::rename(&temp, &local).await {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(format!("{}: {e}", crate::i18n::t("替换本地文件失败", "Failed to replace the local file")));
            }
            if let Some(perms) = existing_perms {
                let _ = tokio::fs::set_permissions(&local, perms).await;
            }
            count
        })
    })
    .await;
    registry.remove(&transfer_id);
    let count = result?;
    Ok(success(if crate::i18n::is_en() {
            format!("Downloaded {count} bytes")
        } else {
            format!("已下载 {count} 字节")
        }))
}

#[tauri::command]
pub async fn sftp_upload(
    app: AppHandle,
    registry: State<'_, SftpTransferRegistry>,
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    local: String,
    remote: String,
    transfer_id: String,
    confirmed: Option<bool>,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    validate_local_path(&local)?;
    let confirmed = confirmed.unwrap_or(false);
    let sensitive = is_sensitive_remote_path(&remote);
    if sensitive && !confirmed {
        write_sftp_audit(
            &db, &host, "sftp_upload", &remote, "confirm", "confirm",
            Some("sensitive path; confirmation required"), 0,
        );
        return Err(if crate::i18n::is_en() {
            format!("{remote} is a sensitive path; uploading requires confirmation")
        } else {
            format!("{remote} 属于敏感路径，上传需要确认")
        });
    }
    let started = Instant::now();
    let cancelled = registry.register(&transfer_id, &host_id);
    let result = with_sftp(&russh, &host, TRANSFER_TIMEOUT, |sftp| {
        let local = local.clone();
        let remote = remote.clone();
        let transfer_id = transfer_id.clone();
        let app = app.clone();
        let cancelled = cancelled.clone();
        Box::pin(async move {
            let total = tokio::fs::metadata(&local)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("读取本地文件信息失败", "Failed to read local file info")))?
                .len();
            let mut src = tokio::fs::File::open(&local)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("打开本地文件失败", "Failed to open the local file")))?;
            // 与 write_atomic 一致：符号链接写到真实目标（rename 到链接路径会把链接替换成普通文件），
            // 目标是目录直接拒绝，覆盖已有文件时沿用其权限
            let real = resolve_link(sftp, &remote).await;
            if (is_sensitive_remote_path(&remote) || is_sensitive_remote_path(&real)) && !confirmed {
                return Err(if crate::i18n::is_en() {
                    format!("{remote} is a sensitive path; uploading requires confirmation")
                } else {
                    format!("{remote} 属于敏感路径，上传需要确认")
                });
            }
            let existing = sftp.symlink_metadata(real.as_str()).await.ok();
            if existing.as_ref().is_some_and(|m| m.file_type().is_dir()) {
                return Err(crate::i18n::t("目标是目录，无法写入文件", "The target is a directory; cannot write a file").to_string());
            }
            // 先写同目录临时文件，最终 rename 替换目标：
            // 目标文件在最后一次 rename 前保持原样，中途断连不会留下半个文件
            let temp = temp_path(&real);
            let mut dst = sftp
                .create(temp.as_str())
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("创建远程临时文件失败", "Failed to create the remote temporary file")))?;
            let count = copy_with_progress(
                &mut src,
                &mut dst,
                total,
                &transfer_id,
                "upload",
                &app,
                &cancelled,
            )
            .await;
            if count.is_err() {
                // 取消/失败时清理临时文件（原目标文件未被改动），
                // 并直接返回原始错误，不被 close 的失败阻断/掩盖
                let _ = sftp.remove_file(temp.as_str()).await;
                return count;
            }
            if let Some(meta) = &existing {
                // 沿用原文件权限（best-effort：部分服务端不支持 fsetstat 时忽略）
                let _ = dst
                    .set_metadata(FileAttributes {
                        permissions: meta.permissions,
                        ..Default::default()
                    })
                    .await;
            }
            dst.close()
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("关闭远程文件失败", "Failed to close the remote file")))?;
            replace_with_temp(sftp, temp.as_str(), &real).await?;
            count
        })
    })
    .await;
    registry.remove(&transfer_id);
    let audit_result = match &result {
        Ok(count) => {
            if crate::i18n::is_en() {
                format!("Uploaded {count} bytes")
            } else {
                format!("已上传 {count} 字节")
            }
        }
        Err(e) => e.clone(),
    };
    write_sftp_audit(
        &db,
        &host,
        "sftp_upload",
        &remote,
        if sensitive { "approved" } else { "auto" },
        if result.is_ok() { "ok" } else { "error" },
        Some(&audit_result),
        started.elapsed().as_millis() as u64,
    );
    let count = result?;
    Ok(success(if crate::i18n::is_en() {
            format!("Uploaded {count} bytes")
        } else {
            format!("已上传 {count} 字节")
        }))
}

#[tauri::command]
pub async fn sftp_delete(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
    confirmed: Option<bool>,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    let sensitive = is_sensitive_remote_path(&path);
    if sensitive && !confirmed.unwrap_or(false) {
        write_sftp_audit(&db, &host, "sftp_delete", &path, "confirm", "confirm", Some("sensitive path; confirmation required"), 0);
        return Err(if crate::i18n::is_en() {
            format!("{path} is a sensitive path; deleting requires confirmation")
        } else {
            format!("{path} 属于敏感路径，删除需要确认")
        });
    }
    let started = Instant::now();
    let result = with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            // lstat：符号链接按链接本身删除，不跟随到目标目录
            let meta = sftp
                .symlink_metadata(path.as_str())
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("读取目标信息失败", "Failed to read the target info")))?;
            if meta.file_type().is_dir() {
                sftp.remove_dir(path.as_str())
                    .await
                    .map_err(|e| format!("{}: {e}", crate::i18n::t("删除目录失败", "Failed to delete the directory")))?;
            } else {
                sftp.remove_file(path.as_str())
                    .await
                    .map_err(|e| format!("{}: {e}", crate::i18n::t("删除文件失败", "Failed to delete the file")))?;
            }
            Ok(())
        })
    })
    .await;
    write_sftp_audit(
        &db, &host, "sftp_delete", &path,
        if sensitive { "approved" } else { "auto" },
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(String::as_str),
        started.elapsed().as_millis() as u64,
    );
    result?;
    Ok(success(crate::i18n::t("已删除", "Deleted")))
}

#[tauri::command]
pub async fn sftp_mkdir(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    let sensitive = is_sensitive_remote_path(&path);
    let started = Instant::now();
    let result = with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            sftp.create_dir(path)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("创建目录失败", "Failed to create the directory")))?;
            Ok(())
        })
    })
    .await;
    write_sftp_audit(
        &db, &host, "sftp_mkdir", &path,
        if sensitive { "approved" } else { "auto" },
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(String::as_str),
        started.elapsed().as_millis() as u64,
    );
    result?;
    Ok(success(crate::i18n::t("已创建目录", "Directory created")))
}

#[tauri::command]
pub async fn sftp_rename(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    from: String,
    to: String,
    confirmed: Option<bool>,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    let sensitive = is_sensitive_remote_path(&from) || is_sensitive_remote_path(&to);
    if sensitive && !confirmed.unwrap_or(false) {
        write_sftp_audit(&db, &host, "sftp_rename", &format!("{from} -> {to}"), "confirm", "confirm", Some("sensitive path; confirmation required"), 0);
        return Err(if crate::i18n::is_en() {
            "A sensitive path is involved; renaming requires confirmation".to_string()
        } else {
            "涉及敏感路径，重命名需要确认".to_string()
        });
    }
    let started = Instant::now();
    let result = with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let from = from.clone();
        let to = to.clone();
        Box::pin(async move {
            sftp.rename(from, to)
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("重命名失败", "Rename failed")))?;
            Ok(())
        })
    })
    .await;
    write_sftp_audit(
        &db, &host, "sftp_rename", &format!("{from} -> {to}"),
        if sensitive { "approved" } else { "auto" },
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(String::as_str),
        started.elapsed().as_millis() as u64,
    );
    result?;
    Ok(success(crate::i18n::t("已重命名", "Renamed")))
}

/// 内置编辑器单文件大小上限（2 MiB）：超过该值不读入内存，避免大文件卡死 / 内存膨胀，
/// 前端据此提示改用「下载到本地编辑」。
const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024;

/// 二进制嗅探取样长度（前 8 KiB 内出现 NUL 即判定为二进制）
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// 远端文本文件内容（供中间区内置编辑器打开）。
#[derive(Serialize)]
pub struct SftpFileContent {
    /// 文件文本内容（UTF-8）
    pub content: String,
    /// 文件字节数
    pub size: u64,
    /// 最近修改时间（unix 秒），保存时回传用于检测远端是否被他人改动
    pub mtime: u64,
    /// 是否命中敏感路径（保存时需前端二次确认）
    pub sensitive: bool,
}

/// 保存结果：`requires_confirm` 为 true 表示命中敏感路径且本次未确认，未执行写入。
#[derive(Serialize)]
pub struct SftpWriteOutcome {
    pub ok: bool,
    pub text: String,
    pub requires_confirm: bool,
    /// 写入后远端文件的 mtime（unix 秒），前端据此更新并发修改基线；0 表示未取到
    pub mtime: u64,
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0)
}

/// 敏感路径判定：与 AI/MCP 工具共用同一份规则，避免 SFTP 与 Agent 的
/// 安全口径漂移。命中时保存/删除/重命名/上传都需要确认。
fn is_sensitive_remote_path(path: &str) -> bool {
    crate::safety::is_sensitive_path(path)
}

/// 取远端路径的父目录（`/a/b.txt` → `/a`，`b.txt` → ``）。
fn remote_parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(idx) => &path[..idx],
        None => "",
    }
}

/// 拼接远端路径并归一化 `.` / `..`（用于把相对符号链接目标解析成绝对路径）。
fn join_remote(dir: &str, name: &str) -> String {
    let base = if name.starts_with('/') {
        String::new()
    } else if dir.is_empty() || dir == "/" {
        String::new()
    } else {
        dir.to_string()
    };
    let joined = format!("{base}/{name}");
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// 符号链接解析：保存时若直接 rename 到链接路径会把链接替换成普通文件，
/// 因此先解析到真实目标再写入（相对目标按链接所在目录拼接）。
async fn resolve_link(sftp: &SftpSession, path: &str) -> String {
    // 递归解析嵌套符号链接（最多 8 层），同时用于敏感路径判断与写入目标。
    // 不跟随无限链，避免恶意/损坏链接造成死循环。
    let mut current = path.to_string();
    for _ in 0..8 {
        let Ok(meta) = sftp.symlink_metadata(&current).await else {
            return current;
        };
        if !meta.file_type().is_symlink() {
            return current;
        }
        match sftp.read_link(&current).await {
            Ok(target) => current = join_remote(remote_parent(&current), &target),
            Err(_) => return current,
        }
    }
    current
}

/// 同目录临时文件名：`/etc/nginx.conf` → `/etc/.nginx.conf.buffterm-<uuid>.tmp`
fn temp_path(path: &str) -> String {
    let dir = remote_parent(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    // 相对路径不能拼成 "/.name"（会跑到根目录）；空父目录保持相对语义，
    // 用 "./" 前缀让临时文件与目标仍在同一目录。
    let prefix = if dir.is_empty() {
        "."
    } else if dir == "/" {
        ""
    } else {
        dir
    };
    format!(
        "{prefix}/.{name}.buffterm-{}.tmp",
        uuid::Uuid::new_v4().simple()
    )
}

/// 用临时文件替换目标：多数服务端 rename 直接覆盖；个别实现拒绝已存在目标时，
/// 先把旧目标 rename 到备份，再替换，失败则把备份改回。绝不再“先删目标”：
/// 第二次 rename 失败时原文件/新内容至少有一份保留，并在错误里给出临时路径。
async fn replace_with_temp(sftp: &SftpSession, temp: &str, path: &str) -> Result<(), String> {
    if sftp.rename(temp, path).await.is_ok() {
        return Ok(());
    }
    let backup = format!(
        "{path}.buffterm-backup-{}",
        uuid::Uuid::new_v4().simple()
    );
    let had_backup = sftp.rename(path, &backup).await.is_ok();
    match sftp.rename(temp, path).await {
        Ok(()) => {
            if had_backup {
                let _ = sftp.remove_file(&backup).await;
            }
            Ok(())
        }
        Err(e) => {
            if had_backup {
                // 尽量恢复原目标；恢复失败时备份仍在，不会丢数据。
                let _ = sftp.rename(&backup, path).await;
            }
            Err(format!(
                "{}: {e}（{}: {temp}）",
                crate::i18n::t("替换目标文件失败", "Failed to replace the target file"),
                crate::i18n::t("新内容已保留在临时文件", "the new content is kept in the temporary file")
            ))
        }
    }
}

/// 原子写入：先写同目录临时文件，再 rename 覆盖目标。
/// 直接截断写入时中途断连会留下半个文件，rename 是原子替换，失败也不会破坏原文件。
async fn write_atomic(
    sftp: &SftpSession,
    path: &str,
    bytes: &[u8],
    expected_mtime: Option<u64>,
) -> Result<(), String> {
    let existing = sftp.symlink_metadata(path).await.ok();
    if let Some(meta) = &existing {
        if meta.file_type().is_dir() {
            return Err(crate::i18n::t("目标是目录，无法写入文件", "The target is a directory; cannot write a file").to_string());
        }
        if let Some(expected) = expected_mtime {
            if meta.mtime.unwrap_or(0) as u64 != expected {
                return Err(crate::i18n::t("远端文件已被其它程序修改，请重新打开后再编辑", "The remote file was modified by another program; reopen it before editing").to_string());
            }
        }
    }

    let temp = temp_path(path);
    let write = async {
        let mut file = sftp
            .create(temp.as_str())
            .await
            .map_err(|e| format!("{}: {e}", crate::i18n::t("创建临时文件失败", "Failed to create the temporary file")))?;
        file.write_all(bytes)
            .await
            .map_err(|e| format!("{}: {e}", crate::i18n::t("写入临时文件失败", "Failed to write the temporary file")))?;
        // 沿用原文件权限（best-effort：部分服务端不支持 fsetstat 时忽略）
        if let Some(meta) = &existing {
            let _ = file
                .set_metadata(FileAttributes {
                    permissions: meta.permissions,
                    ..Default::default()
                })
                .await;
        }
        let _ = file.sync_all().await;
        file.close()
            .await
            .map_err(|e| format!("{}: {e}", crate::i18n::t("关闭临时文件失败", "Failed to close the temporary file")))?;
        Ok::<(), String>(())
    };
    if let Err(e) = write.await {
        let _ = sftp.remove_file(temp.as_str()).await;
        return Err(e);
    }

    // 多数服务端的 rename 直接覆盖目标；个别实现会拒绝已存在的目标，
    // 此时退化为「先删后改名」（不再原子，但保证保存成功）；最终失败时保留临时文件供恢复。
    replace_with_temp(sftp, temp.as_str(), path).await
}

/// 读取远端文本文件（供中间区内置编辑器打开）：
/// 解析符号链接后按大小上限与二进制嗅探做前置校验，UTF-8 解码失败同样拒绝。
#[tauri::command]
pub async fn sftp_read_file(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
) -> Result<SftpFileContent, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_browse_sftp(&manager, &russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            let real = resolve_link(sftp, &path).await;
            // 同时检查输入路径与符号链接目标，避免通过 /tmp/link 绕过敏感路径提示
            let sensitive = is_sensitive_remote_path(&path) || is_sensitive_remote_path(&real);
            let meta = sftp
                .metadata(real.as_str())
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("读取远程文件信息失败", "Failed to read remote file info")))?;
            if meta.file_type().is_dir() {
                return Err(crate::i18n::t("目标是目录，无法作为文件打开", "The target is a directory; cannot open it as a file").to_string());
            }
            let size = meta.len();
            if size > MAX_EDIT_BYTES {
                return Err(if crate::i18n::is_en() {
                    format!(
                        "The file size {} exceeds the built-in editor limit {}; download it and edit locally",
                        human_bytes(size),
                        human_bytes(MAX_EDIT_BYTES)
                    )
                } else {
                    format!(
                        "文件大小 {} 超过内置编辑器上限 {}，请下载到本地编辑",
                        human_bytes(size),
                        human_bytes(MAX_EDIT_BYTES)
                    )
                });
            }
            let bytes = sftp
                .read(real.as_str())
                .await
                .map_err(|e| format!("{}: {e}", crate::i18n::t("读取远程文件失败", "Failed to read the remote file")))?;
            // stat 与读取之间文件可能变大，读完再兜底校验一次
            if bytes.len() as u64 > MAX_EDIT_BYTES {
                return Err(if crate::i18n::is_en() {
                    format!(
                        "The file size exceeds the built-in editor limit {}; download it and edit locally",
                        human_bytes(MAX_EDIT_BYTES)
                    )
                } else {
                    format!(
                        "文件大小超过内置编辑器上限 {}，请下载到本地编辑",
                        human_bytes(MAX_EDIT_BYTES)
                    )
                });
            }
            if looks_binary(&bytes) {
                return Err(crate::i18n::t("疑似二进制文件，已拒绝在内置编辑器中打开", "The file appears to be binary; refused to open it in the built-in editor").to_string());
            }
            let content = String::from_utf8(bytes)
                .map_err(|_| crate::i18n::t("文件不是 UTF-8 文本，请下载到本地编辑", "The file is not UTF-8 text; download it and edit locally").to_string())?;
            Ok(SftpFileContent {
                content,
                size,
                mtime: meta.mtime.unwrap_or(0) as u64,
                sensitive,
            })
        })
    })
    .await
}

/// 保存远端文本文件：敏感路径需 `confirmed` 为 true 才写入（前端先弹窗确认），
/// `expected_mtime` 与远端当前 mtime 不一致时拒绝覆盖（防止静默冲掉他人改动）。
/// 每次写入（成功或失败）都写操作审计，弥补 SFTP 通道绕过终端防护的审计缺口。
#[tauri::command]
pub async fn sftp_write_file(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    manager: State<'_, SftpSessionManager>,
    host_id: String,
    path: String,
    content: String,
    expected_mtime: Option<u64>,
    confirmed: bool,
) -> Result<SftpWriteOutcome, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    // 直接命中敏感路径时先返回确认，避免为了弹确认而必须先建立 SFTP 连接；
    // 符号链接目标是否敏感仍需进入会话后解析（见下方闭包）。
    if is_sensitive_remote_path(&path) && !confirmed {
        return Ok(SftpWriteOutcome {
            ok: false,
            text: if crate::i18n::is_en() {
                format!("{path} is a sensitive path; writing requires confirmation")
            } else {
                format!("{path} 属于敏感路径，写入需要确认")
            },
            requires_confirm: true,
            mtime: 0,
        });
    }
    let bytes = content.into_bytes();
    if bytes.len() as u64 > MAX_EDIT_BYTES {
        return Err(if crate::i18n::is_en() {
            format!(
                "The content size exceeds the built-in editor limit {}; write rejected",
                human_bytes(MAX_EDIT_BYTES)
            )
        } else {
            format!(
                "内容大小超过内置编辑器上限 {}，已拒绝写入",
                human_bytes(MAX_EDIT_BYTES)
            )
        });
    }
    let total = bytes.len() as u64;
    let started = Instant::now();
    let result = with_browse_sftp(&manager, &russh, &host, Duration::from_secs(30), |sftp| {
        let path = path.clone();
        let bytes = bytes.clone();
        Box::pin(async move {
            let real = resolve_link(sftp, &path).await;
            // 敏感判断必须同时覆盖“用户输入路径”和“符号链接解析后的真实路径”：
            // /tmp/link -> /etc/shadow 这种写法不能因为输入路径不敏感而漏掉。
            let sensitive = is_sensitive_remote_path(&path) || is_sensitive_remote_path(&real);
            if sensitive && !confirmed {
                return Ok((0, true));
            }
            write_atomic(sftp, &real, &bytes, expected_mtime).await?;
            // 回传写入后的 mtime，前端据此更新后续保存的并发修改基线
            let mtime = sftp
                .metadata(real.as_str())
                .await
                .ok()
                .and_then(|m| m.mtime)
                .unwrap_or(0) as u64;
            Ok((mtime, sensitive))
        })
    })
    .await;

    let (_mtime, sensitive, result_text) = match &result {
        Ok((mtime, sensitive)) => (
            *mtime,
            *sensitive,
            if crate::i18n::is_en() {
                format!("Wrote {total} bytes")
            } else {
                format!("已写入 {} 字节", total)
            },
        ),
        Err(e) => (0, is_sensitive_remote_path(&path), e.clone()),
    };
    let requires_confirm = sensitive && !confirmed;
    let audit_text = if requires_confirm {
        if crate::i18n::is_en() {
            format!("{path} is a sensitive path; confirmation required")
        } else {
            format!("{path} 属于敏感路径，需要确认")
        }
    } else {
        result_text
    };
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: None,
        host_id: host.id.clone(),
        host_label: host.name.clone(),
        tool_name: "sftp_write_file".to_string(),
        source: "sftp".to_string(),
        summary: truncate(&sanitize(&path), 500),
        permission_mode: "edit".to_string(),
        approval: if requires_confirm {
            "confirm"
        } else if sensitive {
            "approved"
        } else {
            "auto"
        }
        .to_string(),
        status: if requires_confirm {
            "confirm"
        } else if result.is_ok() {
            "ok"
        } else {
            "error"
        }
        .to_string(),
        result: Some(truncate(&sanitize(&audit_text), 500)),
        duration_ms: Some(started.elapsed().as_millis() as u64),
    };
    let _ = db.insert_audit_log(&log);

    if requires_confirm {
        return Ok(SftpWriteOutcome {
            ok: false,
            text: if crate::i18n::is_en() {
                format!("{path} is a sensitive path; writing requires confirmation")
            } else {
                format!("{path} 属于敏感路径，写入需要确认")
            },
            requires_confirm: true,
            mtime: 0,
        });
    }
    let (mtime, _) = result?;
    Ok(SftpWriteOutcome {
        ok: true,
        text: if crate::i18n::is_en() {
            format!("Saved ({})", human_bytes(total))
        } else {
            format!("已保存（{}）", human_bytes(total))
        },
        requires_confirm: false,
        mtime,
    })
}

fn file_type_char(meta: &FileAttributes) -> char {
    match meta.file_type() {
        FileType::Dir => 'd',
        FileType::Symlink => 'l',
        FileType::File => '-',
        FileType::Other => '?',
    }
}

fn permission_string(meta: &FileAttributes) -> String {
    let p = meta.permissions();
    format!(
        "{}{}{}{}{}{}{}{}{}",
        if p.owner_read { "r" } else { "-" },
        if p.owner_write { "w" } else { "-" },
        if p.owner_exec { "x" } else { "-" },
        if p.group_read { "r" } else { "-" },
        if p.group_write { "w" } else { "-" },
        if p.group_exec { "x" } else { "-" },
        if p.other_read { "r" } else { "-" },
        if p.other_write { "w" } else { "-" },
        if p.other_exec { "x" } else { "-" },
    )
}

fn attr_value(value: Option<&str>, id: Option<u32>) -> String {
    value
        .filter(|v| !v.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| id.map(|n| n.to_string()).unwrap_or_else(|| "0".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AuthType;

    #[test]
    fn transfer_cancel_for_host_sets_flag() {
        let registry = SftpTransferRegistry::default();
        let flag = registry.register("t1", "h1");
        registry.cancel_for_host("h1");
        assert!(flag.load(Ordering::SeqCst));
        registry.remove("t1");
    }

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

    fn write_known_hosts(port: u16) {
        let kh = std::env::var("BUFFTERM_KNOWN_HOSTS").expect("BUFFTERM_KNOWN_HOSTS 未设置");
        let hostkey =
            std::env::var("BUFFTERM_TEST_SSH_HOSTKEY").expect("BUFFTERM_TEST_SSH_HOSTKEY 未设置");
        let line = std::fs::read_to_string(hostkey).expect("读取主机公钥失败");
        std::fs::write(&kh, format!("[127.0.0.1]:{port} {line}"))
            .expect("写测试 known_hosts 失败");
    }

    #[test]
    fn sensitive_paths_cover_credentials_and_persistence() {
        for p in [
            "/root/.ssh/authorized_keys",
            "/home/alice/.ssh/config",
            "/etc/shadow",
            "/etc/sudoers",
            "/etc/cron.d/backdoor",
            "/var/spool/cron/root",
            "/etc/systemd/system/x.service",
            "/etc/rc.local",
            "/home/alice/.bashrc",
        ] {
            assert!(is_sensitive_remote_path(p), "{p} 应判定为敏感路径");
        }
        for p in [
            "/var/log/nginx/access.log",
            "/etc/nginx/nginx.conf",
            "/home/alice/app.py",
            "/tmp/a.txt",
        ] {
            assert!(!is_sensitive_remote_path(p), "{p} 不应判定为敏感路径");
        }
    }

    #[test]
    fn join_remote_normalizes_dots() {
        assert_eq!(
            join_remote("/etc/nginx", "../sites-enabled/foo"),
            "/etc/sites-enabled/foo"
        );
        assert_eq!(join_remote("/etc", "./a/b"), "/etc/a/b");
        assert_eq!(join_remote("/", "x"), "/x");
        assert_eq!(join_remote("/etc/nginx", "/abs/path"), "/abs/path");
    }

    #[test]
    fn temp_path_stays_in_same_directory() {
        let t = temp_path("/etc/nginx/nginx.conf");
        assert!(t.starts_with("/etc/nginx/.nginx.conf.buffterm-"), "{t}");
        assert!(t.ends_with(".tmp"), "{t}");
        let t = temp_path("/a.txt");
        assert!(t.starts_with("/.a.txt.buffterm-"), "{t}");
        let t = temp_path("a.txt");
        assert!(t.starts_with("./.a.txt.buffterm-"), "相对路径不应落到根目录: {t}");
    }

    #[test]
    fn looks_binary_detects_nul_in_head() {
        assert!(!looks_binary(b"hello\nworld"));
        assert!(looks_binary(b"\x00\x01\x02"));
        // 嗅探窗口之外出现的 NUL 不影响判定（只取样前 8 KiB）
        let mut data = vec![b'a'; BINARY_SNIFF_BYTES + 1];
        data.push(0);
        assert!(!looks_binary(&data));
    }

    #[test]
    fn human_bytes_formats_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
    }

    /// 集成测试（默认 ignore）：验证内置编辑器的读写链路——符号链接保留、
    /// 权限沿用、原子写不留临时文件、mtime 冲突拒绝覆盖。环境变量同上。
    ///   cargo test sftp::tests::local_sshd_edit_roundtrip -- --ignored
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn local_sshd_edit_roundtrip() {
        use std::os::unix::fs::PermissionsExt;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        rt.block_on(async {
            let host = test_host(
                std::env::var("BUFFTERM_TEST_SSH_KEY").expect("BUFFTERM_TEST_SSH_KEY 未设置"),
            );
            write_known_hosts(host.port);

            let fixture = "/tmp/buffterm-sshd-test/sftp-edit";
            let _ = std::fs::remove_dir_all(fixture);
            std::fs::create_dir_all(fixture).expect("创建 fixture 失败");
            let target = format!("{fixture}/app.conf");
            std::fs::write(&target, b"port=80\n").expect("写 fixture 文件失败");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
                .expect("设置 fixture 权限失败");
            let link = format!("{fixture}/link.conf");
            std::os::unix::fs::symlink("app.conf", &link).unwrap();

            let manager = RusshManager::new();

            // 1) 经符号链接写入：链接保留，内容落到目标文件，权限沿用
            let mtime = with_sftp(&manager, &host, Duration::from_secs(20), |sftp| {
                let link = link.clone();
                Box::pin(async move {
                    let real = resolve_link(sftp, &link).await;
                    write_atomic(sftp, &real, b"port=8080\n", None).await?;
                    Ok::<u64, String>(
                        sftp.metadata(real.as_str())
                            .await
                            .map_err(|e| e.to_string())?
                            .mtime
                            .unwrap_or(0) as u64,
                    )
                })
            })
            .await
            .expect("写入失败");

            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "符号链接应保留"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "port=8080\n");
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640, "写入后应保留原文件权限");

            // 2) mtime 冲突：expected 与远端不一致时拒绝覆盖，内容不变
            let stale = with_sftp(&manager, &host, Duration::from_secs(20), |sftp| {
                let target = target.clone();
                Box::pin(async move { write_atomic(sftp, &target, b"x\n", Some(mtime + 999)).await })
            })
            .await;
            assert!(stale.is_err(), "过期 mtime 应拒绝写入: {stale:?}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "port=8080\n");

            // 3) 目录内不残留临时文件
            let leftovers: Vec<String> = std::fs::read_dir(fixture)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.contains("buffterm-"))
                .collect();
            assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
        });
    }

    /// 集成测试（默认 ignore）：需要本机临时 sshd（环境变量与
    /// russh::tests::local_sshd_end_to_end 相同）。fixture 建在沙盒目录下，
    /// 覆盖：含连续空格文件名、目录符号链接（is_dir 取目标）、文件链接、
    /// 悬空链接、目录优先排序。
    ///   /usr/sbin/sshd -D -e -f /tmp/buffterm-sshd-test/sshd_config
    ///   BUFFTERM_TEST_SSH_PORT=2222 BUFFTERM_TEST_SSH_USER=$USER \
    ///   BUFFTERM_TEST_SSH_KEY=/tmp/buffterm-sshd-test/id_rsa \
    ///   BUFFTERM_TEST_SSH_HOSTKEY=/tmp/buffterm-sshd-test/ssh_host_ed25519_key.pub \
    ///   BUFFTERM_KNOWN_HOSTS=/tmp/buffterm-sshd-test/known_hosts \
    ///   cargo test sftp -- --ignored
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn local_sshd_list() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        rt.block_on(async {
            let host = test_host(
                std::env::var("BUFFTERM_TEST_SSH_KEY").expect("BUFFTERM_TEST_SSH_KEY 未设置"),
            );
            write_known_hosts(host.port);

            // 沙盒 sshd 跑在本机，远端路径即本地路径，直接铺 fixture
            let fixture = "/tmp/buffterm-sshd-test/sftp-fixture";
            let _ = std::fs::remove_dir_all(fixture);
            std::fs::create_dir_all(format!("{fixture}/sub")).expect("创建 fixture 失败");
            std::fs::write(format!("{fixture}/a b  c.txt"), b"hi").expect("写 fixture 文件失败");
            std::os::unix::fs::symlink("sub", format!("{fixture}/link_dir")).unwrap();
            std::os::unix::fs::symlink("a b  c.txt", format!("{fixture}/link_file")).unwrap();
            std::os::unix::fs::symlink("nope", format!("{fixture}/dangling")).unwrap();

            let manager = RusshManager::new();
            let entries = with_sftp(&manager, &host, Duration::from_secs(20), |sftp| {
                Box::pin(async move { list_dir(sftp, fixture).await })
            })
            .await
            .expect("sftp_list 失败");

            assert_eq!(entries.len(), 5, "条目数: {:?}", entries.iter().map(|e| &e.name).collect::<Vec<_>>());
            let by_name = |n: &str| entries.iter().find(|e| e.name == n).expect(n);

            let e = by_name("link_dir");
            assert!(e.is_symlink && e.is_dir, "link_dir 应解析为目录");
            let e = by_name("link_file");
            assert!(e.is_symlink && !e.is_dir, "link_file 应为文件链接");
            let e = by_name("dangling");
            assert!(e.is_symlink && !e.is_dir, "悬空链接应按非目录处理");
            by_name("a b  c.txt"); // 名称含连续两个空格，原样保留

            // 目录优先（link_dir 按目标也算目录），再按名称不区分大小写
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(
                names,
                ["link_dir", "sub", "a b  c.txt", "dangling", "link_file"],
            );
        });
    }
}
