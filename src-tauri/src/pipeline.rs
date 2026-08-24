//! Build pipeline: decide whether artifacts are stale, run install/build,
//! and stamp the covered commit.

use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use tauri::AppHandle;

use crate::{envinfo::EnvInfo, gitops, paths, toolchain::Tool, util};

/// Whether the checkout lacks dependencies, web assets, or a build stamp
/// covering the current HEAD.
pub fn needs_build(harness_dir: &Path, head: &gitops::HeadInfo) -> bool {
    if !harness_dir.join("node_modules").is_dir() {
        return true;
    }
    if !harness_dir.join("apps/web/dist/index.html").is_file() {
        return true;
    }
    paths::stamp_commit(harness_dir).as_deref() != Some(head.commit.as_str())
}

/// `pnpm install` in the harness tree.
pub fn install(harness_dir: &Path, env: &EnvInfo, app: &AppHandle) -> anyhow::Result<()> {
    let mut cmd = env.command(Tool::Pnpm)?;
    cmd.arg("install").current_dir(harness_dir);
    let started = Instant::now();
    util::emit_log(app, "build", "开始安装依赖");
    let result = util::stream_command(&mut cmd, app, "pnpm");
    let elapsed = util::format_elapsed(started.elapsed());
    match result {
        Ok(()) => {
            util::emit_log(app, "build", &format!("依赖安装完成（用时 {elapsed}）"));
            Ok(())
        }
        Err(err) => {
            util::emit_log(app, "build", &format!("依赖安装失败（用时 {elapsed}）"));
            Err(err)
        }
    }
}

/// `pnpm run build`, then record the commit the artifacts now cover.
pub fn build(
    harness_dir: &Path,
    env: &EnvInfo,
    head: &gitops::HeadInfo,
    app: &AppHandle,
) -> anyhow::Result<()> {
    let mut cmd = env.command(Tool::Pnpm)?;
    cmd.arg("run").arg("build").current_dir(harness_dir);
    let started = Instant::now();
    util::emit_log(app, "build", "开始构建");
    let result = util::stream_command(&mut cmd, app, "pnpm").and_then(|()| {
        paths::write_stamp(harness_dir, &head.commit)
            .with_context(|| format!("写入构建标记 {}", paths::stamp_path(harness_dir).display()))
    });
    let elapsed = util::format_elapsed(started.elapsed());
    match result {
        Ok(()) => {
            util::emit_log(app, "build", &format!("构建完成（用时 {elapsed}）"));
            Ok(())
        }
        Err(err) => {
            util::emit_log(app, "build", &format!("构建失败（用时 {elapsed}）"));
            Err(err)
        }
    }
}
