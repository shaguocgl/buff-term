use std::collections::HashSet;
use std::sync::Arc;
use crate::credentials;
use crate::ai::TestResult;
use crate::db::Db;
use crate::models::{AuthType, Host};
use crate::sshconfig;
use crate::util::now;
use serde::{Deserialize, Serialize};
use std::fs;
use tauri::{AppHandle, State};

#[derive(Debug, Deserialize)]
pub struct HostInput {
    pub name: String,
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    #[serde(default = "default_auth_type")]
    pub auth_type: AuthType,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

fn default_port() -> u16 {
    22
}

fn default_auth_type() -> AuthType {
    AuthType::Key
}

pub fn host_from_input(input: HostInput) -> Host {
    Host {
        id: uuid::Uuid::new_v4().to_string(),
        name: input.name,
        address: input.address,
        port: input.port,
        username: input.username,
        auth_type: input.auth_type,
        key_path: input.key_path,
        notes: input.notes,
        created_at: now(),
    }
}

pub fn list(db: &Db) -> Result<Vec<Host>, String> {
    db.list().map_err(|e| format!("读取主机列表失败: {e}"))
}

/// 按 id 从数据库加载主机（凭据类操作的唯一受信来源）。
/// 所有需要连接主机的命令都应只接收 host_id 并从这里取 Host，
/// 避免前端传入的 Host 对象把 address/username 指向恶意服务器、
/// 却仍用该 id 对应的已保存密码认证（凭据重定向）。
pub fn load_host(db: &Db, host_id: &str) -> Result<Host, String> {
    db.get_host(host_id)
        .map_err(|e| format!("读取主机失败: {e}"))?
        .ok_or_else(|| "主机不存在或已被删除".to_string())
}

pub fn create(db: &Db, input: HostInput) -> Result<Host, String> {
    let host = host_from_input(input);
    db.insert(&host)
        .map_err(|e| format!("保存主机失败: {e}"))?;
    Ok(host)
}

pub fn update(db: &Db, host: Host) -> Result<(), String> {
    db.update(&host).map_err(|e| format!("更新主机失败: {e}"))
}

pub fn delete(db: &Db, id: String) -> Result<(), String> {
    // 级联清理该主机的指标/审计/巡检/整改，避免孤儿数据
    db.delete_host_cascade(&id)
        .map_err(|e| format!("删除主机失败: {e}"))?;
    credentials::delete_password(&id);
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
}

pub fn import_config(db: &Db, path: Option<String>) -> Result<ImportResult, String> {
    let path = path.unwrap_or_else(default_ssh_config_path);
    let content =
        fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path))?;
    let inputs = sshconfig::parse(&content);
    let mut existing: HashSet<String> = db
        .list()
        .map_err(|e| format!("读取主机列表失败: {e}"))?
        .into_iter()
        .map(|h| h.name)
        .collect();
    let (accepted, skipped) = filter_new_hosts(inputs, &mut existing);
    let mut imported = 0;
    for input in accepted {
        let host = host_from_input(input);
        db.insert(&host).map_err(|e| format!("导入主机失败: {e}"))?;
        imported += 1;
    }
    Ok(ImportResult { imported, skipped })
}

fn filter_new_hosts(
    inputs: Vec<HostInput>,
    existing: &mut HashSet<String>,
) -> (Vec<HostInput>, usize) {
    let mut skipped = 0;
    let mut accepted = Vec::new();
    for input in inputs {
        if !existing.insert(input.name.clone()) {
            skipped += 1;
            continue;
        }
        accepted.push(input);
    }
    (accepted, skipped)
}

fn default_ssh_config_path() -> String {
    match crate::util::user_home_dir() {
        Some(home) => home
            .join(".ssh")
            .join("config")
            .to_string_lossy()
            .to_string(),
        None => ".ssh/config".to_string(),
    }
}

#[tauri::command]
pub fn list_hosts(db: State<'_, Arc<Db>>) -> Result<Vec<Host>, String> {
    list(&db)
}

#[tauri::command]
pub fn create_host(db: State<'_, Arc<Db>>, input: HostInput) -> Result<Host, String> {
    create(&db, input)
}

#[tauri::command]
pub fn update_host(db: State<'_, Arc<Db>>, host: Host) -> Result<(), String> {
    update(&db, host)
}

#[tauri::command]
pub fn delete_host(
    db: State<'_, Arc<Db>>,
    agents: State<'_, crate::agent::AgentManager>,
    id: String,
) -> Result<(), String> {
    // 先清理该主机的 AI 会话历史（借用 id），再删除主机（move id）
    agents.clear_history(&id);
    delete(&db, id)?;
    Ok(())
}

#[tauri::command]
pub fn import_ssh_config(db: State<'_, Arc<Db>>, path: Option<String>) -> Result<ImportResult, String> {
    import_config(&db, path)
}

#[tauri::command]
pub fn save_host_credentials(app: AppHandle, id: String, password: String) -> Result<(), String> {
    let _ = app;
    credentials::save_password(&id, &password)
}

/// 测试主机连接。传入的 Host 来自前端表单（可能是未保存的新主机）。
/// 安全约束：仅当传入的 id 已在库中且 address/port/username 与库中记录一致时，
/// 才允许回退使用该 id 已保存的密码；否则密码认证必须显式传入 password，
/// 防止把已保存凭据发送到任意地址（凭据重定向）。
#[tauri::command]
pub async fn test_host_connection(
    db: State<'_, Arc<Db>>,
    host: Host,
    password: Option<String>,
) -> Result<TestResult, String> {
    let saved = db
        .get_host(&host.id)
        .map_err(|e| format!("读取主机失败: {e}"))?;
    if let Some(saved) = saved {
        let matches = saved.address == host.address
            && saved.port == host.port
            && saved.username == host.username;
        if !matches {
            // 库中已有该 id，但传入的地址/用户不一致：禁止复用已保存密码
            if host.auth_type == AuthType::Password {
                if password.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    return Ok(TestResult {
                        ok: false,
                        message: "主机的连接信息与已保存记录不一致，请显式输入密码后再测试".to_string(),
                    });
                }
            }
        }
    } else if host.auth_type == AuthType::Password
        && password.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        return Ok(TestResult {
            ok: false,
            message: "密码认证需要输入密码后再测试".to_string(),
        });
    }
    let russh = crate::russh::RusshManager::new();
    match russh.test_connection(&host, password).await {
        Ok(message) => Ok(TestResult {
            ok: true,
            message,
        }),
        Err(message) => Ok(TestResult {
            ok: false,
            message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> HostInput {
        HostInput {
            name: name.to_string(),
            address: format!("{name}.example.com"),
            port: 22,
            username: "root".to_string(),
            auth_type: AuthType::Key,
            key_path: None,
            notes: None,
        }
    }

    #[test]
    fn import_dedups_existing_and_intra_batch_names() {
        let mut existing: HashSet<String> = ["web".to_string()].into_iter().collect();
        let (accepted, skipped) = filter_new_hosts(
            vec![input("web"), input("db"), input("db"), input("cache")],
            &mut existing,
        );
        assert_eq!(skipped, 2);
        let names: Vec<&str> = accepted.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["db", "cache"]);
        let (accepted2, skipped2) =
            filter_new_hosts(vec![input("db"), input("new")], &mut existing);
        assert_eq!(skipped2, 1);
        assert_eq!(accepted2.len(), 1);
        assert_eq!(accepted2[0].name, "new");
    }
}
