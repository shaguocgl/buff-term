//! 系统托盘：客户端值守启用时关闭窗口改为隐藏到托盘，让值守持续运行。
//! 未启用值守时关闭窗口照常退出，应用行为与之前完全一致。

use crate::db::Db;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

const TRAY_ID: &str = "buffterm-main";

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn guard_running(app: &AppHandle) -> bool {
    app.state::<Arc<Db>>()
        .get_guard_settings()
        .map(|s| s.enabled && s.hosts.iter().any(|h| h.mode == "client"))
        .unwrap_or(false)
}

fn tooltip(app: &AppHandle) -> String {
    if guard_running(app) {
        format!(
            "buffTerm · {}",
            crate::i18n::t("智能值守运行中", "smart guard running")
        )
    } else {
        "buffTerm".to_string()
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let show = MenuItem::with_id(
        app,
        "show",
        crate::i18n::t("显示主窗口", "Show main window"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        crate::i18n::t("退出 buffTerm", "Quit buffTerm"),
        true,
        None::<&str>,
    )?;
    Menu::with_items(app, &[&show, &quit])
}

/// 创建托盘图标。失败只记日志：托盘不可用不应阻止应用启动。
pub fn init(app: &AppHandle) {
    let menu = match build_menu(app) {
        Ok(menu) => menu,
        Err(e) => {
            log::error!("[tray] build menu failed: {e}");
            return;
        }
    };
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip(tooltip(app))
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            // 走 app.exit 而不是关窗：值守启用时关闭事件会被拦截成隐藏
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    if let Err(e) = builder.build(app) {
        log::error!("[tray] create tray icon failed: {e}");
    }
}

/// 语言切换或值守开关变化后同步托盘文案与提示。
pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(menu) = build_menu(app) {
        let _ = tray.set_menu(Some(menu));
    }
    let _ = tray.set_tooltip(Some(tooltip(app)));
}