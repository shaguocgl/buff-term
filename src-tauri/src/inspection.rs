use crate::alert;
use crate::credentials;
use crate::db::Db;
use crate::models::{Host, Inspection, InspectionRun};
use crate::russh::RusshManager;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};

#[derive(Debug, Deserialize)]
pub struct InspectionInput {
    pub host_id: String,
    #[serde(default = "default_interval")]
    pub interval_min: u64,
    #[serde(default)]
    pub enabled: bool,
}

fn default_interval() -> u64 {
    60
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

// ---------- 命令接口 ----------

#[tauri::command]
pub fn list_inspections(db: State<'_, Db>) -> Result<Vec<Inspection>, String> {
    db.list_inspections()
        .map_err(|e| format!("读取巡检计划失败: {e}"))
}

#[tauri::command]
pub fn save_inspection(
    db: State<'_, Db>,
    input: InspectionInput,
    id: Option<String>,
) -> Result<Inspection, String> {
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let exists = db
        .list_inspections()
        .map_err(|e| format!("读取巡检计划失败: {e}"))?
        .iter()
        .any(|i| i.id == id);
    let ins = Inspection {
        id,
        host_id: input.host_id,
        interval_min: input.interval_min.max(1),
        enabled: input.enabled,
        last_run_at: None,
        created_at: now(),
    };
    if exists {
        db.update_inspection(&ins)
            .map_err(|e| format!("更新巡检计划失败: {e}"))?;
    } else {
        db.insert_inspection(&ins)
            .map_err(|e| format!("保存巡检计划失败: {e}"))?;
    }
    Ok(ins)
}

#[tauri::command]
pub fn delete_inspection(db: State<'_, Db>, id: String) -> Result<(), String> {
    db.delete_inspection(&id)
        .map_err(|e| format!("删除巡检计划失败: {e}"))
}

#[tauri::command]
pub fn list_inspection_runs(
    db: State<'_, Db>,
    limit: Option<u32>,
) -> Result<Vec<InspectionRun>, String> {
    let limit = limit.unwrap_or(50).min(200);
    db.list_inspection_runs(limit)
        .map_err(|e| format!("读取巡检记录失败: {e}"))
}

#[tauri::command]
pub async fn inspection_respond(
    app: AppHandle,
    db: State<'_, Db>,
    run_id: String,
) -> Result<String, String> {
    let run = db
        .get_inspection_run(&run_id)
        .map_err(|e| format!("读取巡检记录失败: {e}"))?
        .ok_or_else(|| "巡检记录不存在".to_string())?;
    let host = db
        .get_host(&run.host_id)
        .map_err(|e| format!("读取主机失败: {e}"))?
        .ok_or_else(|| "主机不存在".to_string())?;
    let summary = run.summary.clone().unwrap_or_default();
    let user = format!(
        "这是对服务器 {} 的一次安全巡检结果：\n{}\n\n请给出一份具体的处置建议清单：\
         每条建议给出可执行的命令或操作步骤，并标注风险等级。不要编造巡检结果。",
        host.label_address(),
        summary
    );
    let text = model_chat(&app, &user).await?;
    let mut updated = run.clone();
    updated.respond_text = Some(text.clone());
    db.update_inspection_run(&updated)
        .map_err(|e| format!("保存处置建议失败: {e}"))?;
    Ok(text)
}

// ---------- 后台调度 ----------

pub fn spawn_inspection_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = tick(&app).await;
        }
    });
}

async fn tick(app: &AppHandle) -> Result<(), String> {
    let db = app.state::<Db>();
    let inspections = db
        .list_inspections()
        .map_err(|e| format!("读取巡检计划失败: {e}"))?;
    let now = now();
    for ins in inspections {
        if !ins.enabled {
            continue;
        }
        let due = ins
            .last_run_at
            .map_or(true, |t| now.saturating_sub(t) >= ins.interval_min * 60);
        if !due {
            continue;
        }
        let _ = db.set_inspection_last_run(&ins.id, now);
        let app2 = app.clone();
        let db2 = app.state::<Db>();
        let host = match db2.get_host(&ins.host_id) {
            Ok(Some(h)) => h,
            _ => continue,
        };
        tauri::async_runtime::spawn(async move {
            let _ = run_inspection(&app2, &ins, host).await;
        });
    }
    Ok(())
}

