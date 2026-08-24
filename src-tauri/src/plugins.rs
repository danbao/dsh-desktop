//! Manage out-of-tree DSH plugin bundles installed in the `web` profile.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{envinfo::EnvInfo, toolchain::Tool, util};

const PROFILE_NAME: &str = "web";
const BUILTIN_BUNDLES: &[&str] = &[
    "@deepseek-ai/dsh-base",
    "@deepseek-ai/dsh-web-app",
    "@deepseek-ai/dsh-headless",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub package_name: String,
    pub display_name: String,
    pub description: String,
    pub homepage: Option<String>,
    pub requested_version: Option<String>,
    pub installed_version: Option<String>,
    pub curated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCatalog {
    pub profile: String,
    pub plugins: Vec<PluginInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdate {
    pub package_name: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagePluginRequest {
    pub action: PluginAction,
    pub package_spec: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagePluginResult {
    pub catalog: PluginCatalog,
    pub service_restarted: bool,
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileManifest {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dsh: ProfileDsh,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileDsh {
    #[serde(default)]
    profile: ProfileBundles,
    #[serde(default)]
    bundle: BundleDeclaration,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileBundles {
    #[serde(default)]
    bundles: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct BundleDeclaration {
    patch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstalledManifest {
    version: String,
    #[serde(default)]
    dsh: ProfileDsh,
}

pub fn list_plugins() -> anyhow::Result<PluginCatalog> {
    read_plugins_from(&web_profile_dir())
}

pub fn validate_request(request: &ManagePluginRequest) -> anyhow::Result<()> {
    parse_package_spec(&request.package_spec).map(|_| ())
}

pub fn check_updates(harness_dir: &Path, env: &EnvInfo) -> Vec<PluginUpdate> {
    let catalog = match list_plugins() {
        Ok(catalog) => catalog,
        Err(err) => {
            return vec![PluginUpdate {
                package_name: "*".into(),
                latest_version: None,
                update_available: false,
                error: Some(err.to_string()),
            }];
        }
    };
    catalog
        .plugins
        .iter()
        .map(
            |plugin| match registry_latest(harness_dir, env, &plugin.package_name) {
                Ok(latest) => PluginUpdate {
                    package_name: plugin.package_name.clone(),
                    update_available: is_update_available(
                        plugin.installed_version.as_deref(),
                        &latest,
                    ),
                    latest_version: Some(latest),
                    error: None,
                },
                Err(err) => PluginUpdate {
                    package_name: plugin.package_name.clone(),
                    latest_version: None,
                    update_available: false,
                    error: Some(err.to_string()),
                },
            },
        )
        .collect()
}

pub fn run_operation(
    harness_dir: &Path,
    env: &EnvInfo,
    app: &AppHandle,
    request: &ManagePluginRequest,
) -> anyhow::Result<(PluginCatalog, String)> {
    let spec = parse_package_spec(&request.package_spec)?;
    let before = list_plugins()?;
    let installed_version = before
        .plugins
        .iter()
        .find(|plugin| plugin.package_name == spec.name)
        .and_then(|plugin| plugin.installed_version.as_deref());
    let requested_version = before
        .plugins
        .iter()
        .find(|plugin| plugin.package_name == spec.name)
        .and_then(|plugin| plugin.requested_version.as_deref());
    let operation_version = operation_version(request.action, installed_version, requested_version);
    let args = operation_args(request.action, &spec, operation_version)?;
    run_profile_cli(harness_dir, env, app, &args)?;

    let after = list_plugins()?;
    if request.action == PluginAction::Install {
        let is_bundle = after
            .plugins
            .iter()
            .any(|plugin| plugin.package_name == spec.name && plugin.installed_version.is_some());
        if !is_bundle {
            let cleanup = operation_args(
                PluginAction::Remove,
                &PackageSpec {
                    name: spec.name.clone(),
                    selector: None,
                },
                Some("cleanup"),
            )?;
            let cleanup_result = run_profile_cli(harness_dir, env, app, &cleanup);
            return match cleanup_result {
                Ok(()) => Err(anyhow!("{} 未声明 dsh.bundle.patch，已撤销安装", spec.name)),
                Err(err) => Err(anyhow!(
                    "{} 不是可加载的 DSH 插件，且自动清理失败：{err}",
                    spec.name
                )),
            };
        }
    }

    let message = match request.action {
        PluginAction::Install => format!("已安装 {}", spec.name),
        PluginAction::Update => format!("已升级 {}", spec.name),
        PluginAction::Reinstall => format!("已重装 {}", spec.name),
        PluginAction::Remove => format!("已卸载 {}", spec.name),
    };
    Ok((after, message))
}

fn run_profile_cli(
    harness_dir: &Path,
    env: &EnvInfo,
    app: &AppHandle,
    args: &[String],
) -> anyhow::Result<()> {
    let mut cmd = env.command(Tool::Node)?;
    cmd.args(["--import", "tsx/esm", "apps/cli/src/bin.ts"])
        .args(args)
        .current_dir(harness_dir);
    util::stream_command(&mut cmd, app, "plugin")
}

fn registry_latest(harness_dir: &Path, env: &EnvInfo, name: &str) -> anyhow::Result<String> {
    let mut cmd = env.command(Tool::Pnpm)?;
    cmd.args(["view", name, "version", "--json"])
        .current_dir(harness_dir);
    let stdout = capture_with_timeout(&mut cmd, Duration::from_secs(15))?;
    if let Ok(version) = serde_json::from_str::<String>(&stdout) {
        return Ok(version);
    }
    let version = stdout.trim().trim_matches('"');
    if version.is_empty() {
        bail!("registry 未返回 {name} 的版本");
    }
    Ok(version.to_string())
}

fn capture_with_timeout(cmd: &mut Command, timeout: Duration) -> anyhow::Result<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("无法启动 {}", cmd.get_program().to_string_lossy()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("查询 registry 超时（{} 秒）", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut stdout)?;
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)?;
    if !status.success() {
        let detail = stderr.trim();
        bail!(
            "registry 查询失败{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            }
        );
    }
    Ok(stdout)
}

fn web_profile_dir() -> PathBuf {
    let home = std::env::var_os("DSH_HOME")
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".dsh")
        });
    home.join("profiles").join(PROFILE_NAME)
}

fn read_plugins_from(profile_dir: &Path) -> anyhow::Result<PluginCatalog> {
    let manifest_path = profile_dir.join("package.json");
    let manifest = if manifest_path.is_file() {
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("读取 {}", manifest_path.display()))?;
        serde_json::from_str::<ProfileManifest>(&text)
            .with_context(|| format!("解析 {}", manifest_path.display()))?
    } else {
        ProfileManifest::default()
    };
    let bundle_names = manifest
        .dsh
        .profile
        .bundles
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let mut plugins = Vec::new();
    for curated in curated_plugins() {
        let requested_version = manifest.dependencies.get(curated.package_name).cloned();
        let installed_version = bundle_names
            .contains(curated.package_name)
            .then(|| installed_bundle_version(profile_dir, curated.package_name))
            .flatten();
        plugins.push(PluginInfo {
            package_name: curated.package_name.into(),
            display_name: curated.display_name.into(),
            description: curated.description.into(),
            homepage: Some(curated.homepage.into()),
            requested_version,
            installed_version,
            curated: true,
        });
    }

    for (name, requested_version) in &manifest.dependencies {
        if curated_plugins()
            .iter()
            .any(|plugin| plugin.package_name == name)
            || BUILTIN_BUNDLES.contains(&name.as_str())
            || !bundle_names.contains(name.as_str())
            || !valid_package_name(name)
        {
            continue;
        }
        plugins.push(PluginInfo {
            package_name: name.clone(),
            display_name: name.clone(),
            description: "第三方 DSH 插件".into(),
            homepage: None,
            requested_version: Some(requested_version.clone()),
            installed_version: installed_bundle_version(profile_dir, name),
            curated: false,
        });
    }

    Ok(PluginCatalog {
        profile: PROFILE_NAME.into(),
        plugins,
    })
}

fn installed_bundle_version(profile_dir: &Path, name: &str) -> Option<String> {
    let path = profile_dir
        .join("node_modules")
        .join(name)
        .join("package.json");
    let text = fs::read_to_string(path).ok()?;
    let manifest = serde_json::from_str::<InstalledManifest>(&text).ok()?;
    manifest.dsh.bundle.patch.as_ref()?;
    Some(manifest.version)
}

struct CuratedPlugin {
    package_name: &'static str,
    display_name: &'static str,
    description: &'static str,
    homepage: &'static str,
}

fn curated_plugins() -> &'static [CuratedPlugin] {
    &[CuratedPlugin {
        package_name: "dsh-ringcentral",
        display_name: "RingCentral",
        description: "连接 RingCentral Team Messaging，将 Bot 消息接入 DSH agent。",
        homepage: "https://github.com/ringclaw/dsh-ringcentral",
    }]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageSpec {
    name: String,
    selector: Option<String>,
}

impl PackageSpec {
    fn render(&self) -> String {
        match self.selector.as_deref() {
            Some(selector) => format!("{}@{selector}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginAction {
    Install,
    Update,
    Reinstall,
    Remove,
}

impl PluginAction {
    pub fn busy_label(self) -> &'static str {
        match self {
            Self::Install => "安装插件",
            Self::Update => "升级插件",
            Self::Reinstall => "重装插件",
            Self::Remove => "卸载插件",
        }
    }
}

fn operation_args(
    action: PluginAction,
    spec: &PackageSpec,
    installed_version: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut args = vec!["plugin".into(), "--profile".into(), PROFILE_NAME.into()];
    match action {
        PluginAction::Install => {
            args.extend(["add".into(), spec.render()]);
        }
        PluginAction::Update | PluginAction::Reinstall | PluginAction::Remove => {
            if spec.selector.is_some() {
                bail!("升级、重装或卸载时只接受已安装的包名");
            }
            let installed_version = installed_version.ok_or_else(|| anyhow!("插件尚未安装"))?;
            match action {
                PluginAction::Update => {
                    args.extend(["add".into(), format!("{}@latest", spec.name)]);
                }
                PluginAction::Reinstall => {
                    args.extend([
                        "add".into(),
                        format!("{}@{installed_version}", spec.name),
                        "--force".into(),
                    ]);
                }
                PluginAction::Remove => {
                    args.extend(["remove".into(), spec.name.clone()]);
                }
                PluginAction::Install => unreachable!(),
            }
        }
    }
    Ok(args)
}

fn operation_version<'a>(
    action: PluginAction,
    installed: Option<&'a str>,
    requested: Option<&'a str>,
) -> Option<&'a str> {
    if action == PluginAction::Remove {
        installed.or(requested)
    } else {
        installed
    }
}

fn parse_package_spec(raw: &str) -> anyhow::Result<PackageSpec> {
    if raw.is_empty() || raw.trim() != raw || raw.starts_with('-') {
        bail!("请输入 npm 包名，可附带精确版本或 dist-tag");
    }

    let selector_at = if raw.starts_with('@') {
        let slash = raw
            .find('/')
            .ok_or_else(|| anyhow!("scope 包名必须使用 @scope/name 格式"))?;
        raw[slash + 1..].rfind('@').map(|index| slash + 1 + index)
    } else {
        raw.rfind('@')
    };
    let (name, selector) = match selector_at {
        Some(index) => (&raw[..index], Some(&raw[index + 1..])),
        None => (raw, None),
    };

    if !valid_package_name(name) {
        bail!("无效的 npm 包名：{name}");
    }
    if let Some(selector) = selector {
        if selector.is_empty()
            || (!valid_dist_tag(selector) && semver::Version::parse(selector).is_err())
        {
            bail!("版本必须是精确 SemVer 或安全的 dist-tag");
        }
    }

    Ok(PackageSpec {
        name: name.to_string(),
        selector: selector.map(str::to_string),
    })
}

fn valid_package_name(name: &str) -> bool {
    if let Some(scoped) = name.strip_prefix('@') {
        let Some((scope, package)) = scoped.split_once('/') else {
            return false;
        };
        !scope.is_empty()
            && !package.is_empty()
            && !package.contains('/')
            && valid_name_part(scope)
            && valid_name_part(package)
    } else {
        !name.is_empty() && !name.contains('/') && valid_name_part(name)
    }
}

fn valid_name_part(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_dist_tag(value: &str) -> bool {
    valid_name_part(value)
}

fn is_update_available(installed: Option<&str>, latest: &str) -> bool {
    let Some(installed) = installed else {
        return false;
    };
    match (
        semver::Version::parse(installed),
        semver::Version::parse(latest),
    ) {
        (Ok(installed), Ok(latest)) => latest > installed,
        _ => latest != installed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn accepts_registry_package_names_with_versions_or_tags() {
        assert_eq!(
            parse_package_spec("dsh-ringcentral@0.3.3").unwrap(),
            PackageSpec {
                name: "dsh-ringcentral".into(),
                selector: Some("0.3.3".into()),
            }
        );
        assert_eq!(
            parse_package_spec("@ringcentral/dsh-plugin@beta").unwrap(),
            PackageSpec {
                name: "@ringcentral/dsh-plugin".into(),
                selector: Some("beta".into()),
            }
        );
        assert_eq!(
            parse_package_spec("dsh-ringcentral").unwrap(),
            PackageSpec {
                name: "dsh-ringcentral".into(),
                selector: None,
            }
        );
    }

    #[test]
    fn rejects_non_registry_or_ambiguous_package_specs() {
        for rejected in [
            "",
            "--filter",
            "../plugin",
            "file:../plugin",
            "https://example.com/plugin.tgz",
            "git+https://github.com/example/plugin.git",
            "dsh plugin",
            "dsh-ringcentral@^0.3.0",
            "dsh-ringcentral@~0.3.0",
            "@scope",
            "@scope/pkg@",
        ] {
            assert!(parse_package_spec(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn lists_curated_and_installed_custom_bundles_from_a_profile() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path();
        fs::write(
            profile.join("package.json"),
            r#"{
              "dependencies": {
                "dsh-ringcentral": "^0.3.0",
                "acme-dsh": "1.4.0",
                "lodash": "4.17.21"
              },
              "dsh": { "profile": { "bundles": [
                "@deepseek-ai/dsh-base", "dsh-ringcentral", "acme-dsh", "@deepseek-ai/dsh-web-app"
              ] } }
            }"#,
        )
        .unwrap();
        write_installed_bundle(profile, "dsh-ringcentral", "0.3.3");
        write_installed_bundle(profile, "acme-dsh", "1.4.0");

        let catalog = read_plugins_from(profile).unwrap();

        assert_eq!(catalog.profile, "web");
        assert_eq!(catalog.plugins.len(), 2);
        assert_eq!(
            catalog.plugins[0],
            PluginInfo {
                package_name: "dsh-ringcentral".into(),
                display_name: "RingCentral".into(),
                description: "连接 RingCentral Team Messaging，将 Bot 消息接入 DSH agent。".into(),
                homepage: Some("https://github.com/ringclaw/dsh-ringcentral".into()),
                requested_version: Some("^0.3.0".into()),
                installed_version: Some("0.3.3".into()),
                curated: true,
            }
        );
        assert_eq!(catalog.plugins[1].package_name, "acme-dsh");
        assert_eq!(catalog.plugins[1].display_name, "acme-dsh");
        assert_eq!(
            catalog.plugins[1].installed_version.as_deref(),
            Some("1.4.0")
        );
        assert!(!catalog.plugins[1].curated);
    }

    #[test]
    fn keeps_curated_plugins_visible_before_profile_initialization() {
        let temp = tempfile::tempdir().unwrap();

        let catalog = read_plugins_from(temp.path()).unwrap();

        assert_eq!(catalog.plugins.len(), 1);
        assert_eq!(catalog.plugins[0].package_name, "dsh-ringcentral");
        assert_eq!(catalog.plugins[0].installed_version, None);
    }

    #[test]
    fn constructs_profile_cli_arguments_for_each_management_action() {
        assert_eq!(
            operation_args(
                PluginAction::Install,
                &parse_package_spec("dsh-ringcentral@0.3.3").unwrap(),
                None,
            )
            .unwrap(),
            ["plugin", "--profile", "web", "add", "dsh-ringcentral@0.3.3"]
        );
        assert_eq!(
            operation_args(
                PluginAction::Update,
                &parse_package_spec("dsh-ringcentral").unwrap(),
                Some("0.3.3"),
            )
            .unwrap(),
            [
                "plugin",
                "--profile",
                "web",
                "add",
                "dsh-ringcentral@latest"
            ]
        );
        assert_eq!(
            operation_args(
                PluginAction::Reinstall,
                &parse_package_spec("dsh-ringcentral").unwrap(),
                Some("0.3.3"),
            )
            .unwrap(),
            [
                "plugin",
                "--profile",
                "web",
                "add",
                "dsh-ringcentral@0.3.3",
                "--force",
            ]
        );
        assert_eq!(
            operation_args(
                PluginAction::Remove,
                &parse_package_spec("dsh-ringcentral").unwrap(),
                Some("0.3.3"),
            )
            .unwrap(),
            ["plugin", "--profile", "web", "remove", "dsh-ringcentral"]
        );
    }

    #[test]
    fn update_remove_and_reinstall_require_a_managed_bare_package_name() {
        for action in [
            PluginAction::Update,
            PluginAction::Reinstall,
            PluginAction::Remove,
        ] {
            assert!(operation_args(
                action,
                &parse_package_spec("dsh-ringcentral@0.3.3").unwrap(),
                Some("0.3.3"),
            )
            .is_err());
            assert!(operation_args(
                action,
                &parse_package_spec("dsh-ringcentral").unwrap(),
                None,
            )
            .is_err());
        }
        assert_eq!(
            operation_version(PluginAction::Remove, None, Some("^0.3.0")),
            Some("^0.3.0")
        );
        assert_eq!(
            operation_version(PluginAction::Reinstall, None, Some("^0.3.0")),
            None
        );
    }

    #[test]
    fn compares_registry_versions_semantically_with_a_safe_fallback() {
        assert!(is_update_available(Some("0.3.3"), "0.10.0"));
        assert!(!is_update_available(Some("0.10.0"), "0.3.3"));
        assert!(!is_update_available(Some("1.0.0"), "1.0.0"));
        assert!(is_update_available(
            Some("workspace-build"),
            "registry-build"
        ));
        assert!(!is_update_available(None, "1.0.0"));
    }

    fn write_installed_bundle(profile: &std::path::Path, name: &str, version: &str) {
        let dir = profile.join("node_modules").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name":"{name}","version":"{version}","dsh":{{"bundle":{{"patch":"./cordis.patch.yml"}}}}}}"#
            ),
        )
        .unwrap();
    }
}
