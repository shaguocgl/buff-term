use crate::models::Host;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

pub struct Session {
    pub host: Host,
    master: Box<dyn MasterPty + Send>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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

impl SessionManager {
    pub fn open(
        &self,
        app: AppHandle,
        host: Host,
        cols: u16,
        rows: u16,
    ) -> Result<u32, String> {
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

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let session = Session {
            host: host.clone(),
            master,
            child: Mutex::new(child),
            writer: writer.clone(),
        };
        self.sessions.lock().unwrap().insert(id, session);

        // 读取线程：把 PTY 输出转发给前端，退出后清理会话
        // 若钥匙串中有该主机密码，自动响应首次密码 / 密钥口令提示
        let app = app.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            let mut scan = String::new();
            let mut sent_secret = false;
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
                        if let Some(secret) = &auto_password {
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
                            let is_prompt = lower.contains("password:")
                                || lower.contains("passphrase for key");
                            if !sent_secret && is_prompt {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(format!("{secret}\r").as_bytes());
                                    let _ = w.flush();
                                }
                                sent_secret = true;
                                scan.clear();
                            }
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

    pub fn write(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        let mut writer = session.writer.lock().unwrap();
        writer
            .write_all(&data)
            .and_then(|_| writer.flush())
            .map_err(|e| format!("写入会话失败: {e}"))
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
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
    state: State<'_, SessionManager>,
    id: u32,
    data: Vec<u8>,
) -> Result<(), String> {
    state.write(id, data)
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
