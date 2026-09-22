use crate::db::Db;
use crate::models::Host;
use crate::russh::RusshManager;
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
}

fn success(text: impl Into<String>) -> SftpResult {
    SftpResult {
        ok: true,
        text: text.into(),
    }
}

/// 校验本地路径：必须为绝对路径，且拒绝凭据类敏感目录，
/// 防止恶意前端/注入把私钥、云凭据等本地文件上传到远端，
/// 或覆盖下载到敏感位置。
fn validate_local_path(local: &str) -> Result<(), String> {
    if !std::path::Path::new(local).is_absolute() {
        return Err("本地路径必须是绝对路径".to_string());
    }
    let lower = local.to_ascii_lowercase();
    for bad in [
        "/.ssh/",
        "\\.ssh\\",
        "/.gnupg/",
        "\\.gnupg\\",
        "/.aws/",
        "\\.aws\\",
        "/.azure/",
    ] {
        if lower.contains(bad) {
            return Err(format!("本地路径包含敏感目录（{bad}），已拒绝该操作"));
        }
    }
    Ok(())
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
pub async fn sftp_exists(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    path: String,
) -> Result<bool, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_sftp(&russh, &host, Duration::from_secs(20), |sftp| {
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
        let sftp_setup = async {
            let channel = lease
                .handle
                .channel_open_session()
                .await
                .map_err(|e| format!("打开 SFTP 通道失败: {e}"))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| format!("启动 SFTP 子系统失败: {e}"))?;
            SftpSession::new(channel.into_stream())
                .await
                .map_err(|e| format!("初始化 SFTP 失败: {e}"))
        };

        let sftp = match tokio::time::timeout(timeout, sftp_setup).await {
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
                return Err("操作超时".to_string());
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
                Err("操作超时".to_string())
            }
        };
    }
    unreachable!()
}

/// 读取目录为结构化条目：跳过 . / ..；符号链接用 stat（follow）解析目标
/// 决定 is_dir / size，悬空链接按非目录处理（size 保留 lstat 值）。
/// 排序：目录优先，再按名称不区分大小写。
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<SftpEntry>, String> {
    let dir = sftp
        .read_dir(path)
        .await
        .map_err(|e| format!("读取目录失败: {e}"))?;
    let mut entries = Vec::new();
    for entry in dir {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let meta = entry.metadata();
        let is_symlink = meta.file_type().is_symlink();
        let mut is_dir = meta.file_type().is_dir();
        let mut size = meta.len();
        if is_symlink {
            let joined = if path.ends_with('/') {
                format!("{path}{name}")
            } else {
                format!("{path}/{name}")
            };
            if let Ok(target) = sftp.metadata(joined.as_str()).await {
                is_dir = target.file_type().is_dir();
                size = target.len();
            } else {
                is_dir = false;
            }
        }
        entries.push(SftpEntry {
            name,
            is_dir,
            is_symlink,
            size,
            mtime: meta.mtime.unwrap_or(0) as u64,
            perms: format!("{}{}", file_type_char(&meta), permission_string(&meta)),
            user: attr_value(meta.user.as_deref(), meta.uid),
            group: attr_value(meta.group.as_deref(), meta.gid),
        });
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
    host_id: String,
    path: String,
) -> Result<Vec<SftpEntry>, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_sftp(&russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move { list_dir(&sftp, &path).await })
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
    let cancelled = registry.register(&transfer_id);
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
            if count.is_err() {
                // 取消/失败时先清理半成品本地文件（尽力而为），
                // 并直接返回原始错误，不被收尾操作（flush/close）掩盖
                let _ = tokio::fs::remove_file(&local).await;
                return count;
            }
            dst.flush()
                .await
                .map_err(|e| format!("刷新本地文件失败: {e}"))?;
            let _ = src.close().await;
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
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    local: String,
    remote: String,
    transfer_id: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    validate_local_path(&local)?;
    let cancelled = registry.register(&transfer_id);
    let result = with_sftp(&russh, &host, TRANSFER_TIMEOUT, |sftp| {
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
            if count.is_err() {
                // 取消/失败时先清理半成品远端文件，并直接返回原始错误，
                // 不被 close 的失败阻断/掩盖
                let _ = sftp.remove_file(&remote).await;
                return count;
            }
            dst.close()
                .await
                .map_err(|e| format!("关闭远程文件失败: {e}"))?;
            count
        })
    })
    .await;
    registry.remove(&transfer_id);
    let count = result?;
    Ok(success(format!("已上传 {count} 字节")))
}

#[tauri::command]
pub async fn sftp_delete(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    path: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_sftp(&russh, &host, Duration::from_secs(20), |sftp| {
        let path = path.clone();
        Box::pin(async move {
            // lstat：符号链接按链接本身删除，不跟随到目标目录
            let meta = sftp
                .symlink_metadata(path.as_str())
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
pub async fn sftp_mkdir(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    path: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_sftp(&russh, &host, Duration::from_secs(20), |sftp| {
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
pub async fn sftp_rename(
    db: State<'_, Arc<Db>>,
    russh: State<'_, RusshManager>,
    host_id: String,
    from: String,
    to: String,
) -> Result<SftpResult, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    with_sftp(&russh, &host, Duration::from_secs(20), |sftp| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AuthType;

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
