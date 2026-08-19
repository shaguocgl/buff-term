use crate::db::Db;
use crate::guard::{GuardConfig, GuardEngine, TerminalGuardApproval};
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
    /// 终端危险命令拦截状态机
    guard: GuardEngine,
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
                        if let Some(manager) = app_for_io.try_state::<SessionManager>() {
                            manager.feed_output(id, &data);
                        }
                        let _ = app_for_io.emit(
                            "terminal:data",
                            TerminalData { session_id: id, data: data.to_vec() },
                        );
                    }
                    Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                        if let Some(manager) = app_for_io.try_state::<SessionManager>() {
                            manager.feed_output(id, &data);
                        }
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
                guard: GuardEngine::new(GuardConfig::default()),
            },
        );
        Ok(id)
    }

    pub fn remove(&self, id: u32) {
        self.sessions.lock().unwrap().remove(&id);
    }

    /// 把远端回显喂给危险命令拦截状态机：Suspended（方向键 / Tab 等）时，
    /// readline 重绘的“提示符 + 历史命令”字节在这里累积，供 Enter 时重同步判定。
    pub fn feed_output(&self, id: u32, data: &[u8]) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(&id) {
            session.guard.on_output(data);
        }
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

    /// 写入终端输入：先经过危险命令拦截状态机，再决定是否转发到远端。
    /// `passthrough` 由前端在每次按键时传入（alternate screen 全屏应用期间为 true），
    /// 与输入同路同步，避免独立调用的乱序竞态。
    pub fn write(
        &self,
        app: &AppHandle,
        id: u32,
        data: Vec<u8>,
        config: GuardConfig,
        passthrough: bool,
        console_line: Option<&str>,
    ) -> Result<(), String> {
        // Suspended（Tab / 方向键 / 编辑键后）状态下回车需要与远端 readline
        // 补全 / 重绘回显同步：本地 Enter 通常先于补全后缀回显到达，若立即判定
        // 会漏掉补全部分（例如第二次 Tab 补全的末尾字符）。给 ~200ms 窗口，
        // 覆盖常见网络往返延迟，让补全/重绘回显先累积进同步缓冲，再执行判定。
        let needs_sync_delay = !passthrough
            && data.iter().any(|&b| b == 0x0d || b == 0x0a)
            && self
                .sessions
                .lock()
                .unwrap()
                .get(&id)
                .map(|s| s.guard.is_suspended())
                .unwrap_or(false);
        if needs_sync_delay {
            let app = app.clone();
            let console_line = console_line.map(|s| s.to_string());
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Some(manager) = app.try_state::<SessionManager>() {
                    let _ = manager.process_input(
                        &app,
                        id,
                        data,
                        config,
                        passthrough,
                        console_line.as_deref(),
                    );
                }
            });
            return Ok(());
        }
        self.process_input(app, id, data, config, passthrough, console_line)
    }

    /// 真正的输入处理：经过拦截状态机后决定转发 / 弹窗 / 审计。
    fn process_input(
        &self,
        app: &AppHandle,
        id: u32,
        data: Vec<u8>,
        config: GuardConfig,
        passthrough: bool,
        console_line: Option<&str>,
    ) -> Result<(), String> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&id).ok_or("会话不存在")?;
        session.guard.set_config(config.clone());
        session.guard.set_passthrough(passthrough);
        let outcome = session.guard.process_with_console_line(&data, console_line);
        if !outcome.forward.is_empty() {
            session
                .input_tx
                .send(outcome.forward)
                .map_err(|_| "会话已关闭".to_string())?;
        }
        if let Some(approval) = outcome.approval {
            let host_label = format!("{} ({})", session.host.name, session.host.label_address());
            let _ = app.emit(
                "terminal:guard-approval",
                TerminalGuardApproval {
                    session_id: id,
                    request_id: approval.request_id.clone(),
                    host_label,
                    command: approval.command.clone(),
                    matched_patterns: approval.matched_patterns,
                },
            );
            // 审批超时：超时按拒绝处理并写审计
            let timeout_secs = config.timeout_secs.max(10);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
                if let Some(manager) = app.try_state::<SessionManager>() {
                    let _ = manager.resolve_approval(&app, id, &approval.request_id, false, true);
                }
            });
        }
        Ok(())
    }

    /// 处理终端命令审批结果（批准放行 Enter / 拒绝发 Ctrl-U）。
    pub fn resolve_approval(
        &self,
        app: &AppHandle,
        id: u32,
        request_id: &str,
        allow: bool,
        timed_out: bool,
    ) -> Result<(), String> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(&id).ok_or("会话不存在")?;
        let outcome = session.guard.resolve(request_id, allow, timed_out);
        if !outcome.forward.is_empty() {
            session
                .input_tx
                .send(outcome.forward)
                .map_err(|_| "会话已关闭".to_string())?;
        }
        if let Some(audit) = outcome.audit {
            let host_label = format!("{} ({})", session.host.name, session.host.label_address());
            let host_id = session.host.id.clone();
            drop(sessions);
            crate::guard::write_guard_audit(app, id, &host_id, &host_label, &audit);
        }
        Ok(())
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
    app: AppHandle,
    state: State<'_, SessionManager>,
    db: State<'_, Arc<Db>>,
    id: u32,
    data: Vec<u8>,
    passthrough: Option<bool>,
    console_line: Option<String>,
) -> Result<(), String> {
    // 读取防护配置失败时降级为“不拦截”，保证终端输入永远不被吞掉
    let config = match (db.get_terminal_guard_settings(), db.list_terminal_rules()) {
        (Ok(settings), Ok(rules)) => GuardConfig::from_settings(&settings, &rules),
        _ => {
            eprintln!("[guard] 读取防护配置失败，本次输入不拦截");
            GuardConfig::default()
        }
    };
    state.write(
        &app,
        id,
        data,
        config,
        passthrough.unwrap_or(false),
        console_line.as_deref(),
    )
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
