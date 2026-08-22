//! Git operations on the managed harness checkout, driven by the user's `git`.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context};
use serde::Serialize;
use tauri::AppHandle;

use crate::{paths, util};

/// Current HEAD facts shown in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadInfo {
    pub commit: String,
    pub short_commit: String,
    pub subject: String,
    pub commit_date: String,
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    // A desktop app must never hang on a credential prompt.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn git_bin() -> anyhow::Result<String> {
    util::which("git")
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("未找到 git"))
}

/// Whether `dir` is an existing git work tree.
pub fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Clone the upstream repository shallowly if `dir` is not a repo yet.
/// Returns `true` when a fresh clone was made.
pub fn ensure_cloned(dir: &Path, app: &AppHandle) -> anyhow::Result<bool> {
    if is_repo(dir) {
        return Ok(false);
    }
    if paths::harness_is_external() {
        return Err(anyhow!(
            "DSH_DESKTOP_HARNESS_PATH 指向的目录不是 git 仓库：{}",
            dir.display()
        ));
    }
    util::emit_log(
        app,
        "git",
        &format!("克隆 {}（浅克隆，跟随上游默认分支）", paths::HARNESS_REPO_URL),
    );
    std::fs::create_dir_all(dir.parent().expect("harness dir has a parent"))?;
    let mut cmd = Command::new(git_bin()?);
    // No --branch: track whatever the upstream default branch is.
    cmd.arg("clone")
        .args(["--depth", "1", "--single-branch"])
        .arg(paths::HARNESS_REPO_URL)
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0");
    util::stream_command(&mut cmd, app, "git")?;
    Ok(true)
}

/// Fetch the upstream default branch shallowly, leaving it in `FETCH_HEAD`.
/// Following HEAD (rather than a pinned name) survives upstream renames.
pub fn fetch_latest(dir: &Path, app: &AppHandle) -> anyhow::Result<()> {
    let mut cmd = git(dir);
    cmd.args(["fetch", "--depth", "1", "origin"]);
    util::stream_command(&mut cmd, app, "git")
}

/// Commits on the freshly fetched upstream that HEAD lacks. On shallow clones
/// any difference collapses to `1`, which the UI reports as "有新版本".
pub fn behind_count(dir: &Path) -> anyhow::Result<u32> {
    let output = git(dir)
        .args(["rev-list", "--count", "HEAD..FETCH_HEAD"])
        .output()
        .context("无法运行 git rev-list")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git rev-list 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let count = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(count)
}

/// Hard-reset HEAD to `FETCH_HEAD` and drop untracked non-ignored files, so a
/// force-push upstream cannot wedge the managed clone.
pub fn reset_to_fetch_head(dir: &Path, app: &AppHandle) -> anyhow::Result<()> {
    let mut reset = git(dir);
    reset.args(["reset", "--hard", "FETCH_HEAD"]);
    util::stream_command(&mut reset, app, "git")?;
    let mut clean = git(dir);
    clean.args(["clean", "-fd"]);
    util::stream_command(&mut clean, app, "git")
}

/// HEAD facts, or `None` before the first clone.
pub fn head_info(dir: &Path) -> Option<HeadInfo> {
    if !is_repo(dir) {
        return None;
    }
    let output = git(dir)
        .args(["log", "-1", "--format=%H%n%h%n%ci%n%s"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let commit = lines.next()?.to_string();
    let short_commit = lines.next()?.to_string();
    let commit_date = lines.next()?.to_string();
    let subject = lines.next().unwrap_or_default().to_string();
    Some(HeadInfo {
        commit,
        short_commit,
        subject,
        commit_date,
    })
}
