use crate::credentials;
use crate::db::Db;
use crate::models::{AiProvider, AuditLog, Host, McpServer};
use crate::remote;
use crate::russh::RusshManager;
use crate::session::SessionManager;
use futures_util::StreamExt;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Default)]
pub struct AgentManager {
    controls: Mutex<HashMap<u32, mpsc::Sender<Control>>>,
    histories: Mutex<HashMap<u32, Vec<serde_json::Value>>>,
}

pub enum Control {
    Approve {
        tool_call_id: String,
        allow: bool,
    },
    Cancel,
}

impl AgentManager {
    fn set_control(&self, id: u32, tx: mpsc::Sender<Control>) {
        self.controls.lock().unwrap().insert(id, tx);
    }

    fn clear_control(&self, id: u32) {
        self.controls.lock().unwrap().remove(&id);
    }

    fn history(&self, id: u32) -> Vec<serde_json::Value> {
        self.histories
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    fn save_history(&self, id: u32, history: Vec<serde_json::Value>) {
        self.histories.lock().unwrap().insert(id, history);
    }

    fn clear_history(&self, id: u32) {
        self.histories.lock().unwrap().remove(&id);
    }
}

#[derive(Clone, Serialize)]
pub struct AiStream {
    pub session_id: u32,
    pub delta: String,
}

#[derive(Clone, Serialize)]
pub struct AiTool {
    pub session_id: u32,
    pub tool_call_id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub state: String,
    pub output: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct AiDone {
    pub session_id: u32,
}

#[derive(Clone, Serialize)]
pub struct AiError {
    pub session_id: u32,
    pub message: String,
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    db: State<'_, Db>,
    sessions: State<'_, SessionManager>,
    agents: State<'_, AgentManager>,
    session_id: u32,
    message: String,
    permission_mode: String,
) -> Result<(), String> {
    if !matches!(permission_mode.as_str(), "all" | "smart" | "none") {
        return Err("无效的安全级别".to_string());
    }
    let host = sessions
        .host(session_id)
        .ok_or_else(|| "会话不存在或已断开".to_string())?;
    let providers = db
        .list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?;
    let provider = providers
        .into_iter()
        .find(|p| p.enabled)
        .ok_or_else(|| "未配置启用的 AI 平台，请先在左侧 AI 配置中添加".to_string())?;
    let model = provider
        .models
        .iter()
        .find(|m| m.is_active)
        .or_else(|| provider.models.first())
        .map(|m| m.model.clone())
        .ok_or_else(|| "该平台未配置模型，请到 AI 配置中添加".to_string())?;
    eprintln!("[agent] 使用模型: {}（{}）", model, provider.name);
    let api_key = credentials::get_api_key(&provider.id)
        .ok_or_else(|| "API Key 未找到，请在 AI 配置中检查".to_string())?;
    let danger_rules: Vec<String> = db
        .list_ai_rules()
        .map_err(|e| format!("读取智能审核规则失败: {e}"))?
        .into_iter()
        .map(|r| r.pattern)
        .collect();
    let russh = app.state::<RusshManager>();
    let mcp_servers: Vec<McpServer> = db
        .list_mcp_servers(true)
        .map_err(|e| format!("读取 MCP 服务器失败: {e}"))?;

    let (tx, rx) = mpsc::channel::<Control>();
    agents.set_control(session_id, tx);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    let mut history = agents.history(session_id);
    let system = system_prompt(&host, &provider, &model);
    if history.is_empty() {
        history.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    } else if let Some(first) = history.first_mut() {
        // 会话中途切换平台/模型时，同步刷新系统提示词中的身份描述
        if first.get("role").and_then(|r| r.as_str()) == Some("system") {
            first["content"] = serde_json::json!(system);
        }
    }
    history.push(serde_json::json!({"role": "user", "content": message}));

    let result = run_agent_loop(
        &app,
        &client,
        &url,
        &api_key,
        &model,
        &host,
        session_id,
        &permission_mode,
        &danger_rules,
        &db,
        &russh,
        &mcp_servers,
        rx,
        &mut history,
    )
    .await;

    agents.save_history(session_id, history);
    agents.clear_control(session_id);
    result
}

#[tauri::command]
pub fn agent_approve(
    agents: State<'_, AgentManager>,
    session_id: u32,
    tool_call_id: String,
    allow: bool,
) -> Result<(), String> {
    let tx = agents
        .controls
        .lock()
        .unwrap()
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "当前没有等待审批的工具调用".to_string())?;
    tx.send(Control::Approve {
        tool_call_id,
        allow,
    })
    .map_err(|_| "会话已结束".to_string())
}

#[tauri::command]
pub fn agent_cancel(agents: State<'_, AgentManager>, session_id: u32) -> Result<(), String> {
    if let Some(tx) = agents.controls.lock().unwrap().get(&session_id).cloned() {
        let _ = tx.send(Control::Cancel);
    }
    Ok(())
}

#[tauri::command]
pub fn agent_reset(agents: State<'_, AgentManager>, session_id: u32) -> Result<(), String> {
    agents.clear_history(session_id);
    Ok(())
}

async fn run_agent_loop(
    app: &AppHandle,
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    model: &str,
    host: &Host,
    session_id: u32,
    permission_mode: &str,
    danger_rules: &[String],
    db: &Db,
    russh: &RusshManager,
    mcp_servers: &[McpServer],
    rx: mpsc::Receiver<Control>,
    history: &mut Vec<serde_json::Value>,
) -> Result<(), String> {
    let mut iterations = 0;
    loop {
        iterations += 1;
        if iterations > 12 {
            let msg = "工具调用次数过多，已停止".to_string();
            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
            return Err(msg);
        }
        if let Ok(Control::Cancel) = rx.try_recv() {
            return Ok(());
        }

        let body = serde_json::json!({
            "model": model,
            "messages": history,
            "stream": true,
            "tools": tools_schema(&mcp_servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>()),
        });
        let resp = client
            .post(url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                let msg = format!("请求 AI 平台失败: {e}");
                let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
                msg
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = extract_error(&text, status);
            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
            return Err(msg);
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut content = String::new();
        let mut tool_calls: HashMap<usize, ToolCallAcc> = HashMap::new();
        let mut done = false;

        while let Some(chunk) = stream.next().await {
            if let Ok(Control::Cancel) = rx.try_recv() {
                return Ok(());
            }
            let chunk = chunk.map_err(|e| format!("读取响应流失败: {e}"))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf = buf[pos + 1..].to_string();
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line[5..].trim();
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let value: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                apply_delta(
                    &value["choices"][0]["delta"],
                    &mut content,
                    &mut tool_calls,
                    app,
                    session_id,
                );
            }
            if done {
                break;
            }
        }
        // 处理流结束时缓冲区中残留的未换行数据，避免丢失最后一段内容
        if !buf.is_empty() {
            let data = buf.trim();
            if let Some(payload) = data.strip_prefix("data:") {
                let payload = payload.trim();
                if payload != "[DONE]" {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                        apply_delta(
                            &value["choices"][0]["delta"],
                            &mut content,
                            &mut tool_calls,
                            app,
                            session_id,
                        );
                    }
                }
            }
        }

        if tool_calls.is_empty() {
            if content.is_empty() {
                content = "（模型未返回内容）".to_string();
            }
            history.push(serde_json::json!({"role": "assistant", "content": content}));
            let _ = app.emit("ai:done", AiDone { session_id });
            return Ok(());
        }

        let mut calls_json = Vec::new();
        for (_, acc) in tool_calls.iter() {
            calls_json.push(serde_json::json!({
                "id": acc.id,
                "type": "function",
                "function": { "name": acc.name, "arguments": acc.args },
            }));
        }
        history.push(serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls_json,
        }));

        for (_, acc) in tool_calls {
            if let Ok(Control::Cancel) = rx.try_recv() {
                return Ok(());
            }
            let started = Instant::now();
            let args = parse_args(&acc.args);

            let need_approval = match permission_mode {
                "all" => true,
                "none" => false,
                _ => {
                    if acc.name != "exec_command" {
                        false
                    } else {
                        let marked = args
                            .get("requires_approval")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let command = args
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        let c = command.to_ascii_lowercase();
                        marked
                            || is_dangerous(command)
                            || danger_rules
                                .iter()
                                .any(|p| !p.trim().is_empty() && c.contains(&p.to_ascii_lowercase()))
                    }
                }
            };

            if need_approval {
                let _ = app.emit(
                    "ai:tool",
                    AiTool {
                        session_id,
                        tool_call_id: acc.id.clone(),
                        name: acc.name.clone(),
                        args: args.clone(),
                        state: "request".to_string(),
                        output: None,
                    },
                );

                let decision = loop {
                    match rx.recv_timeout(Duration::from_secs(600)) {
                        Err(_) => {
                            let msg = "等待审批超时".to_string();
                            let _ = app.emit("ai:error", AiError { session_id, message: msg.clone() });
                            return Err(msg);
                        }
                        Ok(Control::Cancel) => return Ok(()),
                        Ok(Control::Approve { tool_call_id, allow })
                            if tool_call_id == acc.id =>
                        {
                            break allow;
                        }
                        Ok(Control::Approve { .. }) => continue,
                    }
                };

                if !decision {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode,
                        "denied",
                        "denied",
                        None,
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "denied".to_string(),
                            output: None,
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": "用户拒绝执行该操作",
                    }));
                    continue;
                }
            }

            let approval_label = if need_approval { "approved" } else { "auto" };
            let _ = app.emit(
                "ai:tool",
                AiTool {
                    session_id,
                    tool_call_id: acc.id.clone(),
                    name: acc.name.clone(),
                    args: args.clone(),
                    state: "running".to_string(),
                    output: None,
                },
            );

            let result = execute_tool(russh, host, &acc.name, &args, mcp_servers).await;
            match result {
                Ok(output) => {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode,
                        approval_label,
                        "executed",
                        Some(output.clone()),
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "result".to_string(),
                            output: Some(output.clone()),
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": output,
                    }));
                }
                Err(err) => {
                    let _ = insert_audit(
                        db,
                        session_id,
                        host,
                        &acc,
                        &args,
                        permission_mode,
                        approval_label,
                        "error",
                        Some(err.clone()),
                        started.elapsed().as_millis() as u64,
                    );
                    let _ = app.emit(
                        "ai:tool",
                        AiTool {
                            session_id,
                            tool_call_id: acc.id.clone(),
                            name: acc.name.clone(),
                            args: args.clone(),
                            state: "error".to_string(),
                            output: Some(err.clone()),
                        },
                    );
                    history.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": acc.id,
                        "content": format!("执行失败: {err}"),
                    }));
                }
            }
        }
    }
}

