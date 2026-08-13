use crate::models::Host;
use crate::russh::do_connect;
use chrono::{DateTime, Utc};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

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
    host: Host,
    remote: String,
    local: String,
) -> Result<SftpResult, String> {
    let count = with_sftp(&host, Duration::from_secs(120), |sftp| {
        let remote = remote.clone();
        let local = local.clone();
        Box::pin(async move {
            let mut src = sftp
                .open(remote)
                .await
                .map_err(|e| format!("打开远程文件失败: {e}"))?;
            let mut dst = tokio::fs::File::create(local)
                .await
                .map_err(|e| format!("创建本地文件失败: {e}"))?;
            let count = tokio::io::copy(&mut src, &mut dst)
                .await
                .map_err(|e| format!("下载失败: {e}"))?;
            dst.flush()
                .await
                .map_err(|e| format!("刷新本地文件失败: {e}"))?;
            let _ = src.close().await;
            Ok(count)
        })
    })
    .await?;
    Ok(success(format!("已下载 {count} 字节")))
}

#[tauri::command]
pub async fn sftp_upload(host: Host, local: String, remote: String) -> Result<SftpResult, String> {
    let count = with_sftp(&host, Duration::from_secs(120), |sftp| {
        let local = local.clone();
        let remote = remote.clone();
        Box::pin(async move {
            let mut src = tokio::fs::File::open(local)
                .await
                .map_err(|e| format!("打开本地文件失败: {e}"))?;
            let mut dst = sftp
                .create(remote)
                .await
                .map_err(|e| format!("创建远程文件失败: {e}"))?;
            let count = tokio::io::copy(&mut src, &mut dst)
                .await
                .map_err(|e| format!("上传失败: {e}"))?;
            dst.close()
                .await
                .map_err(|e| format!("关闭远程文件失败: {e}"))?;
            Ok(count)
        })
    })
    .await?;
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