async fn run_inspection(app: &AppHandle, ins: &Inspection, host: Host) -> Result<(), String> {
    let db = app.state::<Db>();
    let run_id = uuid::Uuid::new_v4().to_string();
    let started = now();
    let run = InspectionRun {
        id: run_id,
        inspection_id: ins.id.clone(),
        host_id: host.id.clone(),
        host_label: host.name.clone(),
        started_at: started,
        finished_at: None,
        status: "running".to_string(),
        risk_level: "low".to_string(),
        summary: None,
        respond_text: None,
    };
    let _ = db.insert_inspection_run(&run);

    let result = execute_inspection(app, &host).await;
    let mut updated = run.clone();
    match result {
        Ok(summary) => {
            let risk = risk_level(&summary);
            updated.finished_at = Some(now());
            updated.status = "done".to_string();
            updated.risk_level = risk.clone();
            updated.summary = Some(summary.clone());
            let _ = db.update_inspection_run(&updated);
            if risk != "low" {
                let body = format!("{}：{}", host.name, truncate(&summary, 300));
                alert::notify_channel_for_inspection(app, "KeyWisp 巡检告警", &body).await;
            }
        }
        Err(e) => {
            updated.finished_at = Some(now());
            updated.status = "error".to_string();
            updated.summary = Some(truncate(&e, 500));
            let _ = db.update_inspection_run(&updated);
        }
    }
    Ok(())
}

async fn execute_inspection(app: &AppHandle, host: &Host) -> Result<String, String> {
    let db = app.state::<Db>();
    let providers = db
        .list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?;
    let provider = providers
        .into_iter()
        .find(|p| p.enabled)
        .ok_or_else(|| "未配置启用的 AI 平台".to_string())?;
    let model = provider
        .models
        .iter()
        .find(|m| m.is_active)
        .or_else(|| provider.models.first())
        .map(|m| m.model.clone())
        .ok_or_else(|| "该平台未配置模型".to_string())?;
    let api_key = credentials::get_api_key(&provider.id)
        .ok_or_else(|| "API Key 未找到".to_string())?;
    let russh = app.state::<RusshManager>();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    let mut history = vec![
        serde_json::json!({
            "role": "system",
            "content": format!(
                "你是 KeyWisp Agent 的自动安全巡检员，正在检查服务器 {}。\n\
                 只允许执行只读命令（后端会白名单校验）。巡检重点：\n\
                 1. 登录失败与异常登录（last、auth.log、Failed password）；\n\
                 2. 异常进程与可疑监听端口（ps、ss -tlnp）；\n\
                 3. 磁盘、内存、负载异常；\n\
                 4. 关键文件改动（/etc/passwd、/etc/ssh/sshd_config 等）；\n\
                 5. 系统更新缺失与常见风险配置。\n\
                 请先调用工具收集证据，再输出结论：风险等级、发现的异常、依据、建议。使用中文。\n\
                 工具调用约定：工具名称必须是 exec_command、read_file、list_dir、resource_usage 之一，\
                 每次调用必须包含完整的 name 字段且不能为空，参数放入 arguments（JSON 对象）。",
                host.label_address()
            ),
        }),
        serde_json::json!({
            "role": "user",
            "content": "请对这台服务器执行一次完整的安全巡检。",
        }),
    ];

    let mut iterations = 0;
    loop {
        iterations += 1;
        if iterations > 8 {
            return Err("巡检工具调用次数过多，已停止".to_string());
        }
        let body = serde_json::json!({
            "model": model,
            "messages": history,
            "stream": true,
            "tools": tools_schema(),
        });
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("请求 AI 平台失败: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("AI 平台返回 HTTP {}: {}", status.as_u16(), truncate(&text, 200)));
        }

        let (content, tool_calls) = parse_stream(resp).await?;
        if tool_calls.is_empty() {
            let summary = if content.trim().is_empty() {
                "（模型未返回内容）".to_string()
            } else {
                content
            };
            return Ok(summary);
        }

        let calls_json: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|(_, acc)| {
                serde_json::json!({
                    "id": acc.id,
                    "type": "function",
                    "function": { "name": acc.name, "arguments": acc.args },
                })
            })
            .collect();
        history.push(serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls_json,
        }));

        for (_, acc) in tool_calls {
            let args = parse_args(&acc.args);
            let result = execute_tool_inspect(&russh, host, &acc.name, &args).await;
            let output = match result {
                Ok(o) => o,
                Err(e) => format!("执行失败: {e}"),
            };
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": acc.id,
                "content": output,
            }));
        }
    }
}

