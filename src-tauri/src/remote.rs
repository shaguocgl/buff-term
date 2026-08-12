use crate::models::Host;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct RemoteOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// ssh 认证提示行，如 root@1.2.3.4's password: / Enter passphrase for key ...
static AUTH_PROMPT_RE: OnceLock<Regex> = OnceLock::new();

fn auth_prompt_re() -> &'static Regex {
    AUTH_PROMPT_RE.get_or_init(|| {
        Regex::new(r"(?m)^.*('s password:|passphrase for key .*:)\s*$").unwrap()
    })
}

/// 通过一个独立 ssh 进程执行远程命令（非交互），自动响应密码提示，带超时。
pub fn run(host: &Host, command: &str, timeout_secs: u64) -> Result<RemoteOutput, String> {
    run_program(
        &host.id,
        host.auth_type == "password",
        "ssh",
        &host.ssh_args(),
        &[command.to_string()],
        None,
        timeout_secs,
    )
}

/// 通过指定程序（ssh / sftp）执行，stdin 可写入批处理脚本。
pub fn run_program(
    host_id: &str,
    needs_password: bool,
    program: &str,
    base_args: &[String],
    extra_args: &[String],
    stdin_script: Option<&str>,
    timeout_secs: u64,
) -> Result<RemoteOutput, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 60,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("创建 PTY 失败: {e}"))?;
    let master = pair.master;

    let mut cmd = CommandBuilder::new(program);
    for arg in base_args {
        cmd.arg(arg);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("启动 ssh 失败: {e}"))?;
    let reader = master
        .try_clone_reader()
        .map_err(|e| format!("获取 PTY 读取端失败: {e}"))?;
    let writer = master
        .take_writer()
        .map_err(|e| format!("获取 PTY 写入端失败: {e}"))?;
    let auto_password = crate::credentials::get_password(host_id);
    let auto_password_thread = auto_password.clone();
    let script_owned: Option<String> = stdin_script.map(String::from);

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut writer = writer;
        // 密钥认证（无需密码）：脚本可以立即写入
        if !needs_password {
            if let Some(script) = &script_owned {
                let _ = writer.write_all(script.as_bytes());
                let _ = writer.flush();
            }
        }
        let mut buf = [0u8; 8192];
        let mut scan = String::new();
        let mut sent_secret = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if let Some(secret) = &auto_password_thread {
                        scan.push_str(&String::from_utf8_lossy(&data));
                        if scan.len() > 4096 {
                            let cut = scan.len() - 4096;
                            let mut start = cut;
                            while start > 0 && !scan.is_char_boundary(start) {
                                start -= 1;
                            }
                            scan = scan[start..].to_string();
                        }
                        let lower = scan.to_ascii_lowercase();
                        if !sent_secret
                            && (lower.contains("password:") || lower.contains("passphrase for key"))
                        {
                            let _ = writer.write_all(format!("{secret}\r").as_bytes());
                            let _ = writer.flush();
                            sent_secret = true;
                            scan.clear();
                            // 密码认证通过后再写入脚本，避免脚本被当成密码
                            if let Some(script) = &script_owned {
                                let _ = writer.write_all(script.as_bytes());
                                let _ = writer.flush();
                            }
                        }
                    }
                    if tx.send(data).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut output = Vec::<u8>::new();
    let mut auth_failed = false;
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        let remaining = timeout_secs.saturating_sub(started.elapsed().as_secs());
        if remaining == 0 {
            timed_out = true;
            break;
        }
        match rx.recv_timeout(Duration::from_secs(remaining)) {
            Ok(data) => {
                output.extend_from_slice(&data);
                // 需要密码但未保存：立即失败，避免挂到超时
                if auto_password.is_none()
                    && auth_prompt_re().is_match(&String::from_utf8_lossy(&output))
                {
                    auth_failed = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if auth_failed {
        let _ = child.kill();
        return Err(
            "服务器要求密码认证，但该主机未保存密码。请在主机列表编辑该主机，填写密码（保存到系统钥匙串）。"
                .to_string(),
        );
    }
    if timed_out {
        let _ = child.kill();
    }
    let exit_code = child.wait().ok().map(|s| s.exit_code() as i32);

    let raw = String::from_utf8_lossy(&output);
    Ok(RemoteOutput {
        text: auth_prompt_re()
            .replace_all(&clean_output(&raw), "")
            .to_string(),
        exit_code,
        timed_out,
    })
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
