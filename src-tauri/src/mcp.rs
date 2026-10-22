//! 对外 MCP 服务：buffTerm 作为 MCP 服务器（Streamable HTTP），
//! 把用户勾选的 SSH 服务器能力开放给外部 AI（Codex、Claude Desktop 等）。
//!
//! 设计要点：
//! - 默认关闭，用户在 UI 中启用时才监听 127.0.0.1；
//! - 随机 token 认证，可随时吊销；
//! - list_hosts 只返回启用时勾选的服务器；
//! - 权限模式：readonly（只读）/ confirm（危险命令需确认）/ allow（全部放行）；
//! - 所有命令输出复用脱敏规则，外部调用同样写入审计日志。

use crate::db::Db;
use crate::models::{Host, McpPermissionMode, McpRule, McpService};
use crate::russh::{ExecResult, RusshManager};
use crate::safety::{
    command_has_sensitive_path, is_safe_readonly_command, is_sensitive_path, sanitize,
};
use crate::util::{format_exec_output, generate_token, now, shq, truncate};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const DEFAULT_PORT: u16 = 48123;
const PROTOCOL_VERSION: &str = "2025-03-26";

// ---------- 配置 ----------

#[derive(Debug, Deserialize)]
pub struct McpServiceInput {
    pub enabled: bool,
    #[serde(default)]
    pub host_ids: Vec<String>,
    #[serde(default = "default_permission")]
    pub permission_mode: McpPermissionMode,
}

fn default_permission() -> McpPermissionMode {
    McpPermissionMode::Confirm
}

#[derive(Serialize, Clone)]
pub struct McpServiceView {
    #[serde(flatten)]
    pub config: McpService,
    pub running: bool,
    /// 是否已生成 token。token 本身默认不回传，前端点“显示”时再取。
    pub token_set: bool,
}

// ---------- 运行管理 ----------

struct RunningMcp {
    shutdown: tokio::sync::oneshot::Sender<()>,
    /// 服务线程存活标志：停止时置 false，SSE 长连接据此退出；线程异常退出也会置 false。
    alive: Arc<AtomicBool>,
}

/// 服务线程退出时自动把 alive 置 false，避免 manager 误以为服务仍在运行。
struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 同时处理的 MCP HTTP/SSE 连接上限，避免本地客户端异常时线程无限增长。
const MAX_MCP_CONNECTIONS: usize = 32;

#[derive(Default)]
pub struct McpServiceManager {
    inner: Mutex<Option<RunningMcp>>,
    /// 生命周期互斥：is_running 检查与 start/stop 之间加锁，
    /// 防止并发调用同时看到「未运行」而启动多个服务线程
    lifecycle: Mutex<()>,
}

impl McpServiceManager {
    pub fn is_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|running| running.alive.load(Ordering::SeqCst))
    }

    pub fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle.lock().unwrap()
    }

    fn set(&self, running: Option<RunningMcp>) {
        *self.inner.lock().unwrap() = running;
    }
}

// ---------- 审批注册表（外部调用危险命令时弹窗确认） ----------

#[derive(Default)]
pub struct ApprovalRegistry {
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
}