async fn parse_stream(
    resp: reqwest::Response,
) -> Result<(String, Vec<(usize, ToolCallAcc)>), String> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut content = String::new();
    let mut tool_calls: HashMap<usize, ToolCallAcc> = HashMap::new();
    let mut done = false;
    while let Some(chunk) = stream.next().await {
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
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let delta = &value["choices"][0]["delta"];
            if let Some(t) = delta["content"].as_str() {
                content.push_str(t);
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
                    let name = call["function"]["name"]
                        .as_str()
                        .or_else(|| call["name"].as_str());
                    if let Some(name) = name {
                        acc.name = name.to_string();
                    }
                    let args = call["function"]["arguments"]
                        .as_str()
                        .or_else(|| call["arguments"].as_str());
                    if let Some(args) = args {
                        acc.args.push_str(args);
                    }
                }
            }
        }
        if done {
            break;
        }
    }
    let mut calls: Vec<(usize, ToolCallAcc)> = tool_calls.into_iter().collect();
    calls.sort_by_key(|(idx, _)| *idx);
    Ok((content, calls))
}

async fn execute_tool_inspect(
    russh: &RusshManager,
    host: &Host,
    name: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    match name {
        "exec_command" => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .ok_or_else(|| "缺少 command 参数".to_string())?;
            if !allowed_readonly(command) {
                return Err("该命令不在只读白名单内，已拒绝执行".to_string());
            }
            let out = russh
                .exec(host, command, Duration::from_secs(30))
                .await?;
            Ok(format_tool_output(&out))
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "缺少 path 参数".to_string())?;
            let out = russh
                .exec(host, &format!("cat {}", shq(path)), Duration::from_secs(15))
                .await?;
            Ok(format_tool_output(&out))
        }
        "list_dir" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
            let out = russh
                .exec(host, &format!("ls -lah {}", shq(path)), Duration::from_secs(15))
                .await?;
            Ok(format_tool_output(&out))
        }
        "resource_usage" => {
            let script = "echo '-- 磁盘 --'; df -h; echo; echo '-- 内存 --'; free -h 2>/dev/null; echo; echo '-- 负载 --'; uptime; echo; echo '-- TOP 进程 --'; ps aux --sort=-%mem 2>/dev/null | head -8";
            let out = russh
                .exec(host, script, Duration::from_secs(25))
                .await?;
            Ok(format_tool_output(&out))
        }
        _ => {
            let mut effective = name;
            if effective.trim().is_empty() {
                if args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    effective = "exec_command";
                } else if args.get("path").and_then(|p| p.as_str()).is_some() {
                    effective = "read_file";
                }
            }
            let normalized = normalize_tool(effective);
            if normalized != name {
                return Box::pin(execute_tool_inspect(russh, host, normalized, args)).await;
            }
            eprintln!("[inspection] 未知工具调用: {name}");
            Err(format!(
                "未知工具: {name}。可用工具：exec_command（只读命令）、read_file、list_dir、resource_usage。"
            ))
        }
    }
}

fn normalize_tool(name: &str) -> &str {
    match name {
        "exec" | "shell" | "run_command" | "run" | "command" | "execute" => "exec_command",
        "read" | "cat" | "readfile" => "read_file",
        "ls" | "list" | "listdir" | "dir" => "list_dir",
        "resources" | "usage" | "system_status" | "monitor" => "resource_usage",
        _ => name,
    }
}

