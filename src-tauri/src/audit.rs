use std::sync::Arc;
use crate::db::Db;
use crate::models::AuditLog;
use tauri::State;

/// 单次查询的审计条数上限（前端默认拉取最近 1000 条）。
const MAX_AUDIT_LIMIT: u32 = 1000;

#[tauri::command]
pub fn list_audit_logs(db: State<'_, Arc<Db>>, limit: Option<u32>) -> Result<Vec<AuditLog>, String> {
    let limit = limit.unwrap_or(MAX_AUDIT_LIMIT).min(MAX_AUDIT_LIMIT);
    db.list_audit_logs(limit)
        .map_err(|e| format!("读取操作审计失败: {e}"))
}

/// 清空全部操作审计记录，返回删除条数。
#[tauri::command]
pub fn clear_audit_logs(db: State<'_, Arc<Db>>) -> Result<usize, String> {
    db.clear_audit_logs()
        .map_err(|e| format!("清空操作审计失败: {e}"))
}
