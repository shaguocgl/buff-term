use crate::models::Host;
use crate::remote;
use serde::Serialize;

#[derive(Serialize)]
pub struct SftpResult {
    pub ok: bool,
    pub text: String,
}

fn sftp_args(host: &Host) -> Vec<String> {
    let mut args = vec![
        "-F".to_string(),
        crate::models::null_config_path().to_string(),
        "-b".to_string(),
        "-".to_string(),
        "-o".to_string(),
        "SendEnv -LC_* -LANG".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-P".to_string(),
        host.port.to_string(),
    ];
    if let Some(key) = &host.key_path {
        if !key.trim().is_empty() {
            args.push("-i".to_string());
            args.push(key.clone());
        }
    }
    if let Some(jump) = &host.proxy_jump {
        if !jump.trim().is_empty() {
            args.push("-J".to_string());
            args.push(jump.clone());
        }
    }
    args.push(format!("{}@{}", host.username, host.address));
    args
}

fn sftp_quote(p: &str) -> String {
    format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""))
}

fn finish(out: &remote::RemoteOutput) -> Result<SftpResult, String> {
    if out.timed_out {
        return Ok(SftpResult {
            ok: false,
            text: "操作超时".to_string(),
        });
    }
    let text = out.text.trim().to_string();
    Ok(SftpResult {
        ok: out.exit_code == Some(0),
        text,
    })
}

fn run_sftp(host: &Host, script: &str, timeout: u64) -> Result<remote::RemoteOutput, String> {
    remote::run_program(
        &host.id,
        host.auth_type == "password",
        "sftp",
        &[],
        &sftp_args(host),
        Some(script),
        timeout,
    )
}

#[tauri::command]
pub fn sftp_list(host: Host, path: String) -> Result<SftpResult, String> {
    let script = format!("ls -la {}\nbye\n", sftp_quote(&path));
    let out = run_sftp(&host, &script, 20)?;
    finish(&out)
}

#[tauri::command]
pub fn sftp_download(host: Host, remote: String, local: String) -> Result<SftpResult, String> {
    let script = format!("get {} {}\nbye\n", sftp_quote(&remote), sftp_quote(&local));
    let out = run_sftp(&host, &script, 120)?;
    finish(&out)
}

#[tauri::command]
pub fn sftp_upload(host: Host, local: String, remote: String) -> Result<SftpResult, String> {
    let script = format!("put {} {}\nbye\n", sftp_quote(&local), sftp_quote(&remote));
    let out = run_sftp(&host, &script, 120)?;
    finish(&out)
}

#[tauri::command]
pub fn sftp_delete(host: Host, path: String) -> Result<SftpResult, String> {
    let script = format!("rm {}\nbye\n", sftp_quote(&path));
    let out = run_sftp(&host, &script, 20)?;
    finish(&out)
}

#[tauri::command]
pub fn sftp_mkdir(host: Host, path: String) -> Result<SftpResult, String> {
    let script = format!("mkdir {}\nbye\n", sftp_quote(&path));
    let out = run_sftp(&host, &script, 20)?;
    finish(&out)
}

#[tauri::command]
pub fn sftp_rename(host: Host, from: String, to: String) -> Result<SftpResult, String> {
    let script = format!("rename {} {}\nbye\n", sftp_quote(&from), sftp_quote(&to));
    let out = run_sftp(&host, &script, 20)?;
    finish(&out)
}