impl ApprovalRegistry {
    fn register(&self, id: String) -> tokio::sync::oneshot::Receiver<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        rx
    }

    fn resolve(&self, id: &str, allow: bool) -> Result<(), String> {
        let tx = self.pending.lock().unwrap().remove(id);
        match tx {
            Some(tx) => tx.send(allow).map_err(|_| crate::i18n::t("审批接收方已关闭", "The approval receiver is closed").to_string()),
            None => Err(crate::i18n::t("审批请求不存在或已超时", "The approval request does not exist or has timed out").to_string()),
        }
    }

    fn remove(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

// ---------- Tauri 命令 ----------

#[tauri::command]
pub fn get_mcp_service(
    db: State<'_, Arc<Db>>,
    manager: State<'_, McpServiceManager>,
) -> Result<McpServiceView, String> {
    let mut config = db
        .get_mcp_service()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取 MCP 服务配置失败", "Failed to read the MCP service config")))?;
    // 默认不回传明文 token，只告知是否存在；需要复制配置时前端显式调用
    // reveal_mcp_token。这样普通打开设置页不会把 token 长时间留在渲染进程。
    let token_set = crate::credentials::get_mcp_token().is_some();
    config.token = None;
    Ok(McpServiceView {
        running: manager.is_running(),
        config,
        token_set,
    })
}

/// 显式读取 MCP token（用于“显示 token / 复制配置”按钮）。
#[tauri::command]
pub fn reveal_mcp_token() -> Result<String, String> {
    crate::credentials::get_mcp_token().ok_or_else(|| {
        crate::i18n::t("MCP token 不存在，请重新启用或轮换 token", "The MCP token does not exist; re-enable the service or rotate the token").to_string()
    })
}

#[tauri::command]
pub async fn save_mcp_service(
    app: AppHandle,
    db: State<'_, Arc<Db>>,
    manager: State<'_, McpServiceManager>,
    input: McpServiceInput,
) -> Result<McpServiceView, String> {
    if input.enabled && input.host_ids.is_empty() {
        return Err(crate::i18n::t("启用 MCP 服务前，请至少勾选一台服务器", "Select at least one server before enabling the MCP service").to_string());
    }

    let mut config = db
        .get_mcp_service()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取 MCP 服务配置失败", "Failed to read the MCP service config")))?;
    config.enabled = input.enabled;
    config.host_ids = input.host_ids;
    config.permission_mode = input.permission_mode;
    config.updated_at = now();

    if !config.enabled {
        let _life = manager.lock_lifecycle();
        stop_service(&manager);
        drop(_life);
        config.port = None;
        db.save_mcp_service(&config)
            .map_err(|e| format!("{}: {e}", crate::i18n::t("保存 MCP 服务配置失败", "Failed to save the MCP service config")))?;
        return Ok(McpServiceView {
            running: false,
            config,
            token_set: crate::credentials::get_mcp_token().is_some(),
        });
    }

    // 首次启用时生成 token；token 加密存凭据表（DB 列恒为 None），
    // 因此仅当凭据表中也没有 token 时才生成，避免每次改配置都轮换 token
    if config.token.is_none() && crate::credentials::get_mcp_token().is_none() {
        config.token = Some(generate_token());
    }
    // token 加密存凭据表，不落明文到 mcp_service 表
    if let Some(token) = config.token.clone() {
        crate::credentials::save_mcp_token(&token)
            .map_err(|e| format!("{}: {e}", crate::i18n::t("保存 MCP token 失败", "Failed to save the MCP token")))?;
    }
    config.token = None;
    // 未运行时才启动；已运行时只需更新配置（请求时实时读取）。
    // 检查与启动之间持生命周期锁，避免并发调用启动多个服务线程
    let _life = manager.lock_lifecycle();
    if !manager.is_running() {
        let port = start_service(&app, &manager)?;
        config.port = Some(port);
    }
    db.save_mcp_service(&config)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存 MCP 服务配置失败", "Failed to save the MCP service config")))?;
    drop(_life);
    // 回填明文 token 供前端展示
    config.token = crate::credentials::get_mcp_token();
    Ok(McpServiceView {
        running: true,
        config,
        token_set: true,
    })
}

#[tauri::command]
pub fn rotate_mcp_token(
    db: State<'_, Arc<Db>>,
    manager: State<'_, McpServiceManager>,
) -> Result<McpServiceView, String> {
    let mut config = db
        .get_mcp_service()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取 MCP 服务配置失败", "Failed to read the MCP service config")))?;
    let token = generate_token();
    crate::credentials::save_mcp_token(&token)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存 MCP token 失败", "Failed to save the MCP token")))?;
    config.token = None;
    config.updated_at = now();
    db.save_mcp_service(&config)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存 MCP 服务配置失败", "Failed to save the MCP service config")))?;
    config.token = Some(token);
    Ok(McpServiceView {
        running: manager.is_running(),
        config,
        token_set: true,
    })
}

#[tauri::command]
pub fn mcp_approve(
    registry: State<'_, ApprovalRegistry>,
    request_id: String,
    allow: bool,
) -> Result<(), String> {
    registry.resolve(&request_id, allow)
}

#[tauri::command]
pub fn list_mcp_rules(db: State<'_, Arc<Db>>) -> Result<Vec<McpRule>, String> {
    db.list_mcp_rules()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取 MCP 管控规则失败", "Failed to read MCP guard rules")))
}

#[tauri::command]
pub fn add_mcp_rule(db: State<'_, Arc<Db>>, pattern: String) -> Result<McpRule, String> {
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return Err(crate::i18n::t("管控命令不能为空", "The guarded command must not be empty").to_string());
    }
    let rule = McpRule {
        id: uuid::Uuid::new_v4().to_string(),
        pattern,
        enabled: true,
        created_at: now(),
    };
    db.insert_mcp_rule(&rule)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("保存管控规则失败", "Failed to save the guard rule")))?;
    Ok(rule)
}

#[tauri::command]
pub fn delete_mcp_rule(db: State<'_, Arc<Db>>, id: String) -> Result<(), String> {
    db.delete_mcp_rule(&id)
        .map_err(|e| format!("{}: {e}", crate::i18n::t("删除管控规则失败", "Failed to delete the guard rule")))
}

// ---------- 服务生命周期 ----------

