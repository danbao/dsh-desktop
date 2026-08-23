//! Toolchain detection: node / pnpm / git presence and version compatibility.

use std::process::Command;

use serde::Serialize;

use crate::util;

/// Result of probing the host toolchain the build and service pipeline needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvInfo {
    pub node_version: Option<String>,
    pub pnpm_version: Option<String>,
    pub git_version: Option<String>,
    /// Absolute binary paths used to spawn children (None when missing).
    pub node_bin: Option<String>,
    pub pnpm_bin: Option<String>,
    /// Human-readable blockers; empty when ready.
    pub problems: Vec<String>,
    pub ready: bool,
}

impl EnvInfo {
    /// Probe the login-shell PATH for every tool and validate versions.
    pub fn probe() -> Self {
        let node_bin = util::which("node").map(|p| p.to_string_lossy().into_owned());
        let pnpm_bin = util::which("pnpm").map(|p| p.to_string_lossy().into_owned());
        let git_bin = util::which("git").map(|p| p.to_string_lossy().into_owned());

        let node_version = node_bin.as_deref().and_then(version_flag("-v"));
        let pnpm_version = pnpm_bin.as_deref().and_then(version_flag("--version"));

        let mut problems = Vec::new();
        if node_bin.is_none() {
            problems.push("未找到 node（需要 Node.js >=24）".to_string());
        }
        if pnpm_bin.is_none() {
            problems.push("未找到 pnpm".to_string());
        }
        let git_version = git_bin.as_deref().and_then(|git| {
            extracted_version(git, &["--version"], |out| {
                out.split_whitespace().nth(2).map(str::to_string)
            })
        });
        if let Some(version) = &node_version {
            if !node_version_ok(version) {
                problems.push(format!("node {version} 不满足要求（>=24.0.0）"));
            }
        }

        EnvInfo {
            ready: problems.is_empty(),
            node_version,
            pnpm_version,
            git_version,
            node_bin,
            pnpm_bin,
            problems,
        }
    }
}

fn version_flag(flag: &'static str) -> impl Fn(&str) -> Option<String> {
    move |bin| {
        Command::new(bin)
            .arg(flag)
            // Shims like corepack's pnpm launch `node` via env(1), so probes
            // need the login-shell PATH, not the GUI-inherited one.
            .env("PATH", util::login_path())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
fn extracted_version<F>(bin: &str, args: &[&str], extract: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let output = Command::new(bin)
        .args(args)
        .env("PATH", util::login_path())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    extract(&String::from_utf8_lossy(&output.stdout))
}

/// 本项目的工具链要求：Node `>=24.0.0`（上游 harness engines 为
/// `^22.19.0 || >=24.0.0`，本项目统一取 >=24）。
fn node_version_ok(version: &str) -> bool {
    let Some((major, _minor)) = parse_node_version(version) else {
        return false;
    };
    major >= 24
}

fn parse_node_version(version: &str) -> Option<(u64, u64)> {
    let rest = version.trim().trim_start_matches('v');
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}
