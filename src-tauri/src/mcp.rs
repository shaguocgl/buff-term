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
use crate::russh::RusshManager;
use crate::safety::{is_write_operation, sanitize};
use crate::util::{format_exec_output, generate_token, now, shq};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
}

// ---------- 运行管理 ----------

struct RunningMcp {
    shutdown: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub struct McpServiceManager {
    inner: Mutex<Option<RunningMcp>>,
}

impl McpServiceManager {
    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().is_some()
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
            Some(tx) => tx.send(allow).map_err(|_| "审批接收方已关闭".to_string()),
            None => Err("审批请求不存在或已超时".to_string()),
        }
    }
}

// ---------- Tauri 命令 ----------

#[tauri::command]
pub fn get_mcp_service(
    db: State<'_, Arc<Db>>,
    manager: State<'_, McpServiceManager>,
) -> Result<McpServiceView, String> {
    let config = db
        .get_mcp_service()
        .map_err(|e| format!("读取 MCP 服务配置失败: {e}"))?;
    Ok(McpServiceView {
        running: manager.is_running(),
        config,
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
        return Err("启用 MCP 服务前，请至少勾选一台服务器".to_string());
    }

    let mut config = db
        .get_mcp_service()
        .map_err(|e| format!("读取 MCP 服务配置失败: {e}"))?;
    config.enabled = input.enabled;
    config.host_ids = input.host_ids;
    config.permission_mode = input.permission_mode;
    config.updated_at = now();

    if !config.enabled {
        stop_service(&manager);
        config.port = None;
        db.save_mcp_service(&config)
            .map_err(|e| format!("保存 MCP 服务配置失败: {e}"))?;
        return Ok(McpServiceView {
            running: false,
            config,
        });
    }

    // 首次启用时生成 token
    if config.token.is_none() {
        config.token = Some(generate_token());
    }
    // 未运行时才启动；已运行时只需更新配置（请求时实时读取）
    if !manager.is_running() {
        let port = start_service(&app, &manager)?;
        config.port = Some(port);
    }
    db.save_mcp_service(&config)
        .map_err(|e| format!("保存 MCP 服务配置失败: {e}"))?;
    Ok(McpServiceView {
        running: true,
        config,
    })
}

#[tauri::command]
pub fn rotate_mcp_token(
    db: State<'_, Arc<Db>>,
    manager: State<'_, McpServiceManager>,
) -> Result<McpServiceView, String> {
    let mut config = db
        .get_mcp_service()
        .map_err(|e| format!("读取 MCP 服务配置失败: {e}"))?;
    config.token = Some(generate_token());
    config.updated_at = now();
    db.save_mcp_service(&config)
        .map_err(|e| format!("保存 MCP 服务配置失败: {e}"))?;
    Ok(McpServiceView {
        running: manager.is_running(),
        config,
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
        .map_err(|e| format!("读取 MCP 管控规则失败: {e}"))
}

#[tauri::command]
pub fn add_mcp_rule(db: State<'_, Arc<Db>>, pattern: String) -> Result<McpRule, String> {
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return Err("管控命令不能为空".to_string());
    }
    let rule = McpRule {
        id: uuid::Uuid::new_v4().to_string(),
        pattern,
        enabled: true,
        created_at: now(),
    };
    db.insert_mcp_rule(&rule)
        .map_err(|e| format!("保存管控规则失败: {e}"))?;
    Ok(rule)
}

#[tauri::command]
pub fn delete_mcp_rule(db: State<'_, Arc<Db>>, id: String) -> Result<(), String> {
    db.delete_mcp_rule(&id)
        .map_err(|e| format!("删除管控规则失败: {e}"))
}

// ---------- 服务生命周期 ----------

pub(crate) fn start_service(app: &AppHandle, manager: &McpServiceManager) -> Result<u16, String> {
    let (port_tx, port_rx) = mpsc::channel::<Result<u16, String>>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let app = app.clone();
    std::thread::spawn(move || mcp_server_main(app, port_tx, shutdown_rx));
    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "MCP 服务启动超时".to_string())??;
    manager.set(Some(RunningMcp {
        shutdown: shutdown_tx,
    }));
    Ok(port)
}

fn stop_service(manager: &McpServiceManager) {
    if let Some(running) = manager.inner.lock().unwrap().take() {
        let _ = running.shutdown.send(());
    }
}

fn mcp_server_main(
    app: AppHandle,
    port_tx: mpsc::Sender<Result<u16, String>>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let server = match Server::http(&format!("127.0.0.1:{DEFAULT_PORT}")) {
        Ok(s) => s,
        Err(_) => match Server::http("127.0.0.1:0") {
            Ok(s) => s,
            Err(e) => {
                let _ = port_tx.send(Err(format!("启动 MCP 服务失败: {e}")));
                return;
            }
        },
    };
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
    if port_tx.send(Ok(port)).is_err() {
        return;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[mcp] 创建 runtime 失败: {e}");
            return;
        }
    };
    let rt = Arc::new(rt);

    loop {
        match server.recv_timeout(Duration::from_millis(300)) {
            Ok(Some(request)) => {
                let app = app.clone();
                let rt = rt.clone();
                std::thread::spawn(move || handle_http(app, rt, request));
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[mcp] HTTP 服务器错误: {e}");
                break;
            }
        }
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
    }
}