fn insert_audit(
    db: &Db,
    session_id: u32,
    host: &Host,
    acc: &ToolCallAcc,
    args: &serde_json::Value,
    permission_mode: &str,
    approval: &str,
    status: &str,
    result: Option<String>,
    duration_ms: u64,
) -> Result<(), String> {
    let summary = args
        .get("command")
        .and_then(|c| c.as_str())
        .map(String::from)
        .unwrap_or_else(|| serde_json::to_string(args).unwrap_or_default());
    let result = result.map(|r| truncate(&r, 300));
    let log = AuditLog {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now(),
        session_id: Some(session_id),
        host_id: host.id.clone(),
        host_label: format!("{} ({})", host.name, host.label_address()),
        tool_name: acc.name.clone(),
        summary: truncate(&summary, 500),
        permission_mode: permission_mode.to_string(),
        approval: approval.to_string(),
        status: status.to_string(),
        result,
        duration_ms: Some(duration_ms),
    };
    db.insert_audit_log(&log)
        .map_err(|e| format!("写入操作日志失败: {e}"))
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head: String = chars[..max].iter().collect();
        format!("{head}…")
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn apply_delta(
    delta: &serde_json::Value,
    content: &mut String,
    tool_calls: &mut HashMap<usize, ToolCallAcc>,
    app: &AppHandle,
    session_id: u32,
) {
    if let Some(t) = delta["content"].as_str() {
        content.push_str(t);
        let _ = app.emit(
            "ai:stream",
            AiStream {
                session_id,
                delta: t.to_string(),
            },
        );
    }
    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            let acc = tool_calls.entry(index).or_default();
            if let Some(id) = call["id"].as_str() {
                if acc.id.is_empty() {
                    acc.id = id.to_string();
                }
            }
            if let Some(name) = call["function"]["name"].as_str() {
                acc.name = name.to_string();
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                acc.args.push_str(args);
            }
        }
    }
}

