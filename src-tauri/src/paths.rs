//! App data locations and persisted configuration.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

/// Upstream harness repository this app manages a clone of.
pub const HARNESS_REPO_URL: &str = "https://github.com/deepseek-ai/deepseek-harness.git";

/// Persisted user configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Loopback port the harness web service binds.
    pub port: u16,
    /// Launch the service automatically when the app starts.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Optional executable overrides. Missing values keep automatic discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnpm_path: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3080,
            autostart: true,
            node_path: None,
            pnpm_path: None,
        }
    }
}

/// `~/Library/Application Support/com.danbao.dsh-desktop`, created on demand.
pub fn app_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("com.danbao.dsh-desktop");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Where the managed harness checkout lives. A development override
/// (`DSH_DESKTOP_HARNESS_PATH`) points the whole pipeline at an existing
/// source tree instead — cloning is skipped, everything else behaves the same.
pub fn harness_dir() -> PathBuf {
    match env::var("DSH_DESKTOP_HARNESS_PATH") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => app_dir().join("harness"),
    }
}

/// Whether the harness directory is an externally provided tree (no clone).
pub fn harness_is_external() -> bool {
    env::var_os("DSH_DESKTOP_HARNESS_PATH").is_some()
}

/// Load `config.json`, falling back to defaults; malformed content fails loud.
pub fn load_config() -> anyhow::Result<Config> {
    let path = app_dir().join("config.json");
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| anyhow!("config.json 格式错误: {err}"))
}

/// Persist `config.json` atomically enough for a single-user desktop app.
pub fn save_config(config: &Config) -> anyhow::Result<()> {
    let path = app_dir().join("config.json");
    let text = serde_json::to_string_pretty(config)?;
    fs::write(&path, text + "\n").with_context(|| format!("写入 {}", path.display()))
}

/// Stamps live in the app data dir (never inside the harness tree, which may
/// be the user's own working copy), keyed by a stable hash of the path.
pub fn stamp_path(harness_dir: &std::path::Path) -> PathBuf {
    let key = format!(
        "build-{:016x}.json",
        stable_hash(&harness_dir.to_string_lossy())
    );
    let dir = app_dir().join("state");
    let _ = fs::create_dir_all(&dir);
    dir.join(key)
}

/// FNV-1a: deterministic across runs and machines without a crypto dep.
fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_without_tool_paths_remains_compatible() {
        let config: Config =
            serde_json::from_str(r#"{"port":3088,"autostart":false}"#).expect("legacy config");
        assert_eq!(config.port, 3088);
        assert!(!config.autostart);
        assert_eq!(config.node_path, None);
        assert_eq!(config.pnpm_path, None);
    }
}

/// Commit recorded by the last successful build, if any.
pub fn stamp_commit(harness_dir: &std::path::Path) -> Option<String> {
    let text = fs::read_to_string(stamp_path(harness_dir)).ok()?;
    #[derive(Deserialize)]
    struct Stamp {
        commit: String,
    }
    serde_json::from_str::<Stamp>(&text)
        .ok()
        .map(|stamp| stamp.commit)
}

/// Record the commit the just-finished build covers.
pub fn write_stamp(harness_dir: &std::path::Path, commit: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({ "commit": commit });
    fs::write(
        stamp_path(harness_dir),
        serde_json::to_string_pretty(&body)? + "\n",
    )?;
    Ok(())
}
