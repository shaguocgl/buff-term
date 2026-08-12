use crate::models::Host;
use russh::client::{self, Config, Handle};
use russh::keys::{self, key::PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

pub struct ExecResult {
    pub text: String,
    pub exit_code: Option<u32>,
    pub timed_out: bool,
}

/// 自定义 Handler：用 ~/.ssh/known_hosts 校验服务器主机密钥
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
            Ok(matched) => Ok(matched),
            Err(_) => Ok(false),
        }
    }
}

type ConnSlot = Arc<tokio::sync::Mutex<Option<Handle<ClientHandler>>>>;

/// 按主机复用的 russh 连接池
#[derive(Default)]
pub struct RusshManager {
    conns: Mutex<HashMap<String, ConnSlot>>,
}

impl RusshManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, host: &Host) -> ConnSlot {
        let mut map = self.conns.lock().unwrap();
        map.entry(host.id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
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
                Ok(r) => return Ok(r),
                Err(_) => {
                    // 连接失效，丢弃并重连
                    *guard = None;
                }
            }
        }

        let mut handle = connect(host).await?;
        let result = exec_on(&mut handle, command, timeout).await?;
        *guard = Some(handle);
        Ok(result)
    }

    /// 测试主机连接：连接 + 认证，成功返回提示
    pub async fn test_connection(
        &self,
        host: &Host,
        password: Option<String>,
    ) -> Result<String, String> {
        let _handle = tokio::time::timeout(
            Duration::from_secs(10),
            do_connect(host, password),
        )
        .await
        .map_err(|_| "连接超时（10 秒）".to_string())??;
        Ok(format!(
            "连接成功（{}@{}:{}）",
            host.username, host.address, host.port
        ))
    }
}

async fn connect(host: &Host) -> Result<Handle<ClientHandler>, String> {
    tokio::time::timeout(Duration::from_secs(15), do_connect(host, None))
        .await
        .map_err(|_| "SSH 连接超时（15 秒）".to_string())?
}

async fn do_connect(
    host: &Host,
    password_override: Option<String>,
) -> Result<Handle<ClientHandler>, String> {
    let mut config = Config::default();
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
        if e.to_string().to_lowercase().contains("host key")
            || e.to_string().to_lowercase().contains("fingerprint")
        {
            format!("SSH 连接失败：主机指纹未通过校验，请先在终端中连接一次确认指纹（{e}）")
        } else {
            format!("SSH 连接失败: {e}")
        }
    })?;

    let success = if host.auth_type == "password" {
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
            Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => stdout.extend_from_slice(&data),
            Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let _ = channel.close().await;
    Ok(ExecResult {
        text: String::from_utf8_lossy(&stdout).to_string(),
        exit_code,
        timed_out,
    })
}

fn default_key_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = format!("{home}/.ssh/{name}");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    format!("{home}/.ssh/id_ed25519")
}

fn default_known_hosts_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".ssh/known_hosts")
}
