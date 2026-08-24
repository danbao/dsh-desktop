//! Discover a terminal-equivalent toolchain for a macOS GUI application.
//!
//! Finder-launched applications inherit a minimal environment. This module
//! hides shell startup rules, version-manager fallbacks, executable probing,
//! and the final child-process PATH behind one small interface.

use std::collections::HashSet;
use std::ffi::{CStr, OsStr, OsString};
use std::fs;
use std::io::{Read, Result as IoResult};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde::Serialize;

use crate::paths::Config;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum Tool {
    Node,
    Pnpm,
    Git,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Pnpm => "pnpm",
            Self::Git => "git",
        }
    }
}

/// A fully resolved and validated toolchain. Callers never need to know how
/// shell startup or version-manager discovery works.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolchainEnv {
    pub node_version: Option<String>,
    pub pnpm_version: Option<String>,
    pub git_version: Option<String>,
    pub node_bin: Option<String>,
    pub pnpm_bin: Option<String>,
    pub git_bin: Option<String>,
    pub node_source: Option<String>,
    pub pnpm_source: Option<String>,
    pub git_source: Option<String>,
    pub shell: Option<String>,
    pub discovery_notes: Vec<String>,
    pub configured_node_path: Option<String>,
    pub configured_pnpm_path: Option<String>,
    pub problems: Vec<String>,
    pub ready: bool,
    #[serde(skip)]
    effective_path: OsString,
}

#[derive(Debug, Clone)]
struct PathEntry {
    dir: PathBuf,
    source: String,
}

#[derive(Debug)]
struct ResolvedTool {
    path: PathBuf,
    version: String,
    source: String,
}

impl ToolchainEnv {
    pub fn discover(config: &Config) -> Self {
        let shell = login_shell();
        let shell_name = shell
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned());
        let mut notes = Vec::new();
        let mut entries = Vec::new();

        if let Some(path) = std::env::var_os("PATH") {
            add_path_entries(&mut entries, &path, "继承环境");
        }

        if let Some(shell_path) = shell.as_deref() {
            match capture_shell_path(shell_path, PROBE_TIMEOUT) {
                Ok(path) => add_path_entries(
                    &mut entries,
                    &path,
                    &format!("登录 shell ({})", shell_name.as_deref().unwrap_or("未知")),
                ),
                Err(err) => notes.push(format!("登录 shell 环境读取失败：{err}")),
            }
        } else {
            notes.push("无法确定用户登录 shell，已使用目录扫描".to_string());
        }

        add_fallback_entries(&mut entries);
        dedup_entries(&mut entries);

        let configured_node = normalized_override(config.node_path.as_deref());
        let configured_pnpm = normalized_override(config.pnpm_path.as_deref());
        let mut problems = Vec::new();

        let base_path = join_entries(&entries);
        let node = if let Some(path) = configured_node.as_deref() {
            match resolve_manual(path, Tool::Node, &base_path) {
                Ok(tool) => Some(tool),
                Err(err) => {
                    problems.push(err);
                    None
                }
            }
        } else {
            resolve_automatic(Tool::Node, &entries, &base_path)
        };
        if configured_node.is_none() && node.is_none() {
            problems.push("未找到可用 node（需要 Node.js >=24）".to_string());
        }

        let path_with_node = prepend_tool_dir(&base_path, node.as_ref());
        let pnpm = if let Some(path) = configured_pnpm.as_deref() {
            match resolve_manual(path, Tool::Pnpm, &path_with_node) {
                Ok(tool) => Some(tool),
                Err(err) => {
                    problems.push(err);
                    None
                }
            }
        } else {
            resolve_automatic(Tool::Pnpm, &entries, &path_with_node)
        };
        if configured_pnpm.is_none() && pnpm.is_none() {
            problems.push("未找到可运行的 pnpm".to_string());
        }

        let path_with_package_tools = prepend_tool_dir(&path_with_node, pnpm.as_ref());
        let git = resolve_automatic(Tool::Git, &entries, &path_with_package_tools);
        if git.is_none() {
            problems.push("未找到可运行的 git（请安装 Xcode Command Line Tools）".to_string());
        }

