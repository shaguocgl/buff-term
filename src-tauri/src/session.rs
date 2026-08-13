use crate::models::Host;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

pub struct Session {
    pub host: Host,
    master: Box<dyn MasterPty + Send>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    input_tx: Mutex<mpsc::Sender<Vec<u8>>>,
}

#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
}

#[derive(Clone, Serialize)]
pub struct TerminalData {
    pub session_id: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Serialize)]
pub struct SessionStatus {
    pub session_id: u32,
    pub status: String,
}

#[derive(Clone, Serialize)]
pub struct SessionNotice {
    pub session_id: u32,
    pub message: String,
}

impl SessionManager {
    pub fn open(
        &self,
        app: AppHandle,
        host: Host,
        cols: u16,
        rows: u16,
    ) -> Result<u32, String> {
        // 防御：异常/过窄尺寸会导致动态输出（docker compose 等）每帧换行堆叠
        let cols = cols.clamp(20, 400);
        let rows = rows.clamp(5, 200);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("创建 PTY 失败: {e}"))?;
        let master = pair.master;

        let mut cmd = CommandBuilder::new("ssh");
        for arg in host.ssh_args() {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动 ssh 失败: {e}"))?;
        let reader = master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读取端失败: {e}"))?;
        let writer = master
            .take_writer()
            .map_err(|e| format!("获取 PTY 写入端失败: {e}"))?;
        let writer = Arc::new(Mutex::new(writer));
        let auto_password = crate::credentials::get_password(&host.id);
        let expecting_password = Arc::new(AtomicBool::new(false));
        let password_capture = Arc::new(Mutex::new(Vec::<u8>::new()));
        let captured_once = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let writer_for_input = writer.clone();

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let session = Session {
            host: host.clone(),
            master,
            child: Mutex::new(child),
            input_tx: Mutex::new(input_tx),
        };
        self.sessions.lock().unwrap().insert(id, session);

        // 输入写入线程：串行消费输入队列，保证顺序、不丢字
        let app_for_input = app.clone();
        let host_id = host.id.clone();
        let expecting_for_input = expecting_password.clone();
        let capture_for_input = password_capture.clone();
        let captured_for_input = captured_once.clone();
        std::thread::spawn(move || {
            let writer = writer_for_input;
            for data in input_rx {
                if expecting_for_input.load(Ordering::SeqCst) {
                    let mut cap = capture_for_input.lock().unwrap();
                    for &b in &data {
                        match b {
                            // 回车/换行：密码输入结束
                            b'\r' | b'\n' => {
                                let password = String::from_utf8_lossy(&cap).to_string();
                                cap.clear();
                                expecting_for_input.store(false, Ordering::SeqCst);
                                if !password.is_empty() {
                                    let saved =
                                        crate::credentials::save_password(&host_id, &password)
                                            .is_ok();
                                    captured_for_input.store(true, Ordering::SeqCst);
                                    if saved {
                                        let _ = app_for_input.emit(
                                            "session:notice",
                                            SessionNotice {
                                                session_id: id,
                                                message: "密码已保存到系统钥匙串，AI 可直接使用"
                                                    .to_string(),
                                            },
                                        );
                                    }
                                }
                                break;
                            }
                            // 退格：删除前一个字符（与终端行规程一致）
                            0x7f | 0x08 => {
                                cap.pop();
                            }
                            _ => cap.push(b),
                        }
                    }
                }
                if let Ok(mut w) = writer.lock() {
                    if w.write_all(&data).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            }
        });

        // 读取线程：把 PTY 输出转发给前端，退出后清理会话
        // 若钥匙串中有该主机密码，自动响应首次密码 / 密钥口令提示
        let app = app.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            let mut scan = String::new();
            let mut sent_secret = false;
            // 捕获模式：无自动密码时默认开启；自动填充失败（Permission denied）时也会开启，
            // 以便把用户手动输入的正确密码重新保存
            let mut capture_mode = auto_password.is_none();
            let expecting = expecting_password.clone();
            let captured = captured_once.clone();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        let _ = app.emit(
                            "terminal:data",
                            TerminalData {
                                session_id: id,
                                data: data.clone(),
                            },
                        );
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
                        let is_auth_prompt =
                            lower.contains("password:") || lower.contains("passphrase for key");
                        let is_password_prompt = lower.contains("password:");
                        if lower.contains("permission denied") {
                            // 自动填充或手动输入失败：转为捕获模式，允许重新记录正确密码
                            capture_mode = true;
                            captured.store(false, Ordering::SeqCst);
                            scan.clear();
                        }
                        if let Some(secret) = &auto_password {
                            if !sent_secret && is_auth_prompt {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(format!("{secret}\r").as_bytes());
                                    let _ = w.flush();
                                }
                                sent_secret = true;
                                scan.clear();
                            }
                        }
                        // 捕获用户手动输入的密码（含自动填充失败后重新输入的情况）
                        if capture_mode
                            && !captured.load(Ordering::SeqCst)
                            && !expecting.load(Ordering::SeqCst)
                            && is_password_prompt
                        {
                            expecting.store(true, Ordering::SeqCst);
                            scan.clear();
                        }
                    }
                }
            }
            let _ = app.emit(
                "session:status",
                SessionStatus {
                    session_id: id,
                    status: "exited".to_string(),
                },
            );
            app.state::<SessionManager>().remove(id);
        });

        Ok(id)
    }

    pub fn remove(&self, id: u32) {
        self.sessions.lock().unwrap().remove(&id);
    }

    pub fn host(&self, id: u32) -> Option<Host> {
        self.sessions.lock().unwrap().get(&id).map(|s| s.host.clone())
    }

    pub fn close(&self, app: &AppHandle, id: u32) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or("会话不存在")?;
        let _ = session.child.lock().unwrap().kill();
        let _ = app.emit(
            "session:status",
            SessionStatus {
                session_id: id,
                status: "closed".to_string(),
            },
        );
        Ok(())
    }

    pub fn write(&self, _app: &AppHandle, id: u32, data: Vec<u8>) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        let tx = session.input_tx.lock().unwrap();
        tx.send(data).map_err(|_| "会话已关闭".to_string())
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let cols = cols.clamp(20, 400);
        let rows = rows.clamp(5, 200);
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("调整 PTY 尺寸失败: {e}"))
    }
}

#[tauri::command]
pub fn open_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    host: Host,
    cols: u16,
    rows: u16,
) -> Result<u32, String> {
    state.open(app, host, cols, rows)
}

#[tauri::command]
pub fn close_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    id: u32,
) -> Result<(), String> {
    state.close(&app, id)
}

#[tauri::command]
pub fn session_input(
    app: AppHandle,
    state: State<'_, SessionManager>,
    id: u32,
    data: Vec<u8>,
) -> Result<(), String> {
    state.write(&app, id, data)
}

#[tauri::command]
pub fn session_resize(
    state: State<'_, SessionManager>,
    id: u32,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    state.resize(id, cols, rows)
}
