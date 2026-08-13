use crate::models::Host;
use regex::Regex;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct RemoteOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

fn clean_output(raw: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]")
            .unwrap()
    });
    let cleaned = re.replace_all(raw, "");
    cleaned.replace("\r\n", "\n").replace('\r', "\n")
}

/// 通过 sftp 批处理执行（不依赖 PTY 提示符猜测）：
/// 密码认证用 SSH_ASKPASS 注入，未保存密码时给出明确错误。
pub fn run_sftp_batch(
    host: &Host,
    args: &[String],
    script: &str,
    timeout_secs: u64,
) -> Result<RemoteOutput, String> {
    let mut cmd = std::process::Command::new("sftp");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut askpass_path: Option<std::path::PathBuf> = None;
    if host.auth_type == "password" {
        let password = crate::credentials::get_password(&host.id).ok_or_else(|| {
            "服务器要求密码认证，但该主机未保存密码。请连接一次服务器并在终端输入密码（会自动保存），\
             或在主机编辑中保存密码。"
                .to_string()
        })?;
        askpass_path = Some(write_askpass(&password)?);
        let path = askpass_path.clone().unwrap();
        cmd.env("SSH_ASKPASS", &path);
        cmd.env("SSH_ASKPASS_REQUIRE", "force");
        cmd.env("DISPLAY", ":0");
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 sftp 失败: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
        let _ = stdin.flush();
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法获取 sftp stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法获取 sftp stderr".to_string())?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.as_bytes().to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
        let _ = stderr_tx.send(buf);
    });

    let mut output = Vec::<u8>::new();
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        let remaining = timeout_secs.saturating_sub(started.elapsed().as_secs());
        if remaining == 0 {
            timed_out = true;
            break;
        }
        match rx.recv_timeout(Duration::from_secs(remaining)) {
            Ok(data) => output.extend_from_slice(&data),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if timed_out {
        let _ = child.kill();
    }
    let exit_code = child
        .wait()
        .ok()
        .map(|s| s.code().unwrap_or(1));

    if let Some(path) = &askpass_path {
        let _ = std::fs::remove_file(path);
    }

    let mut text = String::from_utf8_lossy(&output).to_string();
    if let Ok(stderr_text) = stderr_rx.recv_timeout(Duration::from_secs(2)) {
        text.push_str(&String::from_utf8_lossy(&stderr_text));
    }
    let lower = text.to_lowercase();
    if exit_code != Some(0) && lower.contains("permission denied") {
        return Err(
            "密码认证失败（Permission denied）：请检查该主机在钥匙串中保存的密码是否正确。\
             可重新连接服务器输入密码（会自动更新），或在主机编辑中重新保存。"
                .to_string(),
        );
    }
    Ok(RemoteOutput {
        text: clean_output(&text),
        exit_code,
        timed_out,
    })
}

/// 生成 SSH_ASKPASS 辅助脚本（临时文件，用完即删）
fn write_askpass(password: &str) -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir();
    let token = uuid::Uuid::new_v4().to_string();
    let pass_file = dir.join(format!("kw_askpass_{token}.pass"));
    let askpass = dir.join(format!("kw_askpass_{token}.sh"));
    std::fs::write(&pass_file, password)
        .map_err(|e| format!("写入临时密码文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&pass_file, std::fs::Permissions::from_mode(0o600));
    }
    let script = format!(
        "#!/bin/sh\ncat '{}'\n",
        pass_file.to_string_lossy().replace('\'', "'\\''")
    );
    std::fs::write(&askpass, script)
        .map_err(|e| format!("写入 askpass 脚本失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o755));
    }
    Ok(askpass)
}