pub(crate) fn start_service(app: &AppHandle, manager: &McpServiceManager) -> Result<u16, String> {
    let (port_tx, port_rx) = mpsc::channel::<Result<u16, String>>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let app = app.clone();
    let alive_thread = alive.clone();
    std::thread::spawn(move || mcp_server_main(app, port_tx, shutdown_rx, alive_thread));
    let port = match port_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(port)) => port,
        Ok(Err(e)) => {
            alive.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Err(_) => {
            alive.store(false, Ordering::SeqCst);
            return Err(crate::i18n::t("MCP 服务启动超时", "Timed out starting the MCP service").to_string());
        }
    };
    manager.set(Some(RunningMcp {
        shutdown: shutdown_tx,
        alive,
    }));
    Ok(port)
}

pub(crate) fn stop_service(manager: &McpServiceManager) {
    if let Some(running) = manager.inner.lock().unwrap().take() {
        // 先关闭存活标志，让已经建立的 SSE 长连接在下一个检查点退出；
        // oneshot 只负责唤醒 accept 循环。
        running.alive.store(false, Ordering::SeqCst);
        let _ = running.shutdown.send(());
    }
}

fn mcp_server_main(
    app: AppHandle,
    port_tx: mpsc::Sender<Result<u16, String>>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    alive: Arc<AtomicBool>,
) {
    let _alive_guard = AliveGuard(alive.clone());
    let server = match Server::http(&format!("127.0.0.1:{DEFAULT_PORT}")) {
        Ok(s) => s,
        Err(_) => match Server::http("127.0.0.1:0") {
            Ok(s) => s,
            Err(e) => {
                let _ = port_tx.send(Err(format!("{}: {e}", crate::i18n::t("启动 MCP 服务失败", "Failed to start the MCP service"))));
                return;
            }
        },
    };
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(DEFAULT_PORT);

    // runtime 创建失败时不要先把端口告诉 manager，否则 UI 会显示“运行中”
    // 但实际没有可用的监听线程。
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = port_tx.send(Err(format!(
                "{}: {e}",
                crate::i18n::t("创建 runtime 失败", "failed to create the runtime")
            )));
            return;
        }
    };
    let rt = Arc::new(rt);
    if port_tx.send(Ok(port)).is_err() {
        return;
    }

    let active = Arc::new(AtomicUsize::new(0));
    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }
        match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(request)) => {
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_MCP_CONNECTIONS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = request.respond(
                        Response::from_string("too many connections")
                            .with_status_code(StatusCode(503)),
                    );
                    continue;
                }
                let app = app.clone();
                let rt = rt.clone();
                let alive_req = alive.clone();
                let active_req = active.clone();
                std::thread::spawn(move || {
                    handle_http(app, rt, request, alive_req);
                    active_req.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("[mcp] {}: {e}", crate::i18n::t("HTTP 服务器错误", "HTTP server error"));
                break;
            }
        }
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
    }
}

// ---------- HTTP 层 ----------

/// 仅对本地回环/应用自身的 Origin 回显 CORS 头；MCP 客户端（Codex、Claude Desktop）
/// 是本地进程不走浏览器，不需要 CORS。不返回 CORS 头时浏览器会阻止跨域读取，
/// 从而避免任意网页在 token 泄露后直接调用本地服务。
fn allowed_origin(origin: &str) -> bool {
    let o = origin.trim();
    o == "http://localhost"
        || o == "http://127.0.0.1"
        || o.starts_with("http://localhost:")
        || o.starts_with("http://127.0.0.1:")
        || o == "tauri://localhost"
        || o == "http://tauri.localhost"
        || o == "https://tauri.localhost"
        || o == "https://localhost"
        || o == "https://127.0.0.1"
}

fn cors_headers(request: &Request) -> Vec<Header> {
    let mut headers = Vec::new();
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    if !origin.is_empty() && allowed_origin(origin) {
        headers.push(
            Header::from_bytes(&b"Access-Control-Allow-Origin"[..], origin.as_bytes()).unwrap(),
        );
        headers.push(Header::from_bytes(&b"Vary"[..], &b"Origin"[..]).unwrap());
        headers.push(
            Header::from_bytes(
                &b"Access-Control-Allow-Headers"[..],
                &b"Authorization, Content-Type, MCP-Protocol-Version, MCP-Session-Id"[..],
            )
            .unwrap(),
        );
        headers.push(
            Header::from_bytes(
                &b"Access-Control-Allow-Methods"[..],
                &b"GET, POST, OPTIONS"[..],
            )
            .unwrap(),
        );
    }
    headers
}

fn attach_headers<R: Read>(response: Response<R>, headers: Vec<Header>) -> Response<R> {
    headers
        .into_iter()
        .fold(response, |resp, header| resp.with_header(header))
}

