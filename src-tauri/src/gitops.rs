//! Git operations on the managed harness checkout, driven by the user's `git`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context};
use serde::Serialize;
use tauri::AppHandle;

use crate::{envinfo::EnvInfo, paths, toolchain::Tool, util};

/// Current HEAD facts shown in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadInfo {
    pub commit: String,
    pub short_commit: String,
    pub subject: String,
    pub commit_date: String,
}

fn git(dir: &Path, env: &EnvInfo) -> anyhow::Result<Command> {
    let mut cmd = env.command(Tool::Git)?;
    cmd.current_dir(dir);
    // A desktop app must never hang on a credential prompt.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    Ok(cmd)
}

/// Whether `dir` is an existing git work tree.
pub fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Clone the upstream repository shallowly if `dir` is not a repo yet.
/// Returns `true` when a fresh clone was made.
pub fn ensure_cloned(dir: &Path, app: &AppHandle, env: &EnvInfo) -> anyhow::Result<bool> {
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
        &format!(
            "克隆 {}（浅克隆，跟随上游默认分支）",
            paths::HARNESS_REPO_URL
        ),
    );
    std::fs::create_dir_all(dir.parent().expect("harness dir has a parent"))?;
    let mut cmd = env.command(Tool::Git)?;
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
pub fn fetch_latest(dir: &Path, app: &AppHandle, env: &EnvInfo) -> anyhow::Result<()> {
    let mut cmd = git(dir, env)?;
    cmd.args(["fetch", "--depth", "1", "origin"]);
    util::stream_command(&mut cmd, app, "git")
}

/// Commits on the freshly fetched upstream that HEAD lacks. On shallow clones
/// any difference collapses to `1`, which the UI reports as "有新版本".
pub fn behind_count(dir: &Path, env: &EnvInfo) -> anyhow::Result<u32> {
    let output = git(dir, env)?
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
pub fn reset_to_fetch_head(dir: &Path, app: &AppHandle, env: &EnvInfo) -> anyhow::Result<()> {
    let mut reset = git(dir, env)?;
    reset.args(["reset", "--hard", "FETCH_HEAD"]);
    util::stream_command(&mut reset, app, "git")?;
    let mut clean = git(dir, env)?;
    clean.args(["clean", "-fd"]);
    util::stream_command(&mut clean, app, "git")?;
    let removed = remove_orphan_packages(dir)?;
    if !removed.is_empty() {
        util::emit_log(
            app,
            "git",
            &format!(
                "清理上游已删除包的残留目录（{} 个）：{}",
                removed.len(),
                removed.join("、")
            ),
        );
    }
    Ok(())
}

/// Workspace glob roots of the harness repository whose subdirectories tsdown
/// treats as buildable packages.
const WORKSPACE_PACKAGE_ROOTS: &[(&str, usize)] = &[("packages", 2), ("vendor", 1), ("apps", 1)];

/// Residue a deleted package leaves behind: ignored build output that
/// `git clean -fd` keeps. Anything else marks the directory as live content.
const ORPHAN_RESIDUE_DIRS: &[&str] = &["lib", "node_modules", ".typecheck"];
const ORPHAN_RESIDUE_SUFFIX: &str = ".tsbuildinfo";

/// When upstream deletes a package, `git reset --hard` removes its tracked
/// files but `git clean -fd` keeps the ignored build output, so a
/// manifest-less directory full of stale `lib` trees survives the update.
/// tsdown's workspace glob still matches those directories and bundles the
/// stale code, failing the build on exports that no longer exist upstream.
/// Remove a package directory only when it has no `package.json` and contains
/// nothing but build residue; anything else is left untouched. Returns the
/// removed directories.
pub fn remove_orphan_packages(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut candidates = Vec::new();
    for (root, depth) in WORKSPACE_PACKAGE_ROOTS {
        collect_dirs_at_depth(&dir.join(root), *depth, &mut candidates)?;
    }
    let mut removed = Vec::new();
    for path in candidates {
        if !is_orphan_package(&path) {
            continue;
        }
        fs::remove_dir_all(&path).with_context(|| format!("移除孤儿包目录 {}", path.display()))?;
        removed.push(path.to_string_lossy().into_owned());
    }
    Ok(removed)
}

fn collect_dirs_at_depth(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if depth == 0 {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_dirs_at_depth(&entry.path(), depth - 1, out)?;
        }
    }
    Ok(())
}

fn is_orphan_package(path: &Path) -> bool {
    if path.join("package.json").exists() {
        return false;
    }
    match fs::read_dir(path) {
        Ok(entries) => entries.flatten().all(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            ORPHAN_RESIDUE_DIRS.contains(&name.as_ref()) || name.ends_with(ORPHAN_RESIDUE_SUFFIX)
        }),
        Err(_) => false,
    }
}

/// HEAD facts, or `None` before the first clone.
pub fn head_info(dir: &Path, env: &EnvInfo) -> Option<HeadInfo> {
    if !is_repo(dir) {
        return None;
    }
    let output = git(dir, env)
        .ok()?
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-desktop-gitops-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn orphan_package_dirs_are_removed_and_live_ones_kept() {
        let root = temp_dir("orphans");
        // Orphan: a deleted package leaving only ignored residue behind.
        fs::create_dir_all(root.join("packages/host/apiproxy/lib/types")).unwrap();
        fs::write(
            root.join("packages/host/apiproxy/lib/types/api-proxy.js"),
            "stale",
        )
        .unwrap();
        fs::create_dir_all(root.join("packages/host/apiproxy/node_modules/x")).unwrap();
        // Live package: a manifest keeps it even with build output beside it.
        fs::create_dir_all(root.join("packages/host/live/lib")).unwrap();
        fs::write(root.join("packages/host/live/package.json"), "{}").unwrap();
        fs::write(root.join("packages/host/live/lib/index.js"), "export {}").unwrap();
        // Unknown content without a manifest: never touched.
        fs::create_dir_all(root.join("packages/host/mystery")).unwrap();
        fs::write(root.join("packages/host/mystery/keep.txt"), "data").unwrap();
        // One-level orphan under vendor, and a tsbuildinfo-only orphan.
        fs::create_dir_all(root.join("vendor/oldpkg/lib")).unwrap();
        fs::write(root.join("vendor/oldpkg/lib/x.js"), "stale").unwrap();
        fs::create_dir_all(root.join("packages/util/cached")).unwrap();
        fs::write(root.join("packages/util/cached/tsconfig.tsbuildinfo"), "{}").unwrap();

        let removed = remove_orphan_packages(&root).expect("clean");

        assert_eq!(removed.len(), 3, "{removed:?}");
        assert!(!root.join("packages/host/apiproxy").exists());
        assert!(!root.join("vendor/oldpkg").exists());
        assert!(!root.join("packages/util/cached").exists());
        assert!(root.join("packages/host/live/package.json").exists());
        assert!(root.join("packages/host/live/lib/index.js").exists());
        assert!(root.join("packages/host/mystery/keep.txt").exists());
    }
}
