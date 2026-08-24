//! dsh-desktop library entry: wires Tauri state, commands, and exit cleanup.

pub mod commands;
pub mod envinfo;
pub mod gitops;
pub mod logs;
pub mod paths;
pub mod pipeline;
pub mod plugins;
pub mod service;
pub mod snapshot;
pub mod toolchain;
pub mod util;

use std::sync::Arc;

use tauri::{webview::NewWindowResponse, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::service::AppState;

pub fn run() {
    service::install_signal_cleanup();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Arc::new(AppState::new()))
        .setup(|app| {
            let handle = app.handle().clone();
            let opener = handle.clone();
            let main_config = app
                .config()
                .app
                .windows
                .iter()
                .find(|config| config.label == "main")
                .expect("main window config");
            tauri::WebviewWindowBuilder::from_config(app.handle(), main_config)?
                .on_new_window(move |url, _features| {
                    if is_browser_url(&url) {
                        if let Err(err) = opener.opener().open_url(url.as_str(), None::<&str>) {
                            util::emit_log(
                                &opener,
                                "desktop",
                                &format!("无法用默认浏览器打开 {url}：{err}"),
                            );
                        }
                    } else {
                        util::emit_log(
                            &opener,
                            "desktop",
                            &format!("已阻止不支持的外链协议：{url}"),
                        );
                    }
                    NewWindowResponse::Deny
                })
                .build()?;
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
            commands::get_plugins,
            commands::check_plugin_updates,
            commands::manage_plugin,
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

fn is_browser_url(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
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

#[cfg(test)]
mod tests {
    use super::is_browser_url;

    #[test]
    fn allows_only_http_and_https_external_urls() {
        for allowed in [
            "http://example.com/path",
            "https://example.com/path?q=one%20two#result",
        ] {
            assert!(is_browser_url(&allowed.parse().unwrap()), "{allowed}");
        }

        for rejected in [
            "file:///tmp/example.txt",
            "data:text/plain,hello",
            "javascript:alert(1)",
            "tauri://localhost/index.html",
            "mailto:user@example.com",
        ] {
            assert!(!is_browser_url(&rejected.parse().unwrap()), "{rejected}");
        }
    }
}
