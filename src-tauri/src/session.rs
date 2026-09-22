use crate::db::Db;
use crate::guard::{GuardConfig, GuardEngine, TerminalGuardApproval};
use crate::models::Host;
use crate::russh::{do_connect, ClientHandler};
use russh::client::Handle;
use russh::{ChannelMsg, ChannelWriteHalf};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

/// 喂给会话守护消费者（单任务顺序处理）的消息：
/// - Input：前端按键（含权威控制台行与防护配置快照）
/// - Resolve：审批结果 / 超时按拒绝
/// 回显（Echo）不走此队列，走独立的 echo 通道（见 Session::echo_tx），
/// 保证 Enter 的 200ms 回显同步窗口内消费者能持续吸收 readline 重绘字节。
/// 所有 guard 状态机变更都在同一消费者任务内串行执行，
/// 消除跨线程乱序与「审批结果 vs 超时」竞态。
enum GuardMsg {
    Input {
        data: Vec<u8>,
        config: GuardConfig,
        passthrough: bool,
        console_line: Option<String>,
    },
    Resolve {
        request_id: String,
        allow: bool,
        timed_out: bool,
    },
}

pub struct Session {
    pub host: Host,
    /// 终端输出通道（二进制帧：首字节 0x00 = 字节流，0x01 = 状态 JSON）
    out: Channel<InvokeResponseBody>,
    _handle: Handle<ClientHandler>,
    write_half: Arc<ChannelWriteHalf<russh::client::Msg>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// 发送按键 / 审批结果给该会话的守护消费者
    guard_tx: mpsc::UnboundedSender<GuardMsg>,
    /// 远端回显高优先级通道（read loop → 守护消费者），
    /// 与 guard_tx 分离，避免 Enter 同步窗口内回显被输入队列阻塞
    echo_tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// 终端数据帧首字节：原始字节流
const FRAME_DATA: u8 = 0x00;
/// 终端数据帧首字节：状态 JSON
const FRAME_STATUS: u8 = 0x01;
/// 合帧窗口：pending 非空时最多再等 4ms，把连续到达的输出合并发送
const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(4);
/// pending 超过 32 KiB 立即 flush，不等窗口
const FLUSH_AT: usize = 32 * 1024;

/// 发送数据帧（0x00 + 原始字节）
fn send_data(out: &Channel<InvokeResponseBody>, data: &[u8]) {
    let mut frame = Vec::with_capacity(data.len() + 1);
    frame.push(FRAME_DATA);
    frame.extend_from_slice(data);
    let _ = out.send(InvokeResponseBody::Raw(frame));
}

/// 发送状态帧（0x01 + UTF-8 JSON：{"status","reason","exit_code"}）
fn send_status(
    out: &Channel<InvokeResponseBody>,
    status: &str,
    reason: Option<&str>,
    exit_code: Option<u32>,
) {
    let payload = serde_json::json!({
        "status": status,
        "reason": reason,
        "exit_code": exit_code,
    })
    .to_string();
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(FRAME_STATUS);
    frame.extend_from_slice(payload.as_bytes());
    let _ = out.send(InvokeResponseBody::Raw(frame));
}

#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
    /// 终端防护配置缓存（版本号 → 配置），规则变更时 bump 版本失效，避免每次按键读 DB
    guard_config_cache: Mutex<Option<(u64, GuardConfig)>>,
}