// ---------- HTTP 层 ----------

fn cors_headers() -> Vec<Header> {
    vec![
        Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        Header::from_bytes(
            &b"Access-Control-Allow-Headers"[..],
            &b"Authorization, Content-Type, MCP-Protocol-Version, MCP-Session-Id"[..],
        )
        .unwrap(),
        Header::from_bytes(
            &b"Access-Control-Allow-Methods"[..],
            &b"GET, POST, OPTIONS"[..],
        )
        .unwrap(),
    ]
}

fn attach_headers<R: Read>(response: Response<R>, headers: Vec<Header>) -> Response<R> {
    headers
        .into_iter()
        .fold(response, |resp, header| resp.with_header(header))
}

fn check_auth(app: &AppHandle, request: &Request) -> bool {
    let db = match app.try_state::<Arc<Db>>() {
        Some(db) => db,
        None => return false,
    };
    let token = db
        .get_mcp_service()
        .ok()
        .and_then(|c| c.token)
        .unwrap_or_default();
    if token.is_empty() {
        return false;
    }
    if let Some(h) = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
    {
        let v = h.value.as_str();
        if v == token {
            return true;
        }
        if let Some(bearer) = v.strip_prefix("Bearer ") {
            if bearer == token {
                return true;
            }
        }
        if let Some(bearer) = v.strip_prefix("bearer ") {
            if bearer == token {
                return true;
            }
        }
    }
    if let Some(query) = request.url().split('?').nth(1) {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("access_token=") {
                let decoded = percent_encoding::percent_decode_str(v).decode_utf8_lossy();
                if decoded == token {
                    return true;
                }
            }
        }
    }
    false
}

fn handle_http(app: AppHandle, rt: Arc<tokio::runtime::Runtime>, mut request: Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");

    if method == Method::Options {
        let resp = attach_headers(Response::empty(StatusCode(204)), cors_headers());
        let _ = request.respond(resp);
        return;
    }
    if path != "/mcp" && path != "/" {
        let _ = request.respond(
            attach_headers(
                Response::from_string("not found").with_status_code(StatusCode(404)),
                cors_headers(),
            ),
        );
        return;
    }
    if !check_auth(&app, &request) {
        let _ = request.respond(
            attach_headers(
                Response::from_string("未授权：请检查 MCP 配置中的 token")
                    .with_status_code(StatusCode(401)),
                cors_headers(),
            ),
        );
        return;
    }

    match method {
        Method::Get => handle_sse(request),
        Method::Post => {
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let _ = request.respond(
                    attach_headers(
                        Response::from_string("读取请求体失败")
                            .with_status_code(StatusCode(400)),
                        cors_headers(),
                    ),
                );
                return;
            }
            let response = rt.block_on(handle_jsonrpc(&app, &body));
            let mut headers = cors_headers();
            headers.push(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
            let _ = request.respond(attach_headers(Response::from_string(response), headers));
        }
        _ => {
            let _ = request.respond(
                attach_headers(Response::empty(StatusCode(405)), cors_headers()),
            );
        }
    }
}

fn handle_sse(request: Request) {
    // tiny_http 的 respond 会把响应写入 BufWriter，无限流 body 阻塞读取时
    // header 永远无法 flush；这里直接用 into_writer 手动写头并立即 flush。
    let mut writer = request.into_writer();
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n"
    );
    if writer.write_all(headers.as_bytes()).is_err() {
        return;
    }
    if writer.flush().is_err() {
        return;
    }
    let _ = writer.write_all(b": connected\n\n");
    let _ = writer.flush();
    loop {
        std::thread::sleep(Duration::from_secs(15));
        if writer.write_all(b": ping\n\n").is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}

// ---------- JSON-RPC / MCP 协议 ----------

