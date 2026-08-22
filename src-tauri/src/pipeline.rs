//! Build pipeline: decide whether artifacts are stale, run install/build,
//! and stamp the covered commit.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context};
use tauri::AppHandle;

use crate::{envinfo::EnvInfo, gitops, paths, util};

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
    let pnpm = env
        .pnpm_bin
        .clone()
        .ok_or_else(|| anyhow!("未找到 pnpm"))?;
    let mut cmd = Command::new(pnpm);
    cmd.arg("install").current_dir(harness_dir);
    util::stream_command(&mut cmd, app, "pnpm")
}

/// `pnpm run build`, then record the commit the artifacts now cover.
pub fn build(
    harness_dir: &Path,
    env: &EnvInfo,
    head: &gitops::HeadInfo,
    app: &AppHandle,
) -> anyhow::Result<()> {
    let pnpm = env
        .pnpm_bin
        .clone()
        .ok_or_else(|| anyhow!("未找到 pnpm"))?;
    let mut cmd = Command::new(pnpm);
    cmd.arg("run").arg("build").current_dir(harness_dir);
    util::stream_command(&mut cmd, app, "pnpm")?;
    paths::write_stamp(harness_dir, &head.commit)
        .with_context(|| format!("写入构建标记 {}", paths::stamp_path(harness_dir).display()))?;
    Ok(())
}
