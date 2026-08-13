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
            app.manage(db);
            app.manage(inspection::InspectionManager::default());
            app.manage(mcp::McpServiceManager::default());
            app.manage(mcp::ApprovalRegistry::default());
            // 若上次退出前开启了 MCP 服务，启动时自动恢复
            let mcp_enabled = app
                .state::<Db>()
                .get_mcp_service()
                .map(|c| c.enabled)
                .unwrap_or(false);
            if mcp_enabled {
                if let Err(e) =
                    mcp::start_service(app.handle(), &app.state::<mcp::McpServiceManager>())
                {
                    eprintln!("[mcp] 启动 MCP 服务失败: {e}");
                }
            }
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
            ai::set_active_ai_provider,
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
            inspection::start_inspection,
            inspection::get_inspection_report,
            inspection::list_inspection_reports,
            inspection::delete_inspection_report,
            inspection::cancel_inspection,
            alert::get_alert_settings,
            alert::save_alert_settings,
            alert::test_alert_settings,
            mcp::get_mcp_service,
            mcp::save_mcp_service,
            mcp::rotate_mcp_token,
            mcp::mcp_approve,
            mcp::list_mcp_rules,
            mcp::add_mcp_rule,
            mcp::delete_mcp_rule,
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
