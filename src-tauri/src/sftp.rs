use crate::models::Host;
use crate::russh::do_connect;
use chrono::{DateTime, Utc};
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

fn success(text: impl Into<String>) -> SftpResult {
    SftpResult {
        ok: true,
        text: text.into(),
    }
}

/// 进行中传输的取消标志（transfer_id -> flag），供前端随时取消。
/// 复用 inspection.rs / remediation.rs 的 CancelFlags 模式。
#[derive(Default)]
pub struct SftpTransferRegistry {
    flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl SftpTransferRegistry {
    fn register(&self, id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.flags
            .lock()
            .unwrap()
            .insert(id.to_string(), flag.clone());
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
    fn remove(&self, id: &str) {
        self.flags.lock().unwrap().remove(id);
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
pub async fn sftp_exists(host: Host, path: String) -> Result<bool, String> {
    with_sftp(&host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move { Ok(sftp.metadata(path.as_str()).await.is_ok()) })
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
            return Err("传输已取消".to_string());
        }
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取数据失败: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入数据失败: {e}"))?;
        transferred += n as u64;
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            emit(transferred);
            last_emit = Instant::now();
        }
    }
    emit(transferred);
    Ok(transferred)
}

async fn with_sftp<F, T>(host: &Host, timeout: Duration, f: F) -> Result<T, String>
where
    F: for<'a> Fn(&'a SftpSession) -> Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>,
{
    let handle = tokio::time::timeout(Duration::from_secs(15), do_connect(host, None))
        .await
        .map_err(|_| "SSH 连接超时（15 秒）".to_string())??;

    let op = async {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| format!("打开 SFTP 通道失败: {e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("启动 SFTP 子系统失败: {e}"))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("初始化 SFTP 失败: {e}"))?;
        let result = f(&sftp).await;
        let _ = sftp.close().await;
        result
    };

    tokio::time::timeout(timeout, op)
        .await
        .map_err(|_| "操作超时".to_string())?
}

#[tauri::command]
pub async fn sftp_list(host: Host, path: String) -> Result<SftpResult, String> {
    let text = with_sftp(&host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            let dir = sftp
                .read_dir(path)
                .await
                .map_err(|e| format!("读取目录失败: {e}"))?;
            let mut lines = Vec::new();
            for entry in dir {
                let meta = entry.metadata();
                let name = entry.file_name();
                let perms = format!("{}{}", file_type_char(&meta), permission_string(&meta));
                let user = attr_value(meta.user.as_deref(), meta.uid);
                let group = attr_value(meta.group.as_deref(), meta.gid);
                lines.push(format!(
                    "{perms} 1 {user} {group} {} {} {name}",
                    meta.len(),
                    mtime_string(&meta)
                ));
            }
            Ok(lines.join("\n"))
        })
    })
    .await?;
    Ok(success(text))
}

#[tauri::command]
pub async fn sftp_download(
    app: AppHandle,
    registry: State<'_, SftpTransferRegistry>,
    host: Host,
    remote: String,
    local: String,
    transfer_id: String,
) -> Result<SftpResult, String> {
    let cancelled = registry.register(&transfer_id);
    let result = with_sftp(&host, TRANSFER_TIMEOUT, |sftp| {
        let remote = remote.clone();
        let local = local.clone();
        let transfer_id = transfer_id.clone();
        let app = app.clone();
        let cancelled = cancelled.clone();
        Box::pin(async move {
            let meta = sftp
                .metadata(remote.as_str())
                .await
                .map_err(|e| format!("读取远程文件信息失败: {e}"))?;
            let total = meta.len();
            let mut src = sftp
                .open(remote)
                .await
                .map_err(|e| format!("打开远程文件失败: {e}"))?;
            let mut dst = tokio::fs::File::create(&local)
                .await
                .map_err(|e| format!("创建本地文件失败: {e}"))?;
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
            dst.flush()
                .await
                .map_err(|e| format!("刷新本地文件失败: {e}"))?;
            let _ = src.close().await;
            if count.is_err() {
                // 取消/失败时清理半成品本地文件（尽力而为）
                let _ = tokio::fs::remove_file(&local).await;
            }
            count
        })
    })
    .await;
    registry.remove(&transfer_id);
    let count = result?;
    Ok(success(format!("已下载 {count} 字节")))
}

#[tauri::command]
pub async fn sftp_upload(
    app: AppHandle,
    registry: State<'_, SftpTransferRegistry>,
    host: Host,
    local: String,
    remote: String,
    transfer_id: String,
) -> Result<SftpResult, String> {
    let cancelled = registry.register(&transfer_id);
    let result = with_sftp(&host, TRANSFER_TIMEOUT, |sftp| {
        let local = local.clone();
        let remote = remote.clone();
        let transfer_id = transfer_id.clone();
        let app = app.clone();
        let cancelled = cancelled.clone();
        Box::pin(async move {
            let total = tokio::fs::metadata(&local)
                .await
                .map_err(|e| format!("读取本地文件信息失败: {e}"))?
                .len();
            let mut src = tokio::fs::File::open(&local)
                .await
                .map_err(|e| format!("打开本地文件失败: {e}"))?;
            let mut dst = sftp
                .create(&remote)
                .await
                .map_err(|e| format!("创建远程文件失败: {e}"))?;
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
            dst.close()
                .await
                .map_err(|e| format!("关闭远程文件失败: {e}"))?;
            if count.is_err() {
                // 取消/失败时清理半成品远程文件（尽力而为）
                let _ = sftp.remove_file(&remote).await;
            }
            count
        })
    })
    .await;
    registry.remove(&transfer_id);
    let count = result?;
    Ok(success(format!("已上传 {count} 字节")))
}

#[tauri::command]
pub async fn sftp_delete(host: Host, path: String) -> Result<SftpResult, String> {
    with_sftp(&host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            let meta = sftp
                .metadata(path.as_str())
                .await
                .map_err(|e| format!("读取目标信息失败: {e}"))?;
            if meta.file_type().is_dir() {
                sftp.remove_dir(path.as_str())
                    .await
                    .map_err(|e| format!("删除目录失败: {e}"))?;
            } else {
                sftp.remove_file(path.as_str())
                    .await
                    .map_err(|e| format!("删除文件失败: {e}"))?;
            }
            Ok(())
        })
    })
    .await?;
    Ok(success("已删除"))
}

#[tauri::command]
pub async fn sftp_mkdir(host: Host, path: String) -> Result<SftpResult, String> {
    with_sftp(&host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            sftp.create_dir(path)
                .await
                .map_err(|e| format!("创建目录失败: {e}"))?;
            Ok(())
        })
    })
    .await?;
    Ok(success("已创建目录"))
}

#[tauri::command]
pub async fn sftp_rename(host: Host, from: String, to: String) -> Result<SftpResult, String> {
    with_sftp(&host, Duration::from_secs(20), |sftp| {
        let from = from.clone();
        let to = to.clone();
        Box::pin(async move {
            sftp.rename(from, to)
                .await
                .map_err(|e| format!("重命名失败: {e}"))?;
            Ok(())
        })
    })
    .await?;
    Ok(success("已重命名"))
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

fn mtime_string(meta: &FileAttributes) -> String {
    let secs = meta.mtime.unwrap_or(0) as i64;
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.format("%b %d %H:%M").to_string())
        .unwrap_or_else(|| "Jan 01 00:00".to_string())
}