/// 常量时间 token 比较：固定长度随机 token，逐字节异或累积，
/// 避免通过响应时间差逐位猜测（本地服务 + 高并发请求场景）。
fn token_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        // 长度不同也执行一轮等长比较，避免长度信息通过时序泄露
        let mut diff = (a.len() ^ b.len()) as u8;
        let max = a.len().max(b.len());
        for i in 0..max {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            diff |= x ^ y;
        }
        return diff == 0;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn check_auth(app: &AppHandle, request: &Request) -> bool {
    let db = match app.try_state::<Arc<Db>>() {
        Some(db) => db,
        None => return false,
    };
    // token 加密存凭据表；DB 列仅在旧版本遗留明文（迁移后为空）
    let token = match db.get_mcp_service().ok().and_then(|c| c.token) {
        Some(t) if !t.is_empty() => t,
        _ => crate::credentials::get_mcp_token().unwrap_or_default(),
    };
    if token.is_empty() {
        return false;
    }
    if let Some(h) = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
    {
        let v = h.value.as_str();
        if token_eq(v, &token) {
            return true;
        }
        if let Some(bearer) = v.strip_prefix("Bearer ") {
            if token_eq(bearer, &token) {
                return true;
            }
        }
        if let Some(bearer) = v.strip_prefix("bearer ") {
            if token_eq(bearer, &token) {
                return true;
            }
        }
    }
    if let Some(query) = request.url().split('?').nth(1) {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("access_token=") {
                let decoded = percent_encoding::percent_decode_str(v).decode_utf8_lossy();
                if token_eq(&decoded, &token) {
                    return true;
                }
            }
        }
    }
    false
}

fn handle_http(
    app: AppHandle,
    rt: Arc<tokio::runtime::Runtime>,
    mut request: Request,
    alive: Arc<AtomicBool>,
) {
    if !alive.load(Ordering::SeqCst) {
        let _ = request.respond(Response::empty(StatusCode(503)));
        return;
    }
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");

    // 不只在响应里省略 CORS 头，还直接拒绝非本机 Origin：浏览器用
    // no-cors/simple request 仍可能把请求送达本地服务，必须显式拒绝。
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string());
    if let Some(origin) = origin {
        if !allowed_origin(&origin) {
            let _ = request.respond(
                Response::from_string("forbidden origin").with_status_code(StatusCode(403)),
            );
            return;
        }
    }

    if method == Method::Options {
        let headers = cors_headers(&request);
        let resp = attach_headers(Response::empty(StatusCode(204)), headers);
        let _ = request.respond(resp);
        return;
    }
    if path != "/mcp" && path != "/" {
        let headers = cors_headers(&request);
        let resp = attach_headers(
            Response::from_string("not found").with_status_code(StatusCode(404)),
            headers,
        );
        let _ = request.respond(resp);
        return;
    }
    if !check_auth(&app, &request) {
        let headers = cors_headers(&request);
        let resp = attach_headers(
            Response::from_string(crate::i18n::t("未授权：请检查 MCP 配置中的 token", "Unauthorized: check the token in your MCP configuration"))
                .with_status_code(StatusCode(401)),
            headers,
        );
        let _ = request.respond(resp);
        return;
    }

    match method {
        Method::Get => handle_sse(request, alive),
        Method::Post => {
            // 限制请求体大小，避免本地恶意/异常客户端无界占用内存
            const MAX_BODY: u64 = 1024 * 1024;
            let mut body = String::new();
            let mut limited = request.as_reader().take(MAX_BODY + 1);
            if limited.read_to_string(&mut body).is_err() || body.len() as u64 > MAX_BODY {
                let headers = cors_headers(&request);
                let resp = attach_headers(
                    Response::from_string(crate::i18n::t("请求体过大或读取失败（上限 1MB）", "Request body too large or failed to read (limit 1MB)"))
                        .with_status_code(StatusCode(413)),
                    headers,
                );
                let _ = request.respond(resp);
                return;
            }
            let response = rt.block_on(handle_jsonrpc(&app, &body));
            let mut headers = cors_headers(&request);
            headers.push(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
            let _ = request.respond(attach_headers(Response::from_string(response), headers));
        }
        _ => {
            let headers = cors_headers(&request);
            let resp = attach_headers(Response::empty(StatusCode(405)), headers);
            let _ = request.respond(resp);
        }
    }
}

fn handle_sse(request: Request, alive: Arc<AtomicBool>) {
    // tiny_http 的 respond 会把响应写入 BufWriter，无限流 body 阻塞读取时
    // header 永远无法 flush；这里直接用 into_writer 手动写头并立即 flush。
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str())
        .unwrap_or("")
        .to_string();
    let mut writer = request.into_writer();
    let cors = if !origin.is_empty() && allowed_origin(&origin) {
        format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n")
    } else {
        String::new()
    };
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         {cors}\r\n"
    );
    if writer.write_all(headers.as_bytes()).is_err() {
        return;
    }
    if writer.flush().is_err() {
        return;
    }
    let _ = writer.write_all(b": connected\n\n");
    let _ = writer.flush();
    let mut last_ping = Instant::now();
    while alive.load(Ordering::SeqCst) {
        // 250ms 检查一次存活标志，服务停止时能及时退出并释放线程/连接；
        // 真正的 ping 仍保持 15 秒节奏，避免无意义心跳。
        std::thread::sleep(Duration::from_millis(250));
        if last_ping.elapsed() < Duration::from_secs(15) {
            continue;
        }
        if writer.write_all(b": ping\n\n").is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
        last_ping = Instant::now();
    }
}

