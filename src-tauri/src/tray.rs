//! Menu-bar (tray) mode: the app keeps running with the window closed, and
//! the common operations work straight from the macOS status item.

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::service::{self, AppState};

/// Stable tray id so later refreshes can find the icon again.
const TRAY_ID: &str = "dsh-tray";

mod ids {
    pub const STATUS: &str = "status";
    pub const SHOW: &str = "show";
    pub const TOGGLE_SERVICE: &str = "toggle-service";
    pub const OPEN_WORKBENCH: &str = "open-workbench";
    pub const CHECK_UPDATE: &str = "check-update";
    pub const QUIT: &str = "quit";
}

/// Build the tray icon and install its menu. Called once from app setup.
pub fn setup(app: &AppHandle) -> anyhow::Result<()> {
    let menu = build_menu(app)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("DSH Desktop")
        .menu(&menu)
        .show_menu_on_left_click(true);
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder
        .on_menu_event(handle_menu_event)
        .build(app)
        .map_err(|err| anyhow::anyhow!("无法创建菜单栏图标：{err}"))?;
    Ok(())
}

fn build_menu<R: Runtime>(app: &tauri::AppHandle<R>) -> anyhow::Result<Menu<R>> {
    let state = app.state::<Arc<AppState>>();
    let service_state = state.service.lock().expect("service lock").clone();

    let status = MenuItem::with_id(
        app,
        ids::STATUS,
        status_text(&service_state.status, service_state.port),
        false, // information only; also keeps the row from being clickable
        None::<&str>,
    )?;
    let show = MenuItem::with_id(app, ids::SHOW, "显示主窗口", true, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        ids::TOGGLE_SERVICE,
        toggle_text(&service_state.status),
        true,
        None::<&str>,
    )?;
    let workbench = MenuItem::with_id(
        app,
        ids::OPEN_WORKBENCH,
        "打开工作台（浏览器）",
        matches!(service_state.status.as_str(), "running"),
        None::<&str>,
    )?;
    let update = MenuItem::with_id(app, ids::CHECK_UPDATE, "检查应用更新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ids::QUIT, "退出 DSH Desktop", true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &status,
            &show,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &workbench,
            &update,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
    .map_err(|err| anyhow::anyhow!("无法构建菜单栏菜单：{err}"))
}

/// Rebuild the tray menu so the status row and the start/stop label follow
/// service transitions. Called on every state publish.
pub fn refresh(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(err) => crate::util::emit_log(app, "desktop", &format!("刷新菜单栏失败：{err}")),
    }
}

fn status_text(status: &str, port: u16) -> String {
    match status {
        "running" => format!("服务运行中 · 127.0.0.1:{port}"),
        "starting" => "服务启动中…".to_string(),
        "error" => "服务出错，见主窗口日志".to_string(),
        _ => "服务已停止".to_string(),
    }
}

fn toggle_text(status: &str) -> &'static str {
    match status {
        "running" | "starting" => "停止服务",
        _ => "启动服务",
    }
}

fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        ids::SHOW => show_main_window(app),
        ids::TOGGLE_SERVICE => toggle_service(app),
        ids::OPEN_WORKBENCH => {
            let state = app.state::<Arc<AppState>>().inner().clone();
            if let Err(err) = open_workbench(app, &state) {
                crate::util::emit_log(app, "desktop", &err);
            }
        }
        ids::CHECK_UPDATE => {
            // The updater runs in the webview (download progress, relaunch);
            // it stays alive while the window is hidden, so an event is all
            // the plumbing the tray needs.
            let _ = app.emit("tray-check-update", ());
        }
        ids::QUIT => quit(app),
        _ => {}
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn toggle_service(app: &AppHandle) {
    let app = app.clone();
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let starting = matches!(
            state.service.lock().expect("service lock").status.as_str(),
            "stopped" | "error"
        );
        let result = if starting {
            let env = state.toolchain();
            service::start_service(&app, &state, &env)
        } else {
            service::stop_service(&app, &state)
        };
        if let Err(err) = result {
            crate::util::emit_log(&app, "desktop", &format!("菜单栏操作失败：{err}"));
        }
    });
}

/// Same body as the `open_workbench` command, callable without a `State`
/// guard: open the captured tokenized URL in the default browser.
fn open_workbench(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let url = service::workbench_url(state)
        .ok_or_else(|| "尚未捕获工作台地址：请等服务启动完成后再打开".to_string())?;
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|err| format!("无法用默认浏览器打开工作台：{err}"))
}

fn quit(app: &AppHandle) {
    let app = app.clone();
    let state = app.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Graceful stop beats the exit hook's SIGKILL: the harness gets its
        // SIGTERM window before the whole process group dies.
        let _ = service::stop_service(&app, &state);
        app.exit(0);
    });
}
