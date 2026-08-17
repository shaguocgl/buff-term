use crate::models::Host;
use crate::russh::{do_connect, ClientHandler};
use russh::client::Handle;
use russh::{Channel, ChannelMsg, ChannelWriteHalf};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

pub struct Session {
    pub host: Host,
    _handle: Handle<ClientHandler>,
    write_half: Arc<ChannelWriteHalf<russh::client::Msg>>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
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
    pub async fn open(
        &self,
        app: AppHandle,
        host: Host,
        cols: u16,
        rows: u16,
    ) -> Result<u32, String> {
        let cols = cols.clamp(2, 400);
        let rows = rows.clamp(10, 200);

        let handle = do_connect(&host, None).await?;
        let channel: Channel<russh::client::Msg> = handle
            .channel_open_session()
            .await
            .map_err(|e| format!("打开 SSH 会话通道失败: {e}"))?;
        channel
            .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .map_err(|e| format!("请求 PTY 失败: {e}"))?;
        channel
            .request_shell(false)
            .await
            .map_err(|e| format!("请求 shell 失败: {e}"))?;

        let (mut read_half, write_half) = channel.split();
        let write_half = Arc::new(write_half);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let write_input = write_half.clone();
        let write_resize = write_half.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    input = input_rx.recv() => {
                        match input {
                            Some(data) => {
                                let _ = write_input.data_bytes(data).await;
                            }
                            None => break,
                        }
                    }
                    resize = resize_rx.recv() => {
                        match resize {
                            Some((cols, rows)) => {
                                let _ = write_resize
                                    .window_change(cols as u32, rows as u32, 0, 0)
                                    .await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        let app_for_io = app.clone();
        tokio::spawn(async move {
            loop {
                match read_half.wait().await {
                    Some(ChannelMsg::Data { data }) => {
                        let _ = app_for_io.emit(
                            "terminal:data",
                            TerminalData { session_id: id, data: data.to_vec() },
                        );
                    }
                    Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                        let _ = app_for_io.emit(
                            "terminal:data",
                            TerminalData { session_id: id, data: data.to_vec() },
                        );
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
            let _ = app_for_io.emit(
                "session:status",
                SessionStatus {
                    session_id: id,
                    status: "exited".to_string(),
                },
            );
            let _ = app_for_io.state::<SessionManager>().remove(id);
        });

        self.sessions.lock().unwrap().insert(
            id,
            Session {
                host,
                _handle: handle,
                write_half,
                input_tx,
                resize_tx,
            },
        );
        Ok(id)
    }

    pub fn remove(&self, id: u32) {
        self.sessions.lock().unwrap().remove(&id);
    }

    pub fn host(&self, id: u32) -> Option<Host> {
        self.sessions.lock().unwrap().get(&id).map(|s| s.host.clone())
    }

    pub async fn close(&self, app: &AppHandle, id: u32) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or("会话不存在")?;
        let _ = session.write_half.close().await;
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
        session
            .input_tx
            .send(data)
            .map_err(|_| "会话已关闭".to_string())
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let cols = cols.clamp(2, 400);
        let rows = rows.clamp(10, 200);
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        session
            .resize_tx
            .send((cols, rows))
            .map_err(|_| "会话已关闭".to_string())
    }
}

#[tauri::command]
pub async fn open_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    host: Host,
    cols: u16,
    rows: u16,
) -> Result<u32, String> {
    state.open(app, host, cols, rows).await
}

#[tauri::command]
pub async fn close_session(
    app: AppHandle,
    state: State<'_, SessionManager>,
    id: u32,
) -> Result<(), String> {
    state.close(&app, id).await
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
