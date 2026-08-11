use crate::db::Db;
use crate::models::AuditLog;
use tauri::State;

#[tauri::command]
pub fn list_audit_logs(db: State<'_, Db>, limit: Option<u32>) -> Result<Vec<AuditLog>, String> {
    let limit = limit.unwrap_or(100).min(500);
    db.list_audit_logs(limit)
        .map_err(|e| format!("读取操作日志失败: {e}"))
}
