use crate::credentials;
use crate::ai::TestResult;
use crate::db::Db;
use crate::models::Host;
use crate::sshconfig;
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
    pub auth_type: String,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

fn default_port() -> u16 {
    22
}

fn default_auth_type() -> String {
    "key".to_string()
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
    db.delete(&id).map_err(|e| format!("删除主机失败: {e}"))?;
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
    let existing: Vec<String> = db
        .list()
        .map_err(|e| format!("读取主机列表失败: {e}"))?
        .into_iter()
        .map(|h| h.name)
        .collect();
    let mut imported = 0;
    let mut skipped = 0;
    for input in inputs {
        if existing.contains(&input.name) {
            skipped += 1;
            continue;
        }
        let host = host_from_input(input);
        db.insert(&host).map_err(|e| format!("导入主机失败: {e}"))?;
        imported += 1;
    }
    Ok(ImportResult { imported, skipped })
}

fn default_ssh_config_path() -> String {
    match std::env::var("HOME") {
        Ok(home) => format!("{home}/.ssh/config"),
        Err(_) => "/Users/current/.ssh/config".to_string(),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tauri::command]
pub fn list_hosts(db: State<'_, Db>) -> Result<Vec<Host>, String> {
    list(&db)
}

#[tauri::command]
pub fn create_host(db: State<'_, Db>, input: HostInput) -> Result<Host, String> {
    create(&db, input)
}

#[tauri::command]
pub fn update_host(db: State<'_, Db>, host: Host) -> Result<(), String> {
    update(&db, host)
}

#[tauri::command]
pub fn delete_host(db: State<'_, Db>, id: String) -> Result<(), String> {
    delete(&db, id)
}

#[tauri::command]
pub fn import_ssh_config(db: State<'_, Db>, path: Option<String>) -> Result<ImportResult, String> {
    import_config(&db, path)
}

#[tauri::command]
pub fn save_host_credentials(app: AppHandle, id: String, password: String) -> Result<(), String> {
    let _ = app;
    credentials::save_password(&id, &password)
}

#[tauri::command]
pub async fn test_host_connection(
    host: Host,
    password: Option<String>,
) -> Result<TestResult, String> {
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