// ---------- JSON-RPC / MCP 协议 ----------

async fn handle_jsonrpc(app: &AppHandle, body: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return json_error(None, -32700, &format!("{}: {e}", crate::i18n::t("解析请求失败", "Failed to parse the request"))),
    };
    let method = value["method"].as_str().unwrap_or("").to_string();
    let id = value.get("id").cloned();

    match method.as_str() {
        "initialize" => respond(
            id,
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "buffterm-ssh", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "notifications/initialized" => String::new(),
        "ping" => respond(id, serde_json::json!({})),
        "tools/list" => {
            respond(id, serde_json::json!({ "tools": tools_schema() }))
        }
        "tools/call" => {
            let name = value["params"]["name"].as_str().unwrap_or("").to_string();
            let args = value["params"]["arguments"].clone();
            let args = if args.is_object() {
                args
            } else {
                serde_json::json!({})
            };
            match call_tool(app, &name, &args).await {
                Ok(text) => respond(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    }),
                ),
                Err(e) => respond(
                    id,
                    serde_json::json!({
                        "content": [{ "type": "text", "text": format!("{}: {e}", crate::i18n::t("错误", "Error")) }],
                        "isError": true
                    }),
                ),
            }
        }
        _ => json_error(id, -32601, &format!("{}: {method}", crate::i18n::t("未知方法", "Unknown method"))),
    }
}

fn respond(id: Option<serde_json::Value>, result: serde_json::Value) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".to_string(), serde_json::json!("2.0"));
    if let Some(id) = id {
        obj.insert("id".to_string(), id);
    }
    obj.insert("result".to_string(), result);
    serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default()
}

fn json_error(id: Option<serde_json::Value>, code: i64, message: &str) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".to_string(), serde_json::json!("2.0"));
    if let Some(id) = id {
        obj.insert("id".to_string(), id);
    }
    obj.insert(
        "error".to_string(),
        serde_json::json!({ "code": code, "message": message }),
    );
    serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default()
}

fn tools_schema() -> Vec<serde_json::Value> {
    let tools = vec![
        serde_json::json!({
            "name": "list_hosts",
            "description": crate::i18n::t("列出当前 MCP 服务授权可操作的服务器（仅包含用户勾选的主机）", "List the servers this MCP service is authorized to operate on (only the hosts selected by the user)"),
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "resource_usage",
            "description": crate::i18n::t("查看指定服务器（host 参数）的磁盘、内存、负载和占用最高的进程。host 可以是 list_hosts 返回的 id、name 或 address", "Show disk, memory, load and the busiest processes of the given server (host parameter). host may be an id, name or address returned by list_hosts"),
            "inputSchema": {
                "type": "object",
                "properties": { "host": { "type": "string", "description": crate::i18n::t("主机 id / name / address", "host id / name / address") } },
                "required": ["host"]
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": crate::i18n::t("读取指定服务器上的文件内容。敏感路径（SSH/凭据/cron 等）在只读模式下会被拒绝，管控模式下需要用户确认", "Read a file on the given server. Sensitive paths (SSH/credentials/cron, etc.) are rejected in read-only mode and require user approval in guarded mode"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": crate::i18n::t("主机 id / name / address", "host id / name / address") },
                    "path": { "type": "string", "description": crate::i18n::t("服务器上的文件路径", "path of the file on the server") }
                },
                "required": ["host", "path"]
            }
        }),
        serde_json::json!({
            "name": "list_dir",
            "description": crate::i18n::t("列出指定服务器上的目录内容", "List the contents of a directory on the given server"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": crate::i18n::t("主机 id / name / address", "host id / name / address") },
                    "path": { "type": "string", "description": crate::i18n::t("目录路径，默认当前目录", "directory path, defaults to the current directory") }
                },
                "required": ["host"]
            }
        }),
        serde_json::json!({
            "name": "exec_command",
            "description": crate::i18n::t("在指定服务器上执行一条 shell 命令并返回输出。只读模式只允许可证明安全的简单只读命令；管控模式下非安全只读命令、敏感路径或命中自定义规则都需要用户确认。host 建议使用 list_hosts 返回的 id", "Run a shell command on the given server and return its output. Read-only mode only allows proven safe simple read-only commands; in guarded mode non-safe commands, sensitive paths, or custom-rule matches require user approval. Prefer the id returned by list_hosts for host"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": crate::i18n::t("主机 id / name / address", "host id / name / address") },
                    "command": { "type": "string", "description": crate::i18n::t("要执行的 shell 命令", "shell command to run") },
                    "timeout_secs": { "type": "number", "description": crate::i18n::t("超时秒数，默认 30", "timeout in seconds, default 30") }
                },
                "required": ["host", "command"]
            }
        }),
    ];
    tools
}