impl SessionManager {
    pub async fn open(
        &self,
        app: AppHandle,
        host: Host,
        cols: u16,
        rows: u16,
        out: Channel<InvokeResponseBody>,
    ) -> Result<u32, String> {
        let cols = cols.clamp(2, 400);
        let rows = rows.clamp(10, 200);

        let handle = tokio::time::timeout(
            // 需覆盖首次连接的主机指纹确认窗口（60s）
            std::time::Duration::from_secs(60),
            do_connect(&host, None, None),
        )
        .await
        .map_err(|_| "SSH 连接超时（60 秒），请检查网络或服务器状态".to_string())??;
        let channel: russh::Channel<russh::client::Msg> = tokio::time::timeout(
            crate::russh::CHANNEL_OP_TIMEOUT,
            handle.channel_open_session(),
        )
        .await
        .map_err(|_| "打开 SSH 会话通道超时".to_string())?
        .map_err(|e| format!("打开 SSH 会话通道失败: {e}"))?;
        tokio::time::timeout(
            crate::russh::CHANNEL_OP_TIMEOUT,
            channel.request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]),
        )
        .await
        .map_err(|_| "请求 PTY 超时".to_string())?
        .map_err(|e| format!("请求 PTY 失败: {e}"))?;
        tokio::time::timeout(
            crate::russh::CHANNEL_OP_TIMEOUT,
            channel.request_shell(false),
        )
        .await
        .map_err(|_| "请求 shell 超时".to_string())?
        .map_err(|e| format!("请求 shell 失败: {e}"))?;

        let (mut read_half, write_half) = channel.split();
        let write_half = Arc::new(write_half);
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
        let (guard_tx, mut guard_rx) = mpsc::unbounded_channel::<GuardMsg>();
        let (echo_tx, mut echo_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        self.sessions.lock().unwrap().insert(
            id,
            Session {
                host: host.clone(),
                out: out.clone(),
                _handle: handle,
                write_half: write_half.clone(),
                resize_tx,
                guard_tx,
                echo_tx,
            },
        );

        let write_resize = write_half.clone();
        tokio::spawn(async move {
            while let Some((cols, rows)) = resize_rx.recv().await {
                let _ = write_resize
                    .window_change(cols as u32, rows as u32, 0, 0)
                    .await;
            }
        });

        let app_for_io = app.clone();
        let out_io = out.clone();
        tokio::spawn(async move {
            // 合帧读循环：连续到达的输出在 4ms 窗口内合并为一条 Channel 消息，
            // 减少 IPC 次数；超过 32 KiB 立即 flush
            let mut pending: Vec<u8> = Vec::new();
            let mut exit_code: Option<u32> = None;
            let mut disconnected = false;
            loop {
                let msg = if pending.is_empty() {
                    read_half.wait().await
                } else {
                    match tokio::time::timeout(FLUSH_WINDOW, read_half.wait()).await {
                        Ok(m) => m,
                        Err(_) => {
                            send_data(&out_io, &pending);
                            pending.clear();
                            continue;
                        }
                    }
                };
                match msg {
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        if let Some(manager) = app_for_io.try_state::<SessionManager>() {
                            manager.feed_output(id, &data);
                        }
                        pending.extend_from_slice(&data);
                        if pending.len() >= FLUSH_AT {
                            send_data(&out_io, &pending);
                            pending.clear();
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        exit_code = Some(exit_status);
                    }
                    Some(ChannelMsg::Close) => break,
                    None => {
                        // 底层连接断开（读端关闭），与 shell 正常退出区分
                        disconnected = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !pending.is_empty() {
                send_data(&out_io, &pending);
            }
            let reason = if disconnected && exit_code.is_none() {
                "disconnected"
            } else {
                "exit"
            };
            send_status(&out_io, "exited", Some(reason), exit_code);
            let _ = app_for_io.state::<SessionManager>().remove(id);
        });

        // 守护消费者：按到达顺序串行处理按键 / 回显 / 审批结果，持有 GuardEngine。
        // 所有 guard 状态变更都在这里，天然消除并发乱序（此前 Enter 的 200ms 同步
        // 延迟是独立 spawn，后续按键可能先于 Enter 到达远端，导致输入顺序错乱）。
        // 回显走独立通道：Suspended 下 Enter 的 200ms 同步窗口内，消费者持续吸收
        // readline 重绘字节，保证补全/历史回放的判定不依赖 console_line 也能重建。
        let write_for_guard = write_half.clone();
        let app_for_guard = app.clone();
        let host_id = host.id.clone();
        let host_label = format!("{} ({})", host.name, host.label_address());
        let session_id_guard = id;
        tokio::spawn(async move {
            let mut guard = GuardEngine::new(GuardConfig::default());
            loop {
                // 主队列（按键/审批）与回显通道并行等待；回显到达立即喂状态机
                let msg = tokio::select! {
                    msg = guard_rx.recv() => msg,
                    echo = echo_rx.recv() => {
                        match echo {
                            Some(data) => {
                                guard.on_output(&data);
                                continue;
                            }
                            None => break,
                        }
                    }
                };
                let Some(msg) = msg else { break };
                match msg {
                    GuardMsg::Input {
                        data,
                        config,
                        passthrough,
                        console_line,
                    } => {
                        // Suspended（Tab / 方向键 / 编辑键后）状态下回车需要与远端
                        // readline 补全 / 重绘回显同步：留 ~200ms 窗口等回显先累积进
                        // 同步缓冲，再执行判定（窗口内只收回显，后续输入不插队）
                        let needs_sync_delay = !passthrough
                            && data.iter().any(|&b| b == 0x0d || b == 0x0a)
                            && guard.is_suspended();
                        if needs_sync_delay {
                            let sleep =
                                tokio::time::sleep(std::time::Duration::from_millis(200));
                            tokio::pin!(sleep);
                            loop {
                                tokio::select! {
                                    _ = &mut sleep => break,
                                    echo = echo_rx.recv() => {
                                        match echo {
                                            Some(data) => guard.on_output(&data),
                                            None => break,
                                        }
                                    }
                                }
                            }
                        }
                        guard.set_config(config);
                        guard.set_passthrough(passthrough);
                        let outcome =
                            guard.process_with_console_line(&data, console_line.as_deref());
                        if !outcome.forward.is_empty() {
                            let _ = write_for_guard.data_bytes(outcome.forward).await;
                        }
                        if let Some(approval) = outcome.approval {
                            let timeout_secs = guard.timeout_secs().max(10);
                            let _ = app_for_guard.emit(
                                "terminal:guard-approval",
                                TerminalGuardApproval {
                                    session_id: session_id_guard,
                                    request_id: approval.request_id.clone(),
                                    host_label: host_label.clone(),
                                    command: approval.command.clone(),
                                    matched_patterns: approval.matched_patterns,
                                    timeout_secs,
                                },
                            );
                            let app = app_for_guard.clone();
                            let req_id = approval.request_id.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(
                                    std::time::Duration::from_secs(timeout_secs),
                                )
                                .await;
                                if let Some(manager) =
                                    app.try_state::<SessionManager>()
                                {
                                    let _ = manager.resolve_approval(
                                        &app,
                                        session_id_guard,
                                        &req_id,
                                        false,
                                        true,
                                    );
                                }
                            });
                        }
                    }
                    GuardMsg::Resolve {
                        request_id,
                        allow,
                        timed_out,
                    } => {
                        let outcome = guard.resolve(&request_id, allow, timed_out);
                        if !outcome.forward.is_empty() {
                            let _ = write_for_guard.data_bytes(outcome.forward).await;
                        }
                        // 超时按拒绝处理后通知前端立即关闭弹窗，避免前后端状态不一致
                        if timed_out {
                            let _ = app_for_guard.emit(
                                "terminal:guard-resolved",
                                serde_json::json!({
                                    "session_id": session_id_guard,
                                    "request_id": request_id,
                                    "approved": false,
                                    "timed_out": true,
                                }),
                            );
                        }
                        if let Some(audit) = outcome.audit {
                            crate::guard::write_guard_audit(
                                &app_for_guard,
                                session_id_guard,
                                &host_id,
                                &host_label,
                                &audit,
                            );
                        }
                    }
                }
            }
        });

        Ok(id)
    }

    pub fn remove(&self, id: u32) {
        self.sessions.lock().unwrap().remove(&id);
    }

    /// 把远端回显喂给守护消费者：Suspended（方向键 / Tab 等）时，
    /// readline 重绘的“提示符 + 历史命令”字节在这里累积，供 Enter 时重同步判定。
    pub fn feed_output(&self, id: u32, data: &[u8]) {
        let sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&id) {
            let _ = session.echo_tx.send(data.to_vec());
        }
    }

    pub fn host(&self, id: u32) -> Option<Host> {
        self.sessions.lock().unwrap().get(&id).map(|s| s.host.clone())
    }

    pub async fn close(&self, _app: &AppHandle, id: u32) -> Result<(), String> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or("会话不存在")?;
        let _ = session.write_half.close().await;
        send_status(&session.out, "closed", None, None);
        Ok(())
    }

    /// 写入终端输入：入队到守护消费者，由消费者串行执行「拦截判定 → 转发 / 弹窗」。
    /// `passthrough` 由前端在每次按键时传入（alternate screen 全屏应用期间为 true），
    /// 与输入同路同步，避免独立调用的乱序竞态。
    pub fn write(
        &self,
        _app: &AppHandle,
        id: u32,
        data: Vec<u8>,
        config: GuardConfig,
        passthrough: bool,
        console_line: Option<&str>,
    ) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        session
            .guard_tx
            .send(GuardMsg::Input {
                data,
                config,
                passthrough,
                console_line: console_line.map(|s| s.to_string()),
            })
            .map_err(|_| "会话已关闭".to_string())
    }

    /// 处理终端命令审批结果（批准放行 Enter / 拒绝发 Ctrl-U）。
    /// 交由消费者串行处理，与「超时任务」天然有序，消除竞态。
    pub fn resolve_approval(
        &self,
        _app: &AppHandle,
        id: u32,
        request_id: &str,
        allow: bool,
        timed_out: bool,
    ) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(&id).ok_or("会话不存在")?;
        session
            .guard_tx
            .send(GuardMsg::Resolve {
                request_id: request_id.to_string(),
                allow,
                timed_out,
            })
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
    db: State<'_, Arc<Db>>,
    host_id: String,
    cols: u16,
    rows: u16,
    on_data: Channel<InvokeResponseBody>,
) -> Result<u32, String> {
    let host = crate::hosts::load_host(&db, &host_id)?;
    state.open(app, host, cols, rows, on_data).await
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
    // 读取防护配置（带版本缓存，规则不变时不重复查库）；
    // 读取失败时降级为“不拦截”，保证终端输入永远不被吞掉
    let config = load_guard_config(&db, &state);
    state.write(
        &app,
        id,
        data,
        config,
        passthrough.unwrap_or(false),
        console_line.as_deref(),
    )
}

/// 读取防护配置：版本号未变时复用缓存，规则/设置变更（bump 版本）后重新加载。
fn load_guard_config(db: &Db, manager: &SessionManager) -> GuardConfig {
    let version = crate::guard::rules_version();
    if let Some((v, cfg)) = manager.guard_config_cache.lock().unwrap().as_ref() {
        if *v == version {
            return cfg.clone();
        }
    }
    let config = match (db.get_terminal_guard_settings(), db.list_terminal_rules()) {
        (Ok(settings), Ok(rules)) => GuardConfig::from_settings(&settings, &rules),
        _ => {
            eprintln!("[guard] 读取防护配置失败，本次输入不拦截");
            GuardConfig::default()
        }
    };
    *manager.guard_config_cache.lock().unwrap() = Some((version, config.clone()));
    config
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