        let effective_path = prepend_tool_dir(&path_with_package_tools, git.as_ref());
        Self {
            node_version: node.as_ref().map(|tool| tool.version.clone()),
            pnpm_version: pnpm.as_ref().map(|tool| tool.version.clone()),
            git_version: git.as_ref().map(|tool| tool.version.clone()),
            node_bin: node.as_ref().map(display_path),
            pnpm_bin: pnpm.as_ref().map(display_path),
            git_bin: git.as_ref().map(display_path),
            node_source: node.as_ref().map(|tool| tool.source.clone()),
            pnpm_source: pnpm.as_ref().map(|tool| tool.source.clone()),
            git_source: git.as_ref().map(|tool| tool.source.clone()),
            shell: shell.map(|path| path.to_string_lossy().into_owned()),
            discovery_notes: notes,
            configured_node_path: config.node_path.clone(),
            configured_pnpm_path: config.pnpm_path.clone(),
            ready: problems.is_empty(),
            problems,
            effective_path,
        }
    }

    pub fn command(&self, tool: Tool) -> anyhow::Result<Command> {
        let bin = match tool {
            Tool::Node => self.node_bin.as_deref(),
            Tool::Pnpm => self.pnpm_bin.as_deref(),
            Tool::Git => self.git_bin.as_deref(),
        }
        .ok_or_else(|| anyhow!("未找到可运行的 {}", tool.name()))?;
        let mut command = Command::new(bin);
        command.env("PATH", &self.effective_path);
        Ok(command)
    }

    /// Manual overrides must never be silently ignored in favor of an
    /// automatically discovered executable.
    pub fn validate_overrides(&self) -> Result<(), String> {
        let node_valid =
            self.configured_node_path.is_none() || self.node_source.as_deref() == Some("手动配置");
        let pnpm_valid =
            self.configured_pnpm_path.is_none() || self.pnpm_source.as_deref() == Some("手动配置");
        if node_valid && pnpm_valid {
            Ok(())
        } else {
            Err(self.problems.join("；"))
        }
    }
}

fn display_path(tool: &ResolvedTool) -> String {
    tool.path.to_string_lossy().into_owned()
}

fn normalized_override(value: Option<&str>) -> Option<PathBuf> {
    normalized_override_with_home(value, std::env::var_os("HOME"))
}

fn normalized_override_with_home(value: Option<&str>, home: Option<OsString>) -> Option<PathBuf> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
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

fn resolve_manual(path: &Path, tool: Tool, path_env: &OsStr) -> Result<ResolvedTool, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{} 手动路径必须是绝对路径或以 ~/ 开头",
            tool.name()
        ));
    }
    if !is_executable(path) {
        return Err(format!(
            "{} 手动路径不存在或不可执行：{}",
            tool.name(),
            path.display()
        ));
    }
    let candidate_path = prepend_dir(path_env, path.parent());
    let Some(version) = probe_version(path, tool, &candidate_path) else {
        return Err(format!(
            "{} 手动路径无法运行：{}",
            tool.name(),
            path.display()
        ));
    };
    if matches!(tool, Tool::Node) && !node_version_ok(&version) {
        return Err(format!("node {version} 不满足要求（>=24.0.0）"));
    }
    Ok(ResolvedTool {
        path: path.to_path_buf(),
        version,
        source: "手动配置".to_string(),
    })
}

fn resolve_automatic(tool: Tool, entries: &[PathEntry], path_env: &OsStr) -> Option<ResolvedTool> {
    let mut seen = HashSet::new();
    for entry in entries {
        let candidate = entry.dir.join(tool.name());
        if !seen.insert(candidate.clone()) || !is_executable(&candidate) {
            continue;
        }
        let candidate_path = prepend_dir(path_env, candidate.parent());
        let Some(version) = probe_version(&candidate, tool, &candidate_path) else {
            continue;
        };
        if matches!(tool, Tool::Node) && !node_version_ok(&version) {
            continue;
        }
        return Some(ResolvedTool {
            path: candidate,
            version,
            source: entry.source.clone(),
        });
    }
    None
}