// ---------- 工具执行 ----------

async fn call_tool(app: &AppHandle, name: &str, args: &serde_json::Value) -> Result<String, String> {
    let db = app
        .try_state::<Arc<Db>>()
        .ok_or_else(|| crate::i18n::t("数据库不可用", "The database is unavailable").to_string())?;
    let config = db
        .get_mcp_service()
        .map_err(|e| format!("{}: {e}", crate::i18n::t("读取 MCP 服务配置失败", "Failed to read the MCP service config")))?;
    if !config.enabled {
        return Err(crate::i18n::t("MCP 服务未启用", "The MCP service is not enabled").to_string());
    }
    let allowed: HashSet<String> = config.host_ids.iter().cloned().collect();
    let hosts = db.list().map_err(|e| format!("{}: {e}", crate::i18n::t("读取主机失败", "Failed to read hosts")))?;

    if name == "list_hosts" {
        let visible: Vec<serde_json::Value> = hosts
            .iter()
            .filter(|h| allowed.contains(&h.id))
            .map(|h| {
                serde_json::json!({
                    "id": h.id,
                    "name": h.name,
                    "address": h.address,
                    "port": h.port,
                    "username": h.username,
                    "auth_type": h.auth_type
                })
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&visible).unwrap_or_default());
    }

    let host = resolve_host(&hosts, args.get("host").and_then(|h| h.as_str()))?;
    if !allowed.contains(&host.id) {
        return Err(if crate::i18n::is_en() {
            format!("Host {} is not authorized for the MCP service; select it in buffTerm first", host.name)
        } else {
            format!("主机 {} 未授权给 MCP 服务，请先在 buffTerm 中勾选该服务器", host.name)
        });
    }
    match name {
        "resource_usage" => {
            let script = if crate::i18n::is_en() {
            "echo '-- Disk --'; df -h; echo; echo '-- Memory --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- Load --'; uptime; echo; echo '-- TOP processes --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8"
        } else {
            "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8"
        };
            run_and_log(app, host, name, script, 25, None).await
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| crate::i18n::t("缺少 path 参数", "Missing path parameter").to_string())?;
            // 先解析符号链接真实目标：/tmp/link -> /etc/shadow 不能因为
            // 输入路径不含敏感片段就绕过审批/只读拒绝。
            let russh = app
                .try_state::<RusshManager>()
                .ok_or_else(|| crate::i18n::t("SSH 管理器不可用", "The SSH manager is unavailable").to_string())?;
            let real = russh
                .resolve_path(host, path)
                .await
                .unwrap_or_else(|| path.to_string());
            let sensitive = is_sensitive_path(path) || is_sensitive_path(&real);
            let mut approval: Option<bool> = None;
            if sensitive {
                match config.permission_mode {
                    McpPermissionMode::Readonly => {
                        let message = crate::i18n::t(
                            "只读模式不允许读取敏感路径，已拒绝",
                            "Sensitive paths cannot be read in read-only mode and were rejected",
                        );
                        let _ = write_audit(
                            app, host, "mcp:read_file", path, "denied", "denied",
                            Some(message), None,
                        );
                        return Err(message.to_string());
                    }
                    McpPermissionMode::Confirm => {
                        approval = Some(request_approval(app, host, &format!("read_file {real}")).await?);
                        if approval == Some(false) {
                            let _ = write_audit(
                                app, host, "mcp:read_file", path, "denied", "denied",
                                Some(crate::i18n::t("用户拒绝读取敏感路径", "The user denied reading the sensitive path")),
                                None,
                            );
                            return Err(crate::i18n::t("用户拒绝读取敏感路径", "The user denied reading the sensitive path").to_string());
                        }
                    }
                    McpPermissionMode::Allow => {}
                }
            }
            let script = format!("cat -- {}", shq(&real));
            run_and_log(app, host, name, &script, 15, approval).await
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let script = format!("ls -lah -- {}", shq(path));
            run_and_log(app, host, name, &script, 15, None).await
        }
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| crate::i18n::t("缺少 command 参数", "Missing command parameter").to_string())?
                .to_string();
            let timeout = args
                .get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(30)
                .clamp(1, 600);
            let mut approval: Option<bool> = None;
            let safe = is_safe_readonly_command(&command);
            let mut sensitive = command_has_sensitive_path(&command);
            if !sensitive && config.permission_mode != McpPermissionMode::Allow {
                if let Some(russh) = app.try_state::<RusshManager>() {
                    sensitive = russh.command_reads_sensitive_path(host, &command).await;
                }
            }
            if config.permission_mode == McpPermissionMode::Readonly {
                // 只读模式：默认拒绝；仅放行可证明安全的简单只读命令。
                // 敏感路径也不放行，即使命令本身是 cat/grep。
                if !safe || sensitive {
                    let message = crate::i18n::t(
                        "只读模式只允许安全的只读命令，已拒绝执行；敏感路径需要切换到管控模式确认",
                        "Read-only mode only allows proven safe read-only commands; sensitive paths require guarded mode confirmation",
                    );
                    let _ = write_audit(
                        app,
                        host,
                        "mcp:exec_command",
                        &command,
                        "denied",
                        "denied",
                        Some(message),
                        None,
                    );
                    return Err(message.to_string());
                }
            } else if config.permission_mode == McpPermissionMode::Confirm {
                // 管控模式：自定义规则、非安全只读命令或敏感路径都要审批。
                // 不再依赖“危险词表是否收录”，未知命令默认需要确认。
                let rule_hit = check_mcp_rule_match(app, &command);
                if rule_hit || !safe || sensitive {
                    approval = Some(request_approval(app, host, &command).await?);
                    if approval == Some(false) {
                        let _ = write_audit(
                            app,
                            host,
                            "mcp:exec_command",
                            &command,
                            "denied",
                            "denied",
                            Some(crate::i18n::t("用户拒绝执行该命令", "The user denied this command")),
                            None,
                        );
                        return Err(crate::i18n::t("用户拒绝执行该命令", "The user denied this command").to_string());
                    }
                }
            }
            run_and_log(app, host, name, &command, timeout, approval).await
        }
        _ => Err(format!("{}: {name}", crate::i18n::t("未知工具", "Unknown tool"))),
    }
}