async fn handle_jsonrpc(app: &AppHandle, body: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return json_error(None, -32700, &format!("解析请求失败: {e}")),
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
                        "content": [{ "type": "text", "text": format!("错误: {e}") }],
                        "isError": true
                    }),
                ),
            }
        }
        _ => json_error(id, -32601, &format!("未知方法: {method}")),
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
            "description": "列出当前 MCP 服务授权可操作的服务器（仅包含用户勾选的主机）",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "resource_usage",
            "description": "查看指定服务器（host 参数）的磁盘、内存、负载和占用最高的进程。host 可以是 list_hosts 返回的 id、name 或 address",
            "inputSchema": {
                "type": "object",
                "properties": { "host": { "type": "string", "description": "主机 id / name / address" } },
                "required": ["host"]
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "读取指定服务器上的文件内容",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "主机 id / name / address" },
                    "path": { "type": "string", "description": "服务器上的文件路径" }
                },
                "required": ["host", "path"]
            }
        }),
        serde_json::json!({
            "name": "list_dir",
            "description": "列出指定服务器上的目录内容",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "主机 id / name / address" },
                    "path": { "type": "string", "description": "目录路径，默认当前目录" }
                },
                "required": ["host"]
            }
        }),
        serde_json::json!({
            "name": "exec_command",
            "description": "在指定服务器上执行一条 shell 命令并返回输出。只读模式下写操作会被拒绝；管控模式下命中自定义管控规则的命令会触发用户确认。host 可以是 list_hosts 返回的 id、name 或 address",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "主机 id / name / address" },
                    "command": { "type": "string", "description": "要执行的 shell 命令" },
                    "timeout_secs": { "type": "number", "description": "超时秒数，默认 30" }
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
        .ok_or_else(|| "数据库不可用".to_string())?;
    let config = db
        .get_mcp_service()
        .map_err(|e| format!("读取 MCP 服务配置失败: {e}"))?;
    if !config.enabled {
        return Err("MCP 服务未启用".to_string());
    }
    let allowed: HashSet<String> = config.host_ids.iter().cloned().collect();
    let hosts = db.list().map_err(|e| format!("读取主机失败: {e}"))?;

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
        return Err(format!(
            "主机 {} 未授权给 MCP 服务，请先在 buffTerm 中勾选该服务器",
            host.name
        ));
    }
    match name {
        "resource_usage" => {
            let script = "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8";
            run_and_log(app, host, name, script, 25, None).await
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "缺少 path 参数".to_string())?;
            let script = format!("cat {}", shq(path));
            run_and_log(app, host, name, &script, 15, None).await
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let script = format!("ls -lah {}", shq(path));
            run_and_log(app, host, name, &script, 15, None).await
        }
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| "缺少 command 参数".to_string())?
                .to_string();
            let timeout = args
                .get("timeout_secs")
                .and_then(|t| t.as_u64())
                .unwrap_or(30);
            let mut approval: Option<bool> = None;
            if config.permission_mode == McpPermissionMode::Readonly {
                // 只读模式：允许只读命令，拒绝任何写操作
                if is_write_operation(&command) {
                    let _ = write_audit(app, host, "mcp:exec_command", &command, "denied");
                    return Err(
                        "只读模式不允许写操作，已拒绝执行。如需写操作请切换到管控模式或全部放行"
                            .to_string(),
                    );
                }
            } else if config.permission_mode == McpPermissionMode::Confirm
                && check_mcp_rule_match(app, &command)
            {
                // 管控模式：仅自定义管控规则命中时弹窗审批
                approval = Some(request_approval(app, host, &command).await?);
                if approval == Some(false) {
                    let _ = write_audit(app, host, "mcp:exec_command", &command, "denied");
                    return Err("用户拒绝执行该命令".to_string());
                }
            }
            run_and_log(app, host, name, &command, timeout, approval).await
        }
        _ => Err(format!("未知工具: {name}")),
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
    let _ = write_audit(
        app,
        host,
        tool_name,
        command,
        match approval {
            Some(true) => "approved",
            Some(false) => "denied",
            None => "auto",
        },
    );
    let russh = app.try_state::<RusshManager>().ok_or_else(|| "SSH 管理器不可用".to_string())?;
    let out = russh
        .exec(host, command, Duration::from_secs(timeout_secs))
        .await?;
    Ok(sanitize(&format_exec_output(&out)))
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
        .ok_or_else(|| "审批服务不可用".to_string())?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let rx = registry.register(request_id.clone());
    let _ = app.emit(
        "mcp:approval-request",
        serde_json::json!({
            "request_id": request_id,
            "host": host.name,
            "host_label": host.label_address(),
            "command": command
        }),
    );
    match tokio::time::timeout(Duration::from_secs(600), rx).await {
        Ok(Ok(allow)) => Ok(allow),
        Ok(Err(_)) => Err("审批通道已关闭".to_string()),
        Err(_) => Err("等待用户审批超时（10 分钟）".to_string()),
    }
}

fn write_audit(
    app: &AppHandle,
    host: &Host,
    tool_name: &str,
    summary: &str,
    approval: &str,
) -> Result<(), String> {
    let db = app.try_state::<Arc<Db>>().ok_or_else(|| "数据库不可用".to_string())?;
    let log = crate::models::AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: None,
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: tool_name.to_string(),
        summary: summary.chars().take(500).collect(),
        permission_mode: "mcp".to_string(),
        approval: approval.to_string(),
        status: "ok".to_string(),
        result: None,
        duration_ms: None,
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作日志失败: {e}"))
}

fn resolve_host<'a>(hosts: &'a [Host], key: Option<&str>) -> Result<&'a Host, String> {
    let key = key.ok_or_else(|| "缺少 host 参数".to_string())?;
    hosts
        .iter()
        .find(|h| h.id == key || h.name == key || h.address == key)
        .ok_or_else(|| format!("未找到主机 {key}，请先调用 list_hosts 查看可用主机"))
}