fn probe_version(path: &Path, tool: Tool, path_env: &OsStr) -> Option<String> {
    let mut command = Command::new(path);
    match tool {
        Tool::Node => {
            command.arg("-v");
        }
        Tool::Pnpm => {
            command.arg("--version");
        }
        Tool::Git => {
            command.arg("--version");
        }
    }
    command.env("PATH", path_env);
    let output = run_capture(command, PROBE_TIMEOUT).ok()?;
    if output.timed_out || !output.success {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    if text.is_empty() {
        return None;
    }
    Some(match tool {
        Tool::Git => text.split_whitespace().nth(2)?.to_string(),
        _ => text.to_string(),
    })
}

fn node_version_ok(version: &str) -> bool {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .is_some_and(|major| major >= 24)
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn add_path_entries(entries: &mut Vec<PathEntry>, path: &OsStr, source: &str) {
    entries.extend(
        std::env::split_paths(path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| PathEntry {
                dir,
                source: source.to_string(),
            }),
    );
}

fn add_fallback_entries(entries: &mut Vec<PathEntry>) {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for (dir, source) in [
            (home.join(".volta/bin"), "Volta"),
            (home.join(".asdf/shims"), "asdf"),
            (home.join(".local/share/mise/shims"), "mise"),
        ] {
            entries.push(PathEntry {
                dir,
                source: source.to_string(),
            });
        }

        add_version_dirs(entries, &home.join(".nvm/versions/node"), "bin", "NVM");
        add_version_dirs(
            entries,
            &home.join(".local/share/fnm/node-versions"),
            "installation/bin",
            "fnm",
        );
        add_version_dirs(
            entries,
            &home.join("Library/Application Support/fnm/node-versions"),
            "installation/bin",
            "fnm",
        );
    }

    for (dir, source) in [
        ("/opt/homebrew/bin", "Homebrew"),
        ("/usr/local/bin", "Homebrew / usr-local"),
        ("/usr/bin", "macOS 系统"),
        ("/bin", "macOS 系统"),
        ("/usr/sbin", "macOS 系统"),
        ("/sbin", "macOS 系统"),
    ] {
        entries.push(PathEntry {
            dir: PathBuf::from(dir),
            source: source.to_string(),
        });
    }
}

fn add_version_dirs(entries: &mut Vec<PathEntry>, root: &Path, suffix: &str, source: &str) {
    let Ok(read_dir) = fs::read_dir(root) else {
        return;
    };
    let mut versions: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|item| item.path())
        .collect();
    versions.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
    entries.extend(versions.into_iter().map(|version| PathEntry {
        dir: version.join(suffix),
        source: source.to_string(),
    }));
}

fn version_key(path: &Path) -> (u64, u64, u64) {
    let text = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .trim_start_matches('v')
        .to_string();
    let mut parts = text.split('.').filter_map(|part| part.parse::<u64>().ok());
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn dedup_entries(entries: &mut Vec<PathEntry>) {
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.dir.clone()));
}

fn join_entries(entries: &[PathEntry]) -> OsString {
    std::env::join_paths(entries.iter().map(|entry| &entry.dir))
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

fn prepend_tool_dir(path: &OsStr, tool: Option<&ResolvedTool>) -> OsString {
    prepend_dir(path, tool.and_then(|tool| tool.path.parent()))
}

fn prepend_dir(path: &OsStr, dir: Option<&Path>) -> OsString {
    let Some(dir) = dir else {
        return path.to_os_string();
    };
    let mut dirs = vec![dir.to_path_buf()];
    dirs.extend(std::env::split_paths(path).filter(|existing| existing != dir));
    std::env::join_paths(dirs).unwrap_or_else(|_| path.to_os_string())
}

fn login_shell() -> Option<PathBuf> {
    // getpwuid reflects the configured login shell even when Finder did not
    // populate SHELL. Copy the C string immediately before another libc call.
    let account_shell = unsafe {
        let passwd = libc::getpwuid(libc::geteuid());
        if passwd.is_null() || (*passwd).pw_shell.is_null() {
            None
        } else {
            Some(PathBuf::from(OsString::from_vec(
                CStr::from_ptr((*passwd).pw_shell).to_bytes().to_vec(),
            )))
        }
    };
    account_shell
        .filter(|path| is_executable(path))
        .or_else(|| {
            std::env::var_os("SHELL")
                .map(PathBuf::from)
                .filter(|path| is_executable(path))
        })
        .or_else(|| Some(PathBuf::from("/bin/zsh")).filter(|path| is_executable(path)))
}

fn capture_shell_path(shell: &Path, timeout: Duration) -> anyhow::Result<OsString> {
    capture_shell_path_with_home(shell, timeout, None)
}

fn capture_shell_path_with_home(
    shell: &Path,
    timeout: Duration,
    home: Option<&Path>,
) -> anyhow::Result<OsString> {
    let name = shell.file_name().unwrap_or_default().to_string_lossy();
    let mut command = Command::new(shell);
    if let Some(home) = home {
        command.env("HOME", home).env("ZDOTDIR", home);
    }
    match name.as_ref() {
        "bash" => {
            // bash login shells read profiles but not .bashrc. The interactive
            // child inherits the profile environment and then reads .bashrc.
            command
                .args([
                    "--login",
                    "-c",
                    "exec \"$DSH_LOGIN_SHELL\" -ic '/usr/bin/env -0'",
                ])
                .env("DSH_LOGIN_SHELL", shell);
        }
        "zsh" | "fish" | "csh" | "tcsh" => {
            command.args(["-lic", "/usr/bin/env -0"]);
        }
        _ => {
            command.args(["-lc", "/usr/bin/env -0"]);
        }
    }
    let output =
        run_capture(command, timeout).with_context(|| format!("无法启动 {}", shell.display()))?;
    if output.timed_out {
        return Err(anyhow!("{} 超过 {} 秒", shell.display(), timeout.as_secs()));
    }
    if !output.success {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "{} 退出失败{}",
            shell.display(),
            stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| format!("：{}", line.trim()))
                .unwrap_or_default()
        ));
    }
    parse_path_record(&output.stdout).ok_or_else(|| anyhow!("shell 输出中没有 PATH"))
}

