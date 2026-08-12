mod alert;
mod ai;
mod agent;
mod audit;
mod credentials;
mod db;
mod hosts;
mod inspection;
mod mcp;
mod models;
mod monitor;
mod remote;
mod russh;
mod session;
mod sftp;
mod sshconfig;

use db::Db;
use agent::AgentManager;
use russh::RusshManager;
use session::SessionManager;
use std::io;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            std::fs::create_dir_all(&dir).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("无法创建数据目录: {e}"))
            })?;
            let db = Db::open(&dir.join("keywisp.db")).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("打开数据库失败: {e}"))
            })?;
            let _ = hosts::migrate_json(&db, app.handle());
            app.manage(db);
            alert::spawn_alert_loop(app.handle().clone());
            inspection::spawn_inspection_loop(app.handle().clone());
            Ok(())
        })
        .manage(SessionManager::default())
        .manage(RusshManager::new())
        .manage(AgentManager::default())
        .invoke_handler(tauri::generate_handler![
            ai::list_ai_providers,
            ai::save_ai_provider,
            ai::delete_ai_provider,
            ai::set_active_ai_model,
            ai::list_ai_rules,
            ai::add_ai_rule,
            ai::delete_ai_rule,
            ai::test_ai_provider,
            agent::agent_chat,
            agent::agent_approve,
            agent::agent_cancel,
            agent::agent_reset,
            audit::list_audit_logs,
            sftp::sftp_list,
            sftp::sftp_download,
            sftp::sftp_upload,
            sftp::sftp_delete,
            sftp::sftp_mkdir,
            sftp::sftp_rename,
            monitor::monitor_snapshot,
            alert::list_alerts,
            alert::save_alert,
            alert::delete_alert,
            inspection::list_inspections,
            inspection::save_inspection,
            inspection::delete_inspection,
            inspection::list_inspection_runs,
            inspection::inspection_respond,
            mcp::list_mcp_servers,
            mcp::save_mcp_server,
            mcp::delete_mcp_server,
            mcp::mcp_test,
            hosts::list_hosts,
            hosts::create_host,
            hosts::update_host,
            hosts::delete_host,
            hosts::import_ssh_config,
            hosts::save_host_credentials,
            hosts::test_host_connection,
            session::open_session,
            session::close_session,
            session::session_input,
            session::session_resize
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