async fn execute_tool(
    russh: &RusshManager,
    host: &Host,
    name: &str,
    args: &serde_json::Value,
    mcp_servers: &[McpServer],
) -> Result<String, String> {
    match name {
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| "缺少 command 参数".to_string())?;
            let timeout = args.get("timeout_secs").and_then(|t| t.as_u64()).unwrap_or(30);
            let out = russh
                .exec(host, command, std::time::Duration::from_secs(timeout))
                .await?;
            Ok(sanitize(&format_exec(&out)))
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "缺少 path 参数".to_string())?;
            let out = russh
                .exec(host, &format!("cat {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec(&out)))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let out = russh
                .exec(host, &format!("ls -lah {}", shq(path)), std::time::Duration::from_secs(15))
                .await?;
            Ok(sanitize(&format_exec(&out)))
        }
        "resource_usage" => {
            let script = "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; (free -h 2>/dev/null || vm_stat); echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; (ps aux --sort=-%mem 2>/dev/null || ps aux) | head -8";
            let out = russh
                .exec(host, script, std::time::Duration::from_secs(25))
                .await?;
            Ok(sanitize(&format_exec(&out)))
        }
        "use_mcp_tool" => {
            let server_name = args
                .get("server")
                .and_then(|s| s.as_str())
                .ok_or_else(|| "缺少 server 参数".to_string())?;
            let tool = args
                .get("tool")
                .and_then(|t| t.as_str())
                .ok_or_else(|| "缺少 tool 参数".to_string())?;
            let arguments = args.get("arguments").cloned().unwrap_or(serde_json::json!({}));
            let server = mcp_servers
                .iter()
                .find(|s| s.name == server_name)
                .ok_or_else(|| format!("MCP 服务器 {server_name} 未配置或未启用"))?;
            let output = crate::mcp::call_tool(server, tool, arguments).await?;
            Ok(sanitize(&output))
        }
        _ => Err(format!("未知工具: {name}")),
    }
}

