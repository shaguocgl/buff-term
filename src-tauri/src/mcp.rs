use crate::db::Db;
use crate::models::McpServer;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tauri::State;

#[derive(Debug, Deserialize)]
pub struct McpServerInput {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tauri::command]
pub fn list_mcp_servers(db: State<'_, Db>) -> Result<Vec<McpServer>, String> {
    db.list_mcp_servers(false)
        .map_err(|e| format!("读取 MCP 服务器失败: {e}"))
}

#[tauri::command]
pub fn save_mcp_server(
    db: State<'_, Db>,
    input: McpServerInput,
    id: Option<String>,
) -> Result<McpServer, String> {
    if input.command.trim().is_empty() {
        return Err("启动命令不能为空".to_string());
    }
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let exists = db
        .list_mcp_servers(false)
        .map_err(|e| format!("读取 MCP 服务器失败: {e}"))?
        .iter()
        .any(|s| s.id == id);
    let server = McpServer {
        id,
        name: input.name,
        command: input.command,
        args: input.args,
        enabled: input.enabled,
        created_at: now(),
    };
    if exists {
        db.update_mcp_server(&server)
            .map_err(|e| format!("更新 MCP 服务器失败: {e}"))?;
    } else {
        db.insert_mcp_server(&server)
            .map_err(|e| format!("保存 MCP 服务器失败: {e}"))?;
    }
    Ok(server)
}

#[tauri::command]
pub fn delete_mcp_server(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_mcp_server(&id)
        .map_err(|e| format!("删除 MCP 服务器失败: {e}"))
}

/// 测试连接：启动服务器并列出可用工具
#[tauri::command]
pub async fn mcp_test(server: McpServer) -> Result<Vec<McpToolInfo>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = list_tools_blocking(&server);
        let _ = tx.send(result);
    });
    tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .map_err(|_| "MCP 连接超时".to_string())?
        .map_err(|_| "MCP 进程异常退出".to_string())?
}

/// 供 Agent 调用的工具执行入口
pub async fn call_tool(
    server: &McpServer,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = server.clone();
    let tool = tool.to_string();
    std::thread::spawn(move || {
        let result = call_tool_blocking(&server, &tool, arguments);
        let _ = tx.send(result);
    });
    tokio::time::timeout(Duration::from_secs(60), rx)
        .await
        .map_err(|_| "MCP 调用超时".to_string())?
        .map_err(|_| "MCP 进程异常退出".to_string())?
}

// ---------- 自研 MCP stdio 客户端 ----------

struct McpProc {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
}

fn spawn_mcp(server: &McpServer) -> Result<McpProc, String> {
    let args: Vec<String> = server.args.split_whitespace().map(String::from).collect();
    let mut child = std::process::Command::new(&server.command)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 MCP 服务器失败（{}）: {e}", server.command))?;
    let stdin = child.stdin.take().ok_or_else(|| "无法获取 MCP stdin".to_string())?;
    let stdout: ChildStdout = child.stdout.take().ok_or_else(|| "无法获取 MCP stdout".to_string())?;
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stderr, &mut sink);
        });
    }
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
    Ok(McpProc { child, stdin, rx })
}

impl McpProc {
    fn request(&mut self, req: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
        let mut payload = serde_json::to_string(req).map_err(|e| format!("序列化请求失败: {e}"))?;
        payload.push('\n');
        self.stdin
            .write_all(payload.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("写入 MCP 请求失败: {e}"))?;

        let deadline = Instant::now() + timeout;
        let req_id = req.get("id").cloned();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("MCP 响应超时".to_string());
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    let text = String::from_utf8_lossy(&line);
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
                        continue;
                    };
                    if value.get("id").is_none() {
                        continue; // 通知类消息
                    }
                    if let Some(id) = &req_id {
                        if value.get("id") != Some(id) {
                            continue; // 不是本次请求的响应
                        }
                    }
                    return Ok(value);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("MCP 响应超时".to_string()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("MCP 进程提前退出".to_string());
                }
            }
        }
    }
}

fn initialize(proc: &mut McpProc) -> Result<(), String> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "keywisp-agent", "version": "0.1.0" }
        }
    });
    let resp = proc.request(&req, Duration::from_secs(10))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("MCP 初始化失败: {err}"));
    }
    Ok(())
}

fn list_tools_blocking(server: &McpServer) -> Result<Vec<McpToolInfo>, String> {
    let mut proc = spawn_mcp(server)?;
    initialize(&mut proc)?;
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
    let resp = proc.request(&req, Duration::from_secs(10))?;
    let tools = resp["result"]["tools"].as_array().cloned().unwrap_or_default();
    let list: Vec<McpToolInfo> = tools
        .iter()
        .map(|t| McpToolInfo {
            name: t["name"].as_str().unwrap_or("").to_string(),
            description: t["description"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    let _ = proc.child.kill();
    Ok(list)
}

fn call_tool_blocking(
    server: &McpServer,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    let mut proc = spawn_mcp(server)?;
    initialize(&mut proc)?;
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let resp = proc.request(&req, Duration::from_secs(60))?;
    let _ = proc.child.kill();
    if let Some(err) = resp.get("error") {
        return Err(format!("MCP 工具调用失败: {err}"));
    }
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    let content = resp["result"]["content"].as_array().cloned().unwrap_or_default();
    let mut text = String::new();
    for item in content {
        if let Some(t) = item["text"].as_str() {
            text.push_str(t);
            text.push('\n');
        }
    }
    if is_error {
        Err(if text.trim().is_empty() {
            "MCP 工具返回错误".to_string()
        } else {
            text
        })
    } else {
        Ok(text)
    }
}
