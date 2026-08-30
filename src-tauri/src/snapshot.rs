//! Assemble the full UI snapshot and broadcast it as `state-changed`.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::{envinfo::EnvInfo, gitops, paths, pipeline, service::AppState, util};

/// Facts about the managed harness checkout.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HarnessView {
    pub path: String,
    pub present: bool,
    pub commit: Option<String>,
    pub short_commit: Option<String>,
    pub subject: Option<String>,
    pub commit_date: Option<String>,
    /// Commits behind upstream from the last fetch; `null` before one runs.
    pub behind: Option<u32>,
    pub build_needed: bool,
}

/// Service facts plus the derived loopback URL the UI embeds.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ServiceView {
    pub status: String,
    pub port: u16,
    pub error: Option<String>,
    pub url: String,
}

/// Full UI state, produced by `get_state` and every transition.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub harness: HarnessView,
    pub env: EnvInfo,
    pub service: ServiceView,
    pub busy: Option<&'static str>,
}

/// Gather a fresh snapshot from disk, git, and shared state.
pub fn build(state: &AppState) -> Snapshot {
    let env = state.toolchain();
    let harness_dir = paths::harness_dir();
    let head = gitops::head_info(&harness_dir, &env);
    let build_needed = head
        .as_ref()
        .map(|head| pipeline::needs_build(&harness_dir, head))
        .unwrap_or(false);
    let behind = *state.last_fetch_behind.lock().expect("behind lock");

    let service_now = state.service.lock().expect("service lock").clone();
    // Prefer the tokenized URL the harness announced on stdout: newer builds
    // answer `401` on the bare port, so the embedded webview needs the token.
    let url = if let Some(captured) = service_now.url.as_deref().filter(|u| !u.is_empty()) {
        captured.to_string()
    } else if service_now.port > 0 {
        format!("http://127.0.0.1:{}", service_now.port)
    } else {
        match paths::load_config() {
            Ok(config) => format!("http://127.0.0.1:{}", config.port),
            Err(_) => String::new(),
        }
    };

    Snapshot {
        harness: HarnessView {
            path: harness_dir.to_string_lossy().into_owned(),
            present: head.is_some(),
            commit: head.as_ref().map(|head| head.commit.clone()),
            short_commit: head.as_ref().map(|head| head.short_commit.clone()),
            subject: head.as_ref().map(|head| head.subject.clone()),
            commit_date: head.as_ref().map(|head| head.commit_date.clone()),
            behind,
            build_needed,
        },
        env,
        service: ServiceView {
            status: service_now.status,
            port: service_now.port,
            error: service_now.error,
            url,
        },
        busy: *state.busy.lock().expect("busy lock"),
    }
}

/// Emit the current snapshot to every window.
pub fn publish(app: &AppHandle, state: &AppState) {
    let _ = app.emit("state-changed", build(state));
}

/// Convenience for command bodies that just finished mutating state.
pub fn refresh_and_log(app: &AppHandle, state: &AppState, message: &str) {
    util::emit_log(app, "desktop", message);
    publish(app, state);
}
