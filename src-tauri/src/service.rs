//! Service lifecycle for the harness web server child process.
//!
//! The supervisor thread owns the `Child`; the shared state carries only the
//! process-group id (`process_group(0)` makes it equal to the spawned pid),
//! a stop flag, and the supervisor handle so commands can signal and wait
//! without racing on the child handle.

use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use std::sync::atomic::AtomicU32;

use crate::{
    envinfo::EnvInfo, gitops, logs::LogBuffer, paths, pipeline, snapshot, toolchain::Tool, util,
};

/// How long the service may take to answer its first healthy probe.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(240);
/// Process group of the live service, for signal handlers that cannot lock.
static ACTIVE_PGID: AtomicU32 = AtomicU32::new(0);

/// Install SIGTERM/SIGINT handlers that kill the service process group before
/// dying. Tauri runs no Rust cleanup on signals, so without this a terminated
/// app would leak the spawned `dsh` tree. The handler only touches atomics and
/// libc, as async-signal-safety requires.
pub fn install_signal_cleanup() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_term_signal as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_term_signal as libc::sighandler_t);
    }
}

extern "C" fn handle_term_signal(sig: i32) {
    let pgid = ACTIVE_PGID.load(Ordering::SeqCst);
    if pgid != 0 {
        util::kill_group(pgid, libc::SIGKILL);
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}
/// Everything the UI needs to know about the service.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceState {
    /// `stopped` | `starting` | `running` | `error`
    pub status: String,
    pub port: u16,
    pub error: Option<String>,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            status: "stopped".to_string(),
            port: 0,
            error: None,
        }
    }
}

/// Shared application state managed by Tauri.
pub struct AppState {
    /// Serializes mutating operations (sync / update / start / stop).
    pub io: Mutex<()>,
    /// Current long-running operation label for the UI (`同步` / `构建` / …).
    pub busy: Mutex<Option<&'static str>>,
    pub service: Mutex<ServiceState>,
    pub last_fetch_behind: Mutex<Option<u32>>,
    pub child: Mutex<Option<RunningService>>,
    /// Complete, non-persistent history for the current application session.
    pub logs: LogBuffer,
    /// One terminal-equivalent environment shared by every child process.
    pub toolchain: RwLock<EnvInfo>,
}

/// A spawned service plus everything needed to stop it from anywhere.
pub struct RunningService {
    pub pgid: u32,
    pub stop_flag: Arc<AtomicBool>,
    pub supervisor: Option<std::thread::JoinHandle<()>>,
}

/// Publishes every busy-state transition and guarantees cleanup on all exits.
pub(crate) struct BusyGuard<'a> {
    state: &'a AppState,
    app: &'a AppHandle,
}

impl<'a> BusyGuard<'a> {
    pub(crate) fn new(state: &'a AppState, app: &'a AppHandle, label: &'static str) -> Self {
        state.set_busy(Some(label));
        state.changed(app);
        Self { state, app }
    }

    pub(crate) fn set_label(&mut self, label: &'static str) {
        self.state.set_busy(Some(label));
        self.state.changed(self.app);
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.state.set_busy(None);
        self.state.changed(self.app);
    }
}

impl AppState {
    pub fn new() -> Self {
        let config = paths::load_config().unwrap_or_default();
        Self {
            io: Mutex::new(()),
            busy: Mutex::new(None),
            service: Mutex::new(ServiceState::default()),
            last_fetch_behind: Mutex::new(None),
            child: Mutex::new(None),
            logs: LogBuffer::default(),
            toolchain: RwLock::new(EnvInfo::discover(&config)),
        }
    }

    pub fn toolchain(&self) -> EnvInfo {
        self.toolchain.read().expect("toolchain lock").clone()
    }

    pub fn set_toolchain(&self, env: EnvInfo) {
        *self.toolchain.write().expect("toolchain lock") = env;
    }

    pub(crate) fn set_service(&self, next: ServiceState) {
        *self.service.lock().expect("service lock") = next;
    }

    pub(crate) fn set_busy(&self, label: Option<&'static str>) {
        *self.busy.lock().expect("busy lock") = label;
    }

    fn current_port(&self) -> u16 {
        self.service.lock().expect("service lock").port
    }

    /// Publish the current snapshot so the UI reflects a transition.
    fn changed(&self, app: &AppHandle) {
        snapshot::publish(app, self);
    }
}