fn parse_path_record(output: &[u8]) -> Option<OsString> {
    let records: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    // Prefer a real env(1) record. Only use the newline form when startup
    // noise was glued to env's first record because it did not end in NUL.
    for record in &records {
        if let Some(path) = record.strip_prefix(b"PATH=") {
            if !path.is_empty() {
                return Some(OsString::from_vec(path.to_vec()));
            }
        }
    }
    for record in records {
        if let Some(index) = find_subslice(record, b"\nPATH=").map(|index| index + 1) {
            let path = &record[index + 5..];
            if !path.is_empty() {
                return Some(OsString::from_vec(path.to_vec()));
            }
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Capture {
    success: bool,
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_capture(mut command: Command, timeout: Duration) -> IoResult<Capture> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_thread = thread::spawn(move || drain_limited(stdout));
    let stderr_thread = thread::spawn(move || drain_limited(stderr));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let success = loop {
        if let Some(status) = child.try_wait()? {
            break status.success();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Shell startup files may leave background descendants holding our pipes.
    // Tear down the private probe group even after the direct child exits.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    if timed_out {
        let _ = child.wait();
    }
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(Capture {
        success,
        timed_out,
        stdout,
        stderr,
    })
}

fn drain_limited<R: Read>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let Ok(count) = reader.read(&mut buffer) else {
            break;
        };
        if count == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_file(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write fixture");
    }

    fn write_executable(path: &Path, contents: &str) {
        write_file(path, contents);
        let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod fixture");
    }

    #[test]
    fn parses_path_after_noisy_shell_output() {
        let output = b"hello from shell\nOTHER=value\0PATH=/one:/two\0HOME=/tmp\0";
        assert_eq!(parse_path_record(output), Some(OsString::from("/one:/two")));
    }

    #[test]
    fn parses_path_when_noise_is_attached_to_first_record() {
        let output = b"startup banner\nPATH=/nvm/bin:/usr/bin\0HOME=/tmp\0";
        assert_eq!(
            parse_path_record(output),
            Some(OsString::from("/nvm/bin:/usr/bin"))
        );
    }

    #[test]
    fn does_not_confuse_manpath_with_path() {
        let output = b"MANPATH=/manual\0PATH=/actual\0";
        assert_eq!(parse_path_record(output), Some(OsString::from("/actual")));
    }

    #[test]
    fn expands_home_but_not_arbitrary_variables() {
        assert_eq!(
            normalized_override_with_home(Some("~/node"), Some(OsString::from("/tmp/dsh-home"))),
            Some(PathBuf::from("/tmp/dsh-home/node"))
        );
        assert_eq!(
            normalized_override_with_home(
                Some("$HOME/node"),
                Some(OsString::from("/tmp/dsh-home"))
            ),
            Some(PathBuf::from("$HOME/node"))
        );
    }

    #[test]
    fn sorts_semantic_versions_numerically() {
        assert!(version_key(Path::new("v24.10.1")) > version_key(Path::new("v24.9.9")));
    }

    #[test]
    fn accepts_only_supported_node_major() {
        assert!(node_version_ok("v24.0.0"));
        assert!(node_version_ok("25.1.0"));
        assert!(!node_version_ok("v22.19.0"));
        assert!(!node_version_ok("not-a-version"));
    }

    #[test]
    fn zsh_reads_login_and_interactive_startup_files() {
        let home = tempfile::tempdir().expect("temp home");
        let node_dir = home.path().join("rc-bin");
        fs::create_dir(&node_dir).expect("node dir");
        write_executable(&node_dir.join("node"), "#!/bin/sh\necho v24.19.0\n");
        write_file(
            &home.path().join(".zprofile"),
            "export PATH=\"$HOME/profile-bin:$PATH\"\n",
        );
        write_file(
            &home.path().join(".zshrc"),
            "print 'startup noise'\nexport PATH=\"$HOME/rc-bin:$PATH\"\n",
        );
        let path = capture_shell_path_with_home(
            Path::new("/bin/zsh"),
            Duration::from_secs(2),
            Some(home.path()),
        )
        .expect("capture zsh PATH");
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert!(dirs.contains(&home.path().join("profile-bin")));
        assert!(dirs.contains(&node_dir));

        let mut entries = Vec::new();
        add_path_entries(&mut entries, &path, "登录 shell (zsh)");
        let node = resolve_automatic(Tool::Node, &entries, &path).expect("node from .zshrc");
        assert_eq!(node.path, node_dir.join("node"));
        assert_eq!(node.source, "登录 shell (zsh)");
    }

    #[test]
    fn bash_reads_profile_and_bashrc() {
        let home = tempfile::tempdir().expect("temp home");
        write_file(
            &home.path().join(".bash_profile"),
            "export PATH=\"$HOME/profile-bin:$PATH\"\n",
        );
        write_file(
            &home.path().join(".bashrc"),
            "echo 'startup noise'\nexport PATH=\"$HOME/rc-bin:$PATH\"\n",
        );
        let path = capture_shell_path_with_home(
            Path::new("/bin/bash"),
            Duration::from_secs(2),
            Some(home.path()),
        )
        .expect("capture bash PATH");
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert!(dirs.contains(&home.path().join("profile-bin")));
        assert!(dirs.contains(&home.path().join("rc-bin")));
    }

    #[test]
    fn shell_startup_is_bounded_by_timeout() {
        let home = tempfile::tempdir().expect("temp home");
        write_file(&home.path().join(".zshrc"), "sleep 5\n");
        let started = Instant::now();
        let result = capture_shell_path_with_home(
            Path::new("/bin/zsh"),
            Duration::from_millis(100),
            Some(home.path()),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn background_startup_process_cannot_hold_probe_pipes_open() {
        let home = tempfile::tempdir().expect("temp home");
        write_file(&home.path().join(".zshrc"), "sleep 5 &\n");
        let started = Instant::now();
        let result = capture_shell_path_with_home(
            Path::new("/bin/zsh"),
            Duration::from_secs(1),
            Some(home.path()),
        );
        assert!(result.is_ok());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn pnpm_env_shim_uses_the_selected_node_directory() {
        let fixture = tempfile::tempdir().expect("tool fixture");
        let node = fixture.path().join("node");
        let pnpm = fixture.path().join("pnpm");
        write_executable(
            &node,
            "#!/bin/sh\nif [ \"$1\" = \"-v\" ]; then echo v24.1.0; else echo 11.0.0; fi\n",
        );
        write_executable(&pnpm, "#!/usr/bin/env node\n");

        let base = OsString::from("/usr/bin:/bin");
        let resolved_node = resolve_manual(&node, Tool::Node, &base).expect("manual node");
        let path = prepend_tool_dir(&base, Some(&resolved_node));
        let resolved_pnpm = resolve_manual(&pnpm, Tool::Pnpm, &path).expect("pnpm shim");
        assert_eq!(resolved_pnpm.version, "11.0.0");
    }

    #[test]
    #[ignore = "host smoke test; requires the README toolchain"]
    fn finder_style_host_environment_discovers_the_full_toolchain() {
        let env = ToolchainEnv::discover(&Config::default());
        assert!(env.ready, "{}", env.problems.join("；"));
        assert!(env.node_version.as_deref().is_some_and(node_version_ok));
        assert!(env.pnpm_version.is_some());
        assert!(env.git_version.is_some());
    }
}