fn format_exec(out: &crate::russh::ExecResult) -> String {
    format_output(&remote::RemoteOutput {
        text: out.text.clone(),
        exit_code: out.exit_code.map(|c| c as i32),
        timed_out: out.timed_out,
    })
}

/// 命令输出进入模型上下文前过滤敏感信息
fn sanitize(text: &str) -> String {
    struct Rule {
        re: Regex,
        keep_key: bool,
    }
    static RE: OnceLock<Vec<Rule>> = OnceLock::new();
    let res = RE.get_or_init(|| {
        vec![
            // 常见密钥/口令键值对：password=xxx / token: "xxx"
            Rule {
                re: Regex::new(
                    r#"(?i)(password|passwd|pwd|secret|token|api[_-]?key)(\s*[:=]\s*["']?)[^\s"',;]{6,}"#,
                )
                .unwrap(),
                keep_key: true,
            },
            // AWS Access Key
            Rule { re: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(), keep_key: false },
            // OpenAI / DeepSeek 风格 sk- 密钥
            Rule { re: Regex::new(r"sk-[A-Za-z0-9_\-]{16,}").unwrap(), keep_key: false },
            // PEM 私钥块
            Rule {
                re: Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                keep_key: false,
            },
            // Bearer / Basic 认证头
            Rule {
                re: Regex::new(r"(?i)authorization:\s*(basic|bearer)\s+[^\r\n]+").unwrap(),
                keep_key: false,
            },
            Rule {
                re: Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._\-]{20,}").unwrap(),
                keep_key: false,
            },
            // 常见云厂商密钥片段
            Rule {
                re: Regex::new(
                    r#"(?i)(access[_-]?key[_-]?id|secret[_-]?access[_-]?key)(\s*[:=]\s*["']?)[^\s"',;]{10,}"#,
                )
                .unwrap(),
                keep_key: true,
            },
        ]
    });
    let mut out = text.to_string();
    for rule in res {
        out = if rule.keep_key {
            rule.re.replace_all(&out, "${1}${2}***").to_string()
        } else {
            rule.re.replace_all(&out, "***").to_string()
        };
    }
    out
}

