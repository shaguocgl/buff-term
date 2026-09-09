mod alert;
mod ai;
mod agent;
mod audit;
mod credentials;
mod db;
mod guard;
mod hosts;
mod inspection;
mod mcp;
mod models;
mod monitor;
mod remediation;
mod russh;
mod safety;
mod session;
mod sftp;
mod sshconfig;
mod update;
mod util;

use std::sync::Arc;
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
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            std::fs::create_dir_all(&dir).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("无法创建数据目录: {e}"))
            })?;
            let db = Db::open(&dir.join("buffterm.db")).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("打开数据库失败: {e}"))
            })?;
            let db = std::sync::Arc::new(db);
            crate::db::init_global(db.clone());
            crate::russh::init_app_handle(app.handle());
            // 迁移旧版本的明文敏感凭据（MCP token / SMTP 密码）到加密存储
            crate::credentials::migrate_plaintext_secrets();
            // 清理 90 天前的历史指标数据，控制 SQLite 体积
            let cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_sub(90 * 24 * 3600);
            let _ = db.prune_metrics(cutoff);
            // 清理 365 天前的审计日志
            let audit_cutoff = cutoff.saturating_sub(275 * 24 * 3600);
            let _ = db.prune_audit_logs(audit_cutoff);
            // 上次异常退出遗留的「运行中」巡检/整改标记为已取消
            let _ = db.mark_stale_running_cancelled();
            app.manage(db);
            app.manage(inspection::InspectionManager::default());
            app.manage(remediation::RemediationManager::default());
            app.manage(mcp::McpServiceManager::default());
            app.manage(mcp::ApprovalRegistry::default());
            app.manage(sftp::SftpTransferRegistry::default());
            // 若上次退出前开启了 MCP 服务，启动时自动恢复
            let mcp_enabled = app
                .state::<Arc<Db>>()
                .get_mcp_service()
                .map(|c| c.enabled)
                .unwrap_or(false);
            if mcp_enabled {
                match mcp::start_service(app.handle(), &app.state::<mcp::McpServiceManager>()) {
                    Ok(port) => {
                        // 默认端口被占时可能绑定随机端口，把实际端口写回数据库，
                        // 否则外部 MCP 客户端按旧端口连接会失败
                        match app.state::<Arc<Db>>().get_mcp_service() {
                            Ok(mut config) => {
                                if config.port != Some(port) {
                                    config.port = Some(port);
                                    let _ = app.state::<Arc<Db>>().save_mcp_service(&config);
                                }
                            }
                            Err(e) => eprintln!("[mcp] 读取 MCP 配置失败: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[mcp] 启动 MCP 服务失败: {e}"),
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
            ai::get_ai_default_context_window,
            ai::save_ai_default_context_window,
            ai::set_active_ai_model,
            ai::set_active_ai_provider,
            ai::list_ai_rules,
            ai::add_ai_rule,
            ai::delete_ai_rule,
            ai::test_ai_provider,
            ai::list_remote_ai_models,
            agent::agent_chat,
            agent::agent_approve,
            agent::agent_cancel,
            agent::agent_reset,
            agent::get_history,
            agent::get_task_plan,
            agent::get_context_usage,
            audit::list_audit_logs,
            sftp::sftp_list,
            sftp::sftp_download,
            sftp::sftp_upload,
            sftp::sftp_delete,
            sftp::sftp_mkdir,
            sftp::sftp_rename,
            sftp::sftp_exists,
            sftp::sftp_cancel_transfer,
            monitor::monitor_snapshot,
            monitor::monitor_history,
            inspection::start_inspection,
            inspection::get_inspection_report,
            inspection::list_inspection_reports,
            inspection::delete_inspection_report,
            inspection::cancel_inspection,
            remediation::start_remediation_planning,
            remediation::get_remediation,
            remediation::execute_remediation,
            remediation::cancel_remediation,
            remediation::retry_remediation,
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
            session::session_resize,
            guard::get_terminal_guard_settings,
            guard::save_terminal_guard_settings,
            guard::list_terminal_rules,
            guard::add_terminal_rule,
            guard::delete_terminal_rule,
            guard::reset_terminal_rules,
            guard::session_guard_approve,
            russh::ssh_confirm_host_key,
            update::check_for_update,
            update::get_app_version
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 退出时关闭 MCP 服务（停止监听端口），SSH 连接随进程退出自然释放
            if let tauri::RunEvent::Exit = event {
                let manager = app.state::<mcp::McpServiceManager>();
                mcp::stop_service(&manager);
            }
        });
}