fn format_tool_output(out: &crate::russh::ExecResult) -> String {
    let mut text = out.text.trim().to_string();
    const MAX: usize = 12000;
    if text.chars().count() > MAX {
        let head: String = text.chars().take(8000).collect();
        let tail: String = text.chars().skip(text.chars().count() - 4000).collect();
        text = format!("{head}\n...[输出过长，已截断]...\n{tail}");
    }
    if out.timed_out {
        text.push_str("\n[命令执行超时]");
    }
    if let Some(code) = out.exit_code {
        if code != 0 {
            text.push_str(&format!("\n[退出码 {code}]"));
        }
    }
    text
}

/// 只读命令白名单：自动巡检只允许查询类命令
fn allowed_readonly(command: &str) -> bool {
    let c = command.trim();
    if c.is_empty() || c.contains('>') || c.contains(';') || c.contains("&&") || c.contains("||") {
        return false;
    }
    const DENY: &[&str] = &[
        "rm ", "mv ", "chmod", "chown", "kill", "reboot", "shutdown", "passwd", "useradd",
        "userdel", "iptables", "docker rm", "curl -o", "wget -O", "systemctl start",
        "systemctl stop", "systemctl restart", "systemctl enable", "systemctl disable",
        "echo ", "printf ",
    ];
    if DENY.iter().any(|d| c.contains(d)) {
        return false;
    }
    let first = c.split_whitespace().next().unwrap_or("");
    const ALLOW: &[&str] = &[
        "last", "journalctl", "grep", "egrep", "awk", "ss", "netstat", "lsof", "df", "free",
        "uptime", "ps", "top", "cat", "ls", "stat", "who", "w", "id", "uname", "date", "head",
        "tail", "wc", "sort", "cut", "dmesg", "ip", "hostname", "find", "systemctl", "env",
        "file", "sha256sum", "md5sum",
    ];
    ALLOW.contains(&first)
}

fn risk_level(summary: &str) -> String {
    let s = summary.to_lowercase();
    const KEYS: &[&str] = &[
        "入侵", "攻击", "爆破", "恶意", "rootkit", "可疑进程", "异常登录", "高风险", "高危",
        "暴力破解", "webshell", "后门",
    ];
    let hits = KEYS.iter().filter(|k| s.contains(**k)).count();
    if hits >= 2 {
        "high".to_string()
    } else if hits >= 1 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
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

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let head: String = chars[..max].iter().collect();
        format!("{head}…")
    }
}

async fn model_chat(app: &AppHandle, user: &str) -> Result<String, String> {
    let db = app.state::<Db>();
    let providers = db
        .list_ai_providers()
        .map_err(|e| format!("读取 AI 配置失败: {e}"))?;
    let provider = providers
        .into_iter()
        .find(|p| p.enabled)
        .ok_or_else(|| "未配置启用的 AI 平台".to_string())?;
    let model = provider
        .models
        .iter()
        .find(|m| m.is_active)
        .or_else(|| provider.models.first())
        .map(|m| m.model.clone())
        .ok_or_else(|| "该平台未配置模型".to_string())?;
    let api_key = credentials::get_api_key(&provider.id)
        .ok_or_else(|| "API Key 未找到".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": "你是 KeyWisp Agent 的安全处置顾问，回答使用中文，给出可执行的具体步骤。" },
            { "role": "user", "content": user },
        ],
        "stream": false,
    });
    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 AI 平台失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("AI 平台返回 HTTP {}: {}", status.as_u16(), truncate(&text, 200)));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("解析响应失败: {e}"))?;
    value["choices"][0]["message"]["content"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "模型未返回内容".to_string())
}

fn tools_schema() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "exec_command",
                "description": "在远程服务器上执行只读 shell 命令（后端白名单校验，写入类命令会被拒绝）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "只读命令，如 last、grep、ss -tlnp、cat /etc/ssh/sshd_config" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "读取远程文件内容",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "列出远程目录内容",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "resource_usage",
                "description": "查看服务器资源占用",
                "parameters": { "type": "object", "properties": {} }
            }
        }
    ])
}