fn exec_status(out: &ExecResult) -> &'static str {
    if out.timed_out || out.exit_code.is_some_and(|code| code != 0) {
        "error"
    } else {
        "executed"
    }
}

async fn run_and_log(
    app: &AppHandle,
    host: &Host,
    tool_name: &str,
    command: &str,
    timeout_secs: u64,
    approval: Option<bool>,
) -> Result<String, String> {
    let approval_label = match approval {
        Some(true) => "approved",
        Some(false) => "denied",
        None => "auto",
    };
    let russh = app
        .try_state::<RusshManager>()
        .ok_or_else(|| crate::i18n::t("SSH 管理器不可用", "The SSH manager is unavailable").to_string())?;
    let started = Instant::now();
    match russh
        .exec(host, command, Duration::from_secs(timeout_secs))
        .await
    {
        Ok(out) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let output = sanitize(&format_exec_output(&out));
            let _ = write_audit(
                app,
                host,
                tool_name,
                command,
                approval_label,
                exec_status(&out),
                Some(&output),
                Some(duration_ms),
            );
            Ok(output)
        }
        Err(err) => {
            let error: String = err;
            let duration_ms = started.elapsed().as_millis() as u64;
            let _ = write_audit(
                app,
                host,
                tool_name,
                command,
                approval_label,
                "error",
                Some(error.as_str()),
                Some(duration_ms),
            );
            Err(error)
        }
    }
}

fn check_mcp_rule_match(app: &AppHandle, command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    app.try_state::<Arc<Db>>()
        .and_then(|db| db.list_mcp_rules().ok())
        .unwrap_or_default()
        .iter()
        .any(|r| {
            let pattern = r.pattern.trim();
            !pattern.is_empty() && c.contains(&pattern.to_ascii_lowercase())
        })
}

async fn request_approval(
    app: &AppHandle,
    host: &Host,
    command: &str,
) -> Result<bool, String> {
    let registry = app
        .try_state::<ApprovalRegistry>()
        .ok_or_else(|| crate::i18n::t("审批服务不可用", "The approval service is unavailable").to_string())?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let rx = registry.register(request_id.clone());
    let _ = app.emit(
        "mcp:approval-request",
        serde_json::json!({
            "request_id": request_id,
            "host": host.name,
            "host_label": host.label_address(),
            "command": command,
            "timeout_secs": 600
        }),
    );
    let result = match tokio::time::timeout(Duration::from_secs(600), rx).await {
        Ok(Ok(allow)) => Ok(allow),
        Ok(Err(_)) => Err(crate::i18n::t("审批通道已关闭", "The approval channel is closed").to_string()),
        Err(_) => Err(crate::i18n::t("等待用户审批超时（10 分钟）", "Timed out waiting for user approval (10 minutes)").to_string()),
    };
    registry.remove(&request_id);
    result
}

