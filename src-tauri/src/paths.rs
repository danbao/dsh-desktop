//! App data locations and persisted configuration.

use std::env;
use std::ffi::OsString;
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
    /// Optional npm registry override for corepack/pnpm. Missing values fall
    /// back to the user's global `~/.npmrc`, then the upstream default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_registry: Option<String>,
    /// Optional harness checkout location. Must be an existing git repo; the
    /// app then leaves its git state alone (no clone/fetch/reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_path: Option<String>,
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
            npm_registry: None,
            harness_path: None,
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

/// Durable rotating log files live here (`logs/app.log`, …).
pub fn logs_dir() -> PathBuf {
    let dir = app_dir().join("logs");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Where the managed harness checkout lives. A development override
/// (`DSH_DESKTOP_HARNESS_PATH`) or a user-configured path points the whole
/// pipeline at an existing source tree instead: the app never clones,
/// fetches, or resets it — it only reads HEAD and builds.
pub fn harness_dir() -> PathBuf {
    let config_path = load_config().ok().and_then(|config| config.harness_path);
    resolve_harness_dir(
        env::var("DSH_DESKTOP_HARNESS_PATH").ok().as_deref(),
        config_path.as_deref(),
        env::var_os("HOME"),
        &app_dir().join("harness"),
    )
}

/// Resolution order: environment override, persisted config, managed default.
/// Pure so tests never touch process-global state.
fn resolve_harness_dir(
    env_override: Option<&str>,
    config_path: Option<&str>,
    home: Option<OsString>,
    default: &std::path::Path,
) -> PathBuf {
    if let Some(path) = env_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(path) = config_path.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(expanded) = expand_home(path, home) {
            return expanded;
        }
    }
    default.to_path_buf()
}

/// Whether the harness directory is an externally provided tree whose git
/// state the app must leave alone.
pub fn harness_is_external() -> bool {
    env::var("DSH_DESKTOP_HARNESS_PATH")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || load_config()
            .ok()
            .and_then(|config| config.harness_path)
            .is_some_and(|value| !value.trim().is_empty())
}

/// Expand a leading `~` against `home`; anything else passes through
/// unchanged. Shared by tool-path overrides and the harness path.
pub(crate) fn expand_home(value: &str, home: Option<OsString>) -> Option<PathBuf> {
    if value == "~" || value.starts_with("~/") {
        let home = home?;
        return Some(if value == "~" {
            PathBuf::from(home)
        } else {
            PathBuf::from(home).join(&value[2..])
        });
    }
    Some(PathBuf::from(value))
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
        assert_eq!(config.npm_registry, None);
        assert_eq!(config.harness_path, None);
    }

    #[test]
    fn harness_dir_prefers_env_then_config_then_default() {
        let home = OsString::from("/tmp/dsh-home");
        let default = PathBuf::from("/tmp/dsh-app/harness");
        assert_eq!(
            resolve_harness_dir(None, None, Some(home.clone()), &default),
            default
        );
        assert_eq!(
            resolve_harness_dir(None, Some("~/src/harness"), Some(home.clone()), &default),
            PathBuf::from("/tmp/dsh-home/src/harness")
        );
        assert_eq!(
            resolve_harness_dir(
                Some("/tmp/dev-harness"),
                Some("~/src/harness"),
                Some(home.clone()),
                &default
            ),
            PathBuf::from("/tmp/dev-harness")
        );
        // Empty strings fall through instead of pointing at the working dir.
        assert_eq!(
            resolve_harness_dir(Some("  "), Some(""), Some(home), &default),
            default
        );
    }
}
