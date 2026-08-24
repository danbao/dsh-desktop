//! dsh-desktop library entry: wires Tauri state, commands, and exit cleanup.

pub mod commands;
pub mod envinfo;
pub mod gitops;
pub mod logs;
pub mod paths;
pub mod pipeline;
pub mod service;
pub mod snapshot;
pub mod toolchain;
pub mod util;

use std::sync::Arc;

use tauri::Manager;

use crate::service::AppState;

pub fn run() {
    service::install_signal_cleanup();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(AppState::new()))
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || autostart(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_logs,
            commands::clear_logs,
            commands::sync_harness,
            commands::update_harness,
            commands::start_service,
            commands::stop_service,
            commands::set_config,
            commands::refresh_toolchain,
            commands::set_toolchain_config,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                let state = app_handle.state::<Arc<AppState>>();
                service::kill_on_exit(&state);
            }
        });
}

/// Launch the service right away unless the user disabled `autostart`.
fn autostart(app: tauri::AppHandle) {
    let config = match paths::load_config() {
        Ok(config) => config,
        Err(err) => {
            util::emit_log(
                &app,
                "desktop",
                &format!("配置读取失败，跳过自动启动：{err}"),
            );
            return;
        }
    };
    if !config.autostart {
        return;
    }
    let state = app.state::<Arc<AppState>>();
    let env = state.toolchain();
    if let Err(err) = commands::ensure_ready(&app, &state, &env) {
        util::emit_log(&app, "desktop", &format!("内核准备失败：{err}"));
        return;
    }
    if let Err(err) = service::start_service(&app, &state, &env) {
        util::emit_log(&app, "desktop", &format!("自动启动失败：{err}"));
    }
}
