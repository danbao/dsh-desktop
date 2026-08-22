//! Tauri commands exposed to the frontend.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::{
    envinfo::EnvInfo,
    gitops,
    paths,
    pipeline,
    service::{self, AppState},
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

fn busy_guard<'a>(state: &'a AppState, label: &'static str) -> BusyGuard<'a> {
    state.set_busy(Some(label));
    BusyGuard(state)
}

struct BusyGuard<'a>(&'a AppState);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.set_busy(None);
    }
}

#[tauri::command]
pub async fn get_state(state: State<'_, Arc<AppState>>) -> Result<Snapshot, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || Ok(snapshot::build(&state)))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn sync_harness(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<SyncResult, String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _busy = busy_guard(&state, "同步");
        sync_locked(&app, &state)
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
        let env = EnvInfo::probe();
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
            let _busy = busy_guard(&state, "更新");
            let sync = sync_locked(&app, &state)?;

            let harness_dir = paths::harness_dir();
            let head = gitops::head_info(&harness_dir)
                .ok_or("同步后仍无法读取 git HEAD")?;
            if sync.updated || pipeline::needs_build(&harness_dir, &head) {
                state.set_busy(Some("构建"));
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
        let _busy = busy_guard(state, "同步");
        sync_locked(app, state)?;
    }
    let head =
        gitops::head_info(&harness_dir).ok_or("同步后仍无法读取 git HEAD")?;
    if pipeline::needs_build(&harness_dir, &head) {
        let _busy = busy_guard(state, "构建");
        pipeline::install(&harness_dir, env, app)
            .and_then(|()| pipeline::build(&harness_dir, env, &head, app))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}


/// Shared sync core: clone when missing, fetch, reset when upstream moved.
/// Callers hold the busy label; this takes the io lock itself.
fn sync_locked(app: &AppHandle, state: &Arc<AppState>) -> Result<SyncResult, String> {
    let _io = state.io.lock().expect("io lock");
    let harness_dir = paths::harness_dir();

    let cloned = gitops::ensure_cloned(&harness_dir, app).map_err(|err| err.to_string())?;
    if cloned {
        *state.last_fetch_behind.lock().expect("behind lock") = Some(0);
        let head = gitops::head_info(&harness_dir);
        snapshot::publish(app, state);
        return Ok(SyncResult {
            updated: true,
            short_commit: head.map(|head| head.short_commit),
            behind: 0,
        });
    }

    gitops::fetch_latest(&harness_dir, app).map_err(|err| err.to_string())?;
    let behind = gitops::behind_count(&harness_dir).map_err(|err| err.to_string())?;
    *state.last_fetch_behind.lock().expect("behind lock") = Some(behind);

    let updated = behind > 0;
    if updated {
        gitops::reset_to_fetch_head(&harness_dir, app).map_err(|err| err.to_string())?;
    }
    let head = gitops::head_info(&harness_dir);
    snapshot::publish(app, state);
    Ok(SyncResult {
        updated,
        short_commit: head.map(|head| head.short_commit),
        behind,
    })
}

#[tauri::command]
pub async fn start_service(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let app = app.clone();
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let env = EnvInfo::probe();
        service::start_service(&app, &state, &env)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn stop_service(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
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