/// 智能审核模式下判断命令是否有风险
fn is_dangerous(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "rm -r",
        "mkfs",
        "dd if=",
        "iptables",
        "ufw ",
        "systemctl stop",
        "systemctl restart",
        "systemctl disable",
        "systemctl mask",
        "shutdown",
        "reboot",
        "poweroff",
        "chmod -r",
        "chown -r",
        "fdisk",
        "parted",
        "pvremove",
        "vgremove",
        "lvremove",
        "userdel",
        "groupdel",
        "drop database",
        "truncate table",
        "delete from",
        "kill -9",
        ">/dev/sd",
    ];
    PATTERNS.iter().any(|p| c.contains(p))
}

fn format_output(out: &remote::RemoteOutput) -> String {
    let mut text = out.text.trim().to_string();
    const MAX: usize = 12000;
    if text.chars().count() > MAX {
        let head: String = text.chars().take(8000).collect();
        let tail: String = text.chars().skip(text.chars().count() - 4000).collect();
        text = format!("{head}\n...[输出过长，已截断]...\n{tail}");
    }
    if out.timed_out {
        text.push_str("\n[命令执行超时，输出可能不完整]");
    }
    if let Some(code) = out.exit_code {
        if code != 0 {
            text.push_str(&format!("\n[退出码 {code}]"));
        }
    }
    text
}

fn parse_args(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ "command": raw.trim() }),
    }
}

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn extract_error(text: &str, status: reqwest::StatusCode) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| text.chars().take(200).collect());
    format!("AI 平台返回 HTTP {}: {}", status.as_u16(), detail)
}

fn system_prompt(host: &Host, provider: &AiProvider, model: &str) -> String {
    format!(
        "你是 KeyWisp Agent，运行在用户本地的 SSH 管理工具中，帮助用户管理远程服务器。\n\
         当前由 {} 平台提供能力，当前配置的底层模型是 {}。\n\
         当前连接的服务器：{}（{}@{}:{}）\n\
         可用工具：exec_command（执行命令）、read_file（读文件）、list_dir（列目录）、resource_usage（资源占用）。\n\
         规则：\n\
         1. 所有 exec_command 都会经过用户批准，获批后才执行，请先说明意图。\n\
         2. 命令输出可能被截断，只基于已有信息回答，不要编造。\n\
         3. 遇到破坏性操作（删除、格式化、改权限、停服务等）时，明确提示风险并给出命令原文。\n\
         4. 使用中文回答，简洁、专业、有条理。\n\
         5. 身份说明：当用户询问“你是什么模型/你由谁开发”时，如实回答你由 {} 驱动、配置的模型为 {}，
            以及你是 KeyWisp Agent；不要声称自己是任何其他 AI 助手（如 ChatGPT、Claude、Gemini 等），
            也不要编造版本号或开发厂商信息。",
        provider.name,
        model,
        host.name,
        host.username,
        host.address,
        host.port,
        provider.name,
        model
    )
}

fn tools_schema(server_names: &[String]) -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "exec_command",
                "description": "在远程服务器上执行一条 shell 命令并返回输出。默认超时 30 秒。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout_secs": { "type": "number", "description": "超时秒数，默认 30" },
                        "requires_approval": { "type": "boolean", "description": "如果你认为该命令有危险（删除、格式化、修改系统状态、影响服务等），设为 true，便于用户安全策略处理" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取远程服务器上的文件内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "远程文件路径" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出远程服务器上的目录内容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "目录路径，默认当前目录" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "resource_usage",
                "description": "查看服务器磁盘、内存、负载和 CPU/内存占用最高的进程",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "use_mcp_tool",
                "description": format!(
                    "调用已配置的外部 MCP 服务器工具，扩展能力（如数据库查询、云平台操作等）。可用的 MCP 服务器：{}",
                    if server_names.is_empty() {
                        "（无，请先在 MCP 配置中添加）".to_string()
                    } else {
                        server_names.join("、")
                    }
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string", "description": "MCP 服务器名称，必须来自上面的列表" },
                        "tool": { "type": "string", "description": "要调用的工具名" },
                        "arguments": { "type": "object", "description": "传给工具的参数对象" }
                    },
                    "required": ["server", "tool", "arguments"]
                }
            }
        }
    ])
}