#[allow(clippy::too_many_arguments)]
fn write_audit(
    app: &AppHandle,
    host: &Host,
    tool_name: &str,
    summary: &str,
    approval: &str,
    status: &str,
    result: Option<&str>,
    duration_ms: Option<u64>,
) -> Result<(), String> {
    let db = app.try_state::<Arc<Db>>().ok_or_else(|| crate::i18n::t("数据库不可用", "The database is unavailable").to_string())?;
    let log = crate::models::AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: None,
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: tool_name.to_string(),
        source: "mcp".to_string(),
        // 命令本身可能包含密码/Token，落库前统一脱敏
        summary: sanitize(summary).chars().take(500).collect(),
        permission_mode: "mcp".to_string(),
        approval: approval.to_string(),
        status: status.to_string(),
        result: result.map(|r| truncate(&sanitize(r), 300)),
        duration_ms,
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作审计失败: {e}"))
}

fn resolve_host<'a>(hosts: &'a [Host], key: Option<&str>) -> Result<&'a Host, String> {
    let key = key.ok_or_else(|| crate::i18n::t("缺少 host 参数", "Missing host parameter").to_string())?;
    // id 唯一，优先按 id 解析；名称/地址可能重复（手工创建时允许重名），
    // 重复时拒绝而不是默默操作第一台，避免 AI 把命令发到错误服务器。
    if let Some(host) = hosts.iter().find(|h| h.id == key) {
        return Ok(host);
    }
    let matches: Vec<&Host> = hosts
        .iter()
        .filter(|h| h.name == key || h.address == key)
        .collect();
    match matches.as_slice() {
        [host] => Ok(host),
        [] => Err(if crate::i18n::is_en() {
            format!("Host {key} not found; call list_hosts to see the available hosts")
        } else {
            format!("未找到主机 {key}，请先调用 list_hosts 查看可用主机")
        }),
        _ => Err(if crate::i18n::is_en() {
            format!(
                "Host name/address {key} matches multiple hosts; use the id returned by list_hosts"
            )
        } else {
            format!("主机名/地址 {key} 匹配到多台主机，请改用 list_hosts 返回的 id")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_result(text: &str, exit_code: Option<u32>, timed_out: bool) -> ExecResult {
        ExecResult {
            text: text.to_string(),
            stdout: text.to_string(),
            stderr: String::new(),
            exit_code,
            timed_out,
        }
    }

    #[test]
    fn exec_status_marks_clean_runs_executed() {
        assert_eq!(exec_status(&exec_result("ok", Some(0), false)), "executed");
        assert_eq!(exec_status(&exec_result("ok", None, false)), "executed");
    }

    #[test]
    fn exec_status_marks_nonzero_and_timeout_error() {
        assert_eq!(exec_status(&exec_result("x", Some(1), false)), "error");
        assert_eq!(exec_status(&exec_result("x", Some(0), true)), "error");
        assert_eq!(exec_status(&exec_result("x", None, true)), "error");
    }

    fn test_host(id: &str, name: &str, address: &str) -> Host {
        Host {
            id: id.to_string(),
            name: name.to_string(),
            address: address.to_string(),
            port: 22,
            username: "root".to_string(),
            auth_type: crate::models::AuthType::Key,
            key_path: None,
            notes: None,
            created_at: 0,
        }
    }

    #[test]
    fn resolve_host_prefers_id_and_rejects_ambiguous_names() {
        let hosts = vec![
            test_host("h1", "prod", "10.0.0.1"),
            test_host("h2", "prod", "10.0.0.2"),
        ];
        assert_eq!(resolve_host(&hosts, Some("prod")).unwrap_err(), {
            if crate::i18n::is_en() {
                "Host name/address prod matches multiple hosts; use the id returned by list_hosts"
                    .to_string()
            } else {
                "主机名/地址 prod 匹配到多台主机，请改用 list_hosts 返回的 id".to_string()
            }
        });
        assert_eq!(resolve_host(&hosts, Some("h2")).unwrap().id, "h2");
        assert_eq!(resolve_host(&hosts, Some("10.0.0.1")).unwrap().id, "h1");
    }

    #[test]
    fn approval_registry_remove_is_idempotent() {
        let registry = ApprovalRegistry::default();
        let _rx = registry.register("r1".to_string());
        assert_eq!(registry.pending_len(), 1);
        registry.remove("r1");
        assert_eq!(registry.pending_len(), 0);
        registry.remove("r1");
        assert_eq!(registry.pending_len(), 0);
    }

    #[test]
    fn approval_registry_cleans_pending_after_timeout() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let registry = ApprovalRegistry::default();
            let rx = registry.register("r2".to_string());
            let result = tokio::time::timeout(Duration::from_millis(10), rx).await;
            assert!(result.is_err());
            registry.remove("r2");
            assert_eq!(registry.pending_len(), 0);
        });
    }
}
