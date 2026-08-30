//! Tauri commands exposed to the frontend.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::{
    envinfo::EnvInfo,
    gitops,
    logs::LogPage,
    paths, pipeline, plugins,
    service::{self, AppState, BusyGuard},
    snapshot::{self, Snapshot},
    util,
};

/// Result of a sync/update run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    /// Whether HEAD moved (fresh clone or reset to a newer upstream commit).
    pub updated: bool,
    pub short_commit: Option<String>,
    pub behind: u32,
}

#[tauri::command]
pub async fn get_state(state: State<'_, Arc<AppState>>) -> Result<Snapshot, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || Ok(snapshot::build(&state)))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub fn get_logs(
    state: State<'_, Arc<AppState>>,
    after_id: Option<u64>,
    limit: Option<usize>,
) -> LogPage {
    state
        .logs
        .page_after(after_id, limit.unwrap_or(2_000).clamp(1, 2_000))
}

#[tauri::command]
pub fn clear_logs(state: State<'_, Arc<AppState>>) -> u64 {
    state.logs.clear()
}

#[tauri::command]
pub async fn sync_harness(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<SyncResult, String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = BusyGuard::new(&state, &app, "同步");
        let env = state.toolchain();
        if !env.ready {
            return Err(env.problems.join("；"));
        }
        sync_locked(&app, &state, &env)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn update_harness(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    restart: bool,
) -> Result<SyncResult, String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = state.toolchain();
        if !env.ready {
            return Err(env.problems.join("；"));
        }

        // Stop first so the rebuild never races a live service.
        let was_running = matches!(
            state.service.lock().expect("service lock").status.as_str(),
            "running" | "starting"
        );
        if was_running {
            service::stop_service(&app, &state)?;
        }

        let result = {
            let mut busy = BusyGuard::new(&state, &app, "更新");
            let sync = sync_locked(&app, &state, &env)?;

            let harness_dir = paths::harness_dir();
            let head = gitops::head_info(&harness_dir, &env).ok_or("同步后仍无法读取 git HEAD")?;
            if sync.updated || pipeline::needs_build(&harness_dir, &head) {
                busy.set_label("构建");
                pipeline::install(&harness_dir, &env, &app)
                    .and_then(|()| pipeline::build(&harness_dir, &env, &head, &app))
                    .map_err(|err| err.to_string())?;
            }
            sync
        };

        if restart && was_running {
            service::start_service(&app, &state, &env)?;
        }
        snapshot::publish(&app, &state);
        Ok(result)
    })
    .await
    .map_err(|err| err.to_string())?
}
/// Bring the harness tree to a runnable state: clone when missing, build when
/// artifacts are stale. Used by autostart before the first service spawn.
pub(crate) fn ensure_ready(
    app: &AppHandle,
    state: &Arc<AppState>,
    env: &EnvInfo,
) -> Result<(), String> {
    let harness_dir = paths::harness_dir();
    if !gitops::is_repo(&harness_dir) {
        let _busy = BusyGuard::new(state, app, "同步");
        sync_locked(app, state, env)?;
    }
    let head = gitops::head_info(&harness_dir, env).ok_or("同步后仍无法读取 git HEAD")?;
    if pipeline::needs_build(&harness_dir, &head) {
        let _busy = BusyGuard::new(state, app, "构建");
        pipeline::install(&harness_dir, env, app)
            .and_then(|()| pipeline::build(&harness_dir, env, &head, app))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// Shared sync core: clone when missing, fetch, reset when upstream moved.
/// Callers hold the busy label; this takes the io lock itself.
fn sync_locked(
    app: &AppHandle,
    state: &Arc<AppState>,
    env: &EnvInfo,
) -> Result<SyncResult, String> {
    let _io = state.io.lock().expect("io lock");
    let harness_dir = paths::harness_dir();

    let cloned = gitops::ensure_cloned(&harness_dir, app, env).map_err(|err| err.to_string())?;
    if cloned {
        *state.last_fetch_behind.lock().expect("behind lock") = Some(0);
        let head = gitops::head_info(&harness_dir, env);
        snapshot::publish(app, state);
        return Ok(SyncResult {
            updated: true,
            short_commit: head.map(|head| head.short_commit),
            behind: 0,
        });
    }

    gitops::fetch_latest(&harness_dir, app, env).map_err(|err| err.to_string())?;
    let behind = gitops::behind_count(&harness_dir, env).map_err(|err| err.to_string())?;
    *state.last_fetch_behind.lock().expect("behind lock") = Some(behind);

    let updated = behind > 0;
    if updated {
        gitops::reset_to_fetch_head(&harness_dir, app, env).map_err(|err| err.to_string())?;
    }
    let head = gitops::head_info(&harness_dir, env);
    snapshot::publish(app, state);
    Ok(SyncResult {
        updated,
        short_commit: head.map(|head| head.short_commit),
        behind,
    })
}

#[tauri::command]
pub async fn start_service(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = state.toolchain();
        service::start_service(&app, &state, &env)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn stop_service(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service::stop_service(&app, &state))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn set_config(app: AppHandle, port: u16) -> Result<(), String> {
    if !(1024..=65535).contains(&port) {
        return Err("端口需在 1024–65535 之间".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let mut config = paths::load_config().map_err(|err| err.to_string())?;
        config.port = port;
        paths::save_config(&config).map_err(|err| err.to_string())?;
        util::emit_log(&app, "desktop", &format!("端口已改为 {port}"));
        Ok(())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn refresh_toolchain(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _io = state.io.lock().expect("io lock");
        let config = paths::load_config().map_err(|err| err.to_string())?;
        let env = EnvInfo::discover(&config);
        let summary = if env.ready {
            "工具链重新检测完成".to_string()
        } else {
            format!("工具链重新检测完成：{}", env.problems.join("；"))
        };
        state.set_toolchain(env);
        snapshot::refresh_and_log(&app, &state, &summary);
        Ok(())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn set_toolchain_config(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    node_path: Option<String>,
    pnpm_path: Option<String>,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _io = state.io.lock().expect("io lock");
        if state.service.lock().expect("service lock").status != "stopped" {
            return Err("请先停止服务，再修改工具链设置".to_string());
        }
        let normalize = |value: Option<String>| {
            value.and_then(|value| {
                let trimmed = value.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
        };
        let mut config = paths::load_config().map_err(|err| err.to_string())?;
        config.node_path = normalize(node_path);
        config.pnpm_path = normalize(pnpm_path);
        let env = EnvInfo::discover(&config);
        env.validate_overrides()?;
        paths::save_config(&config).map_err(|err| err.to_string())?;
        state.set_toolchain(env);
        snapshot::refresh_and_log(&app, &state, "工具链设置已保存并重新检测");
        Ok(())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn get_plugins() -> Result<plugins::PluginCatalog, String> {
    tauri::async_runtime::spawn_blocking(plugins::list_plugins)
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn check_plugin_updates(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<plugins::PluginUpdate>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = state.toolchain();
        if !env.ready {
            return Err(env.problems.join("；"));
        }
        let harness_dir = paths::harness_dir();
        if !harness_dir.join("node_modules").is_dir() {
            return Err("请先更新代码并完成 Harness 构建".to_string());
        }
        Ok(plugins::check_updates(&harness_dir, &env))
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn manage_plugin(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    request: plugins::ManagePluginRequest,
) -> Result<plugins::ManagePluginResult, String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = state.toolchain();
        if !env.ready {
            return Err(env.problems.join("；"));
        }
        let harness_dir = paths::harness_dir();
        if !gitops::is_repo(&harness_dir) || !harness_dir.join("node_modules").is_dir() {
            return Err("请先更新代码并完成 Harness 构建".to_string());
        }
        plugins::validate_request(&request).map_err(|err| err.to_string())?;

        let was_running = matches!(
            state.service.lock().expect("service lock").status.as_str(),
            "running" | "starting"
        );
        if was_running {
            service::stop_service(&app, &state)?;
        }

        let operation = {
            let _busy = BusyGuard::new(&state, &app, request.action.busy_label());
            let _io = state.io.lock().expect("io lock");
            plugins::run_operation(&harness_dir, &env, &app, &request)
                .map_err(|err| err.to_string())
        };

        let restart = if was_running {
            service::start_service(&app, &state, &env)
        } else {
            Ok(())
        };
        snapshot::publish(&app, &state);

        match (operation, restart) {
            (Ok((catalog, message)), Ok(())) => Ok(plugins::ManagePluginResult {
                catalog,
                service_restarted: was_running,
                message,
            }),
            (Err(operation_err), Ok(())) => Err(operation_err),
            (Ok((_catalog, message)), Err(restart_err)) => {
                Err(format!("{message}，但 Harness 服务恢复失败：{restart_err}"))
            }
            (Err(operation_err), Err(restart_err)) => Err(format!(
                "插件操作失败：{operation_err}；Harness 服务恢复也失败：{restart_err}"
            )),
        }
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Open the harness workbench in a dedicated native window.
///
/// The workbench cannot live in the main window's iframe: the harness
/// authenticates its whole web surface with a `SameSite=Strict` cookie, and a
/// cross-site iframe (`tauri://` top-level over an `http://127.0.0.1` frame)
/// is never allowed to send it — every request 401s and the frame stays
/// blank. A dedicated window makes the harness origin the top-level document,
/// exactly like opening the URL printed by `dsh web` in a browser.
#[tauri::command]
pub fn open_workbench(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let url = service::workbench_url(&state)
        .ok_or_else(|| "尚未捕获工作台地址：请等服务启动完成后再打开".to_string())?;
    let parsed: tauri::Url = url.parse().map_err(|err| format!("工作台地址无效：{err}"))?;
    match app.get_webview_window("workbench") {
        Some(window) => {
            window.navigate(parsed).map_err(|err| format!("无法切换工作台地址：{err}"))?;
            window.set_focus().map_err(|err| err.to_string())?;
        }
        None => {
            tauri::WebviewWindowBuilder::new(&app, "workbench", tauri::WebviewUrl::External(parsed))
                .title("DSH 工作台")
                .inner_size(1280.0, 820.0)
                .min_inner_size(980.0, 640.0)
                .build()
                .map_err(|err| format!("无法打开工作台窗口：{err}"))?;
        }
    }
    Ok(())
}