/// Spawn `dsh --profile web` against the harness tree and supervise it until
/// healthy, stopped, or failed. Rebuilds first when artifacts are stale.
pub fn start_service(app: &AppHandle, state: &AppState, env: &EnvInfo) -> Result<(), String> {
    let _io = state.io.lock().expect("io lock");
    // A finished-but-unreaped previous run leaves a stale slot behind.
    let _ = state.child.lock().expect("child lock").take();
    if matches!(
        state.service.lock().expect("service lock").status.as_str(),
        "running" | "starting"
    ) {
        return Ok(());
    }
    if !env.ready {
        return Err(env.problems.join("；"));
    }

    let config = paths::load_config().map_err(|err| err.to_string())?;
    let port = config.port;
    let harness_dir = paths::harness_dir();
    let Some(head) = gitops::head_info(&harness_dir, env) else {
        return Err("尚未获取 deepseek-harness 代码，请先执行「更新代码并构建」".to_string());
    };

    // Stale artifacts are rebuilt right here so 启动 is always sufficient.
    if pipeline::needs_build(&harness_dir, &head) {
        let result = {
            let _busy = BusyGuard::new(state, app, "构建");
            state.set_service(ServiceState {
                status: "starting".into(),
                port,
                error: None,
            });
            state.changed(app);
            util::emit_log(app, "build", "构建产物缺失或过期，自动执行 install/build");
            pipeline::install(&harness_dir, env, app)
                .and_then(|()| pipeline::build(&harness_dir, env, &head, app))
        };
        result.map_err(|err| err.to_string())?;
    }

    if !util::port_free(port) {
        return Err(format!(
            "端口 {port} 已被占用：请在控制台修改端口，或释放该端口后重试"
        ));
    }

    let mut cmd = env.command(Tool::Node).map_err(|err| err.to_string())?;
    cmd.args(["--import", "tsx/esm", "apps/cli/src/bin.ts"])
        .arg("--profile")
        .arg("web")
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--no-open",
        ])
        .current_dir(&harness_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.process_group(0);

    util::emit_log(
        app,
        "dsh",
        &format!("启动 dsh --profile web（端口 {port}）"),
    );
    let mut child = cmd.spawn().map_err(|err| format!("无法启动 node：{err}"))?;
    let pgid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    ACTIVE_PGID.store(pgid, Ordering::SeqCst);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let flag_for_thread = Arc::clone(&stop_flag);
    let supervisor_app = app.clone();
    let supervisor = std::thread::spawn(move || {
        supervise(
            supervisor_app,
            child,
            stdout,
            stderr,
            pgid,
            port,
            flag_for_thread,
        );
    });

    *state.child.lock().expect("child lock") = Some(RunningService {
        pgid,
        stop_flag,
        supervisor: Some(supervisor),
    });

    state.set_service(ServiceState {
        status: "starting".into(),
        port,
        error: None,
    });
    state.changed(app);

    // Wait for readiness or failure here; transitions stream out via events.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        {
            let current = state.service.lock().expect("service lock");
            match current.status.as_str() {
                "running" => return Ok(()),
                "error" | "stopped" => {
                    return Err(current
                        .error
                        .clone()
                        .unwrap_or_else(|| "服务启动失败".to_string()));
                }
                _ => {}
            }
        }
        if Instant::now() > deadline {
            return Err("等待服务就绪超时".to_string());
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Stop the service if one is running; idempotent.
pub fn stop_service(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let _io = state.io.lock().expect("io lock");
    let Some(mut running) = state.child.lock().expect("child lock").take() else {
        state.set_service(ServiceState::default());
        state.changed(app);
        return Ok(());
    };

    let pgid = running.pgid;
    ACTIVE_PGID.store(0, Ordering::SeqCst);
    util::kill_group(pgid, libc::SIGTERM);
    util::emit_log(app, "dsh", &format!("发送 SIGTERM（进程组 {pgid}）"));

    let deadline = Instant::now() + Duration::from_secs(5);
    let graceful = match running.supervisor.take() {
        Some(handle) => wait_join(handle, deadline),
        None => true,
    };
    if !graceful {
        util::kill_group(pgid, libc::SIGKILL);
        util::emit_log(app, "dsh", "优雅退出超时，已发送 SIGKILL");
    }

    state.set_service(ServiceState::default());
    state.changed(app);
    Ok(())
}

/// Best-effort kill during app exit: no waiting, no event emission.
pub fn kill_on_exit(state: &AppState) {
    let running = state.child.lock().expect("child lock").take();
    if let Some(running) = running {
        running.stop_flag.store(true, Ordering::SeqCst);
        ACTIVE_PGID.store(0, Ordering::SeqCst);
        util::kill_group(running.pgid, libc::SIGKILL);
    }
}
/// JoinHandle has no timeout; convert join into a bounded wait via watcher.
fn wait_join(handle: std::thread::JoinHandle<()>, deadline: Instant) -> bool {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    let timeout = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(timeout.max(Duration::from_millis(1)))
        .is_ok()
}

/// Owns the child for its whole life: streams output, probes health, reacts
/// to stop requests, and detects unexpected death or sustained failure.
fn supervise(
    app: AppHandle,
    mut child: Child,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    pgid: u32,
    port: u16,
    stop_flag: Arc<AtomicBool>,
) {
    let readers = spawn_dsh_readers(stdout, stderr, &app);

    // Startup phase: probe until healthy, or fail fast on early exit.
    let started = Instant::now();
    let mut failure: Option<String> = None;
    let mut stopped_by_user = false;
    loop {
        if stop_flag.load(Ordering::SeqCst) {
            graceful_kill(&mut child, pgid);
            stopped_by_user = true;
            break;
        }
        if util::http_ok(port) {
            break;
        }
        if let Some(status) = child.try_wait().ok().flatten() {
            failure = Some(format!(
                "dsh 进程提前退出（退出码 {}）",
                status.code().unwrap_or(-1)
            ));
            break;
        }
        if started.elapsed() > STARTUP_TIMEOUT {
            graceful_kill(&mut child, pgid);
            failure = Some("服务在 240 秒内未就绪".to_string());
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if stopped_by_user {
        reap_silently(child, readers);
        return; // the stopping command owns the final state
    }
    if let Some(message) = failure {
        reap_and_report(&app, child, readers, message);
        return;
    }

    report_state(
        &app,
        ServiceState {
            status: "running".into(),
            port,
            error: None,
        },
    );

    // Steady-state watch: unexpected death or sustained probe failure.
    let mut misses = 0u32;
    loop {
        if stop_flag.load(Ordering::SeqCst) {
            graceful_kill(&mut child, pgid);
            reap_silently(child, readers);
            return; // the stopping command owns the final state
        }
        std::thread::sleep(Duration::from_secs(2));
        if child.try_wait().ok().flatten().is_some() {
            reap_and_report(&app, child, readers, "dsh 进程意外退出".to_string());
            return;
        }
        if util::http_ok(port) {
            misses = 0;
            continue;
        }
        misses += 1;
        if misses >= 6 {
            graceful_kill(&mut child, pgid);
            reap_and_report(
                &app,
                child,
                readers,
                "健康检查连续失败，服务已停止".to_string(),
            );
            return;
        }
    }
}

fn spawn_dsh_readers(
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    app: &AppHandle,
) -> [std::thread::JoinHandle<()>; 2] {
    let app_out = app.clone();
    let t_out = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        util::pump_lines(&mut reader, &app_out, "dsh");
    });
    let app_err = app.clone();
    let t_err = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stderr);
        util::pump_lines(&mut reader, &app_err, "dsh");
    });
    [t_out, t_err]
}

fn graceful_kill(child: &mut Child, pgid: u32) {
    util::kill_group(pgid, libc::SIGTERM);
    for _ in 0..30 {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    util::kill_group(pgid, libc::SIGKILL);
    let _ = child.wait();
}

fn report_state(app: &AppHandle, next: ServiceState) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    state.set_service(next);
    state.changed(app);
}

fn reap_and_report(
    app: &AppHandle,
    mut child: Child,
    readers: [std::thread::JoinHandle<()>; 2],
    message: String,
) {
    let _ = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
    util::emit_log(app, "dsh", &format!("服务错误：{message}"));
    let port = app
        .try_state::<Arc<AppState>>()
        .map(|state| state.current_port())
        .unwrap_or(0);
    report_state(
        app,
        ServiceState {
            status: "error".into(),
            port,
            error: Some(message),
        },
    );
}

fn reap_silently(mut child: Child, readers: [std::thread::JoinHandle<()>; 2]) {
    let _ = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
}
