//! Runtimes, kersh's config, and how a start's settings resolve.
//!
//! An agent file says what the agent is; a runtime says how and where it
//! runs. Runtimes are named presets in `~/.config/kersh/runtimes.yml`.
//! Settings resolve in this order, later winning: kersh's defaults, the
//! runtime the agent file names, the agent file's own fields, the runtime
//! named by `--runtime`, then the individual flags. A gaff profile set by
//! an earlier layer is never removed by a later one; a later layer may
//! only name another.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::paths::Paths;
use crate::session::{Harness, Mode, PaneSize, Settings, StartRequest};

/// One runtime in `runtimes.yml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `local`, the only environment so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The gaff profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Passed with an interactive resume where the harness takes a prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tempdir: Option<bool>,
    /// Literal variables set in the session.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Harness flags, checked against the harness's allow-list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// A runtime with its name, as `kersh runtime list --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEntry {
    pub name: String,
    #[serde(flatten)]
    pub runtime: Runtime,
}

/// How kersh starts its host and session server when none runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceManager {
    /// Spawn a detached process in its own process group.
    None,
    /// A one-shot launchd job (macOS).
    Launchd,
}

/// The memory settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// The notes store `kersh memories` reads, also `KERSH_MEMORY_ROOT`
    /// in every session.
    #[serde(default)]
    pub root: Option<PathBuf>,
}

/// `~/.config/kersh/config.yml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `none` or `launchd`; launchd on macOS by default, else none.
    #[serde(default)]
    pub service_manager: Option<ServiceManager>,
    /// Whether a client that needs a write starts a host. Default true.
    #[serde(default)]
    pub auto_start: Option<bool>,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// The pane size when the caller is not a terminal.
    #[serde(default)]
    pub pane: Option<PaneSize>,
    /// `COLORFGBG` for every pane; `15;0` (light on dark) by default.
    #[serde(default)]
    pub colorfgbg: Option<String>,
}

impl Config {
    /// Read the config; a missing file is the defaults.
    pub fn load(paths: &Paths) -> Result<Self, String> {
        let path = paths.config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_yaml_ng::from_str(&text)
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// The service manager in effect.
    #[must_use]
    pub fn service_manager(&self) -> ServiceManager {
        self.service_manager
            .unwrap_or(if cfg!(target_os = "macos") {
                ServiceManager::Launchd
            } else {
                ServiceManager::None
            })
    }

    #[must_use]
    pub fn auto_start(&self) -> bool {
        self.auto_start.unwrap_or(true)
    }
}

/// Read every runtime; a missing file has none.
pub fn load_runtimes(paths: &Paths) -> Result<BTreeMap<String, Runtime>, String> {
    let path = paths.runtimes();
    match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
        Ok(text) => {
            serde_yaml_ng::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Whether `name` is a safe runtime or session name: letters, digits,
/// `.`, `_`, and `-`, not starting with `-`.
#[must_use]
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `model` can reach an argument vector without reading as an
/// option.
#[must_use]
pub fn safe_model(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('-')
        && model.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/' | b'[' | b']')
        })
}

/// Variables kersh sets itself, which a runtime may not override.
const RESERVED_ENV: &[&str] = &[
    "KERSH_SESSION",
    "KERSH_MEMORY_ROOT",
    "GAFF_PROFILE",
    "COLORTERM",
    "COLORFGBG",
];

/// Check a runtime before it is written or used.
pub fn validate(name: &str, runtime: &Runtime) -> Result<(), String> {
    if !safe_name(name) {
        return Err(format!(
            "`{name}` is not a valid runtime name; use letters, digits, `.`, `_`, and `-`"
        ));
    }
    if let Some(environment) = &runtime.environment
        && environment != "local"
    {
        return Err(format!(
            "runtime `{name}`: environment `{environment}` is not available; use `local`"
        ));
    }
    if let Some(model) = &runtime.model
        && !safe_model(model)
    {
        return Err(format!(
            "runtime `{name}`: `{model}` is not a safe model name"
        ));
    }
    for key in runtime.env.keys() {
        let ok = !key.is_empty()
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return Err(format!("runtime `{name}`: `{key}` is not a variable name"));
        }
        if RESERVED_ENV.contains(&key.as_str()) {
            return Err(format!("runtime `{name}`: kersh sets `{key}` itself"));
        }
    }
    if let Some(harness) = runtime.harness {
        crate::harness::check_flags(harness, &runtime.flags)
            .map_err(|error| format!("runtime `{name}`: {error}"))?;
    }
    Ok(())
}

/// Add or replace the runtime `name` from the YAML file `file`, and
/// return the path written.
pub fn add(paths: &Paths, name: &str, file: &Path) -> Result<PathBuf, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| format!("cannot read `{}`: {error}", file.display()))?;
    let runtime: Runtime = if text.trim().is_empty() {
        Runtime::default()
    } else {
        serde_yaml_ng::from_str(&text).map_err(|error| format!("{}: {error}", file.display()))?
    };
    validate(name, &runtime)?;
    let mut all = load_runtimes(paths)?;
    all.insert(name.to_owned(), runtime);
    let yaml = serde_yaml_ng::to_string(&all).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&paths.config).map_err(|error| error.to_string())?;
    let path = paths.runtimes();
    crate::paths::write_private(&path, yaml.as_bytes()).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Every runtime, by name.
pub fn list(paths: &Paths) -> Result<Vec<RuntimeEntry>, String> {
    Ok(load_runtimes(paths)?
        .into_iter()
        .map(|(name, runtime)| RuntimeEntry { name, runtime })
        .collect())
}

/// Lay `layer` over `settings`.
fn apply(settings: &mut Settings, flags: &mut Vec<String>, layer: &Runtime) {
    if let Some(harness) = layer.harness {
        settings.harness = harness;
    }
    if let Some(mode) = layer.mode {
        settings.mode = mode;
    }
    if let Some(model) = &layer.model {
        settings.model = Some(model.clone());
    }
    if let Some(environment) = &layer.environment {
        settings.environment.clone_from(environment);
    }
    if let Some(profile) = &layer.profile {
        settings.profile = Some(profile.clone());
    }
    if let Some(prompt) = &layer.resume_prompt {
        settings.resume_prompt = Some(prompt.clone());
    }
    if let Some(tempdir) = layer.tempdir {
        settings.tempdir = tempdir;
    }
    settings.env.extend(layer.env.clone());
    if !layer.flags.is_empty() {
        flags.clone_from(&layer.flags);
    }
}

/// The resolved settings and harness flags of a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub settings: Settings,
    pub flags: Vec<String>,
    /// The flags that overrode the files, for the record.
    pub overrides: BTreeMap<String, String>,
}

/// Resolve a start's settings from the agent, the runtimes, and the
/// request, in the documented order.
pub fn resolve(
    agent: Option<&Agent>,
    runtimes: &BTreeMap<String, Runtime>,
    request: &StartRequest,
) -> Result<Resolved, String> {
    let lookup = |name: &str| {
        runtimes
            .get(name)
            .ok_or_else(|| format!("no runtime named `{name}` in runtimes.yml"))
    };
    let agent_runtime = agent
        .and_then(|agent| agent.meta.runtime.as_deref())
        .map(lookup)
        .transpose()?;
    let named_runtime = request.runtime.as_deref().map(lookup).transpose()?;

    let mut settings = Settings {
        harness: Harness::Rig,
        mode: Harness::Rig.default_mode(),
        model: None,
        environment: "local".to_owned(),
        profile: None,
        resume_prompt: None,
        tempdir: false,
        env: BTreeMap::new(),
    };
    let mut flags = Vec::new();
    let mut mode_set = false;
    if let Some(layer) = agent_runtime {
        apply(&mut settings, &mut flags, layer);
        mode_set |= layer.mode.is_some();
    }
    if let Some(agent) = agent
        && let Some(profile) = &agent.meta.profile
    {
        settings.profile = Some(profile.clone());
    }
    if let Some(layer) = named_runtime {
        apply(&mut settings, &mut flags, layer);
        mode_set |= layer.mode.is_some();
    }

    let mut overrides = BTreeMap::new();
    if let Some(harness) = request.harness {
        settings.harness = harness;
        overrides.insert("harness".to_owned(), harness.to_string());
    }
    if let Some(mode) = request.mode {
        settings.mode = mode;
        mode_set = true;
        overrides.insert("mode".to_owned(), mode.to_string());
    }
    if !mode_set {
        settings.mode = settings.harness.default_mode();
    }
    if let Some(model) = &request.model {
        settings.model = Some(model.clone());
        overrides.insert("model".to_owned(), model.clone());
    }
    if let Some(profile) = &request.profile {
        settings.profile = Some(profile.clone());
        overrides.insert("profile".to_owned(), profile.clone());
    }
    if request.tempdir {
        settings.tempdir = true;
        overrides.insert("tempdir".to_owned(), "true".to_owned());
    }
    if let Some(skill) = &request.skill {
        overrides.insert("skill".to_owned(), skill.clone());
    }
    if let Some(file) = &request.instructions_file {
        overrides.insert("instructions_file".to_owned(), file.display().to_string());
    }
    if !request.harness_flags.is_empty() {
        flags.extend(request.harness_flags.iter().cloned());
    }

    if let Some(model) = &settings.model
        && !safe_model(model)
    {
        return Err(format!("`{model}` is not a safe model name"));
    }
    crate::harness::check_flags(settings.harness, &flags)?;
    Ok(Resolved {
        settings,
        flags,
        overrides,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn agent(text: &str) -> Agent {
        let name = text
            .lines()
            .find_map(|line| line.strip_prefix("name: "))
            .unwrap()
            .to_owned();
        Agent::parse(text, Path::new("AGENT.md"), &name).unwrap()
    }

    fn runtimes(yaml: &str) -> BTreeMap<String, Runtime> {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn later_layers_win_and_the_mode_defaults_by_harness() {
        let all = runtimes(
            "worker:\n  harness: claude-code\n  model: opus\n  profile: worker\n\
             fast:\n  model: haiku\n",
        );
        let fixer = agent("---\nname: fixer\nruntime: worker\n---\nFix.\n");
        let request = StartRequest {
            runtime: Some("fast".to_owned()),
            ..StartRequest::default()
        };
        let resolved = resolve(Some(&fixer), &all, &request).unwrap();
        assert_eq!(resolved.settings.harness, Harness::ClaudeCode);
        assert_eq!(resolved.settings.mode, Mode::Interactive);
        assert_eq!(resolved.settings.model.as_deref(), Some("haiku"));
        assert_eq!(resolved.settings.profile.as_deref(), Some("worker"));
    }

    #[test]
    fn a_later_layer_never_removes_a_profile() {
        let all =
            runtimes("worker:\n  harness: codex\n  profile: worker\nbare:\n  mode: headless\n");
        let request = StartRequest {
            runtime: Some("bare".to_owned()),
            ..StartRequest::default()
        };
        let fixer = agent("---\nname: fixer\nruntime: worker\n---\nFix.\n");
        let resolved = resolve(Some(&fixer), &all, &request).unwrap();
        assert_eq!(resolved.settings.profile.as_deref(), Some("worker"));
        assert_eq!(resolved.settings.mode, Mode::Headless);
    }

    #[test]
    fn flags_override_the_files_and_are_recorded() {
        let request = StartRequest {
            harness: Some(Harness::Codex),
            mode: Some(Mode::Headless),
            model: Some("gpt-6".to_owned()),
            harness_flags: vec!["--search".to_owned()],
            ..StartRequest::default()
        };
        let resolved = resolve(None, &BTreeMap::new(), &request).unwrap();
        assert_eq!(resolved.settings.harness, Harness::Codex);
        assert_eq!(resolved.flags, ["--search"]);
        assert_eq!(
            resolved.overrides.get("model").map(String::as_str),
            Some("gpt-6")
        );
    }

    #[test]
    fn an_unknown_runtime_and_a_refused_flag_fail() {
        let request = StartRequest {
            runtime: Some("nope".to_owned()),
            ..StartRequest::default()
        };
        assert!(resolve(None, &BTreeMap::new(), &request).is_err());
        let request = StartRequest {
            harness: Some(Harness::ClaudeCode),
            harness_flags: vec!["--bare".to_owned()],
            ..StartRequest::default()
        };
        assert!(resolve(None, &BTreeMap::new(), &request).is_err());
    }

    #[test]
    fn a_runtime_is_validated_before_it_is_written() {
        let runtime = Runtime {
            env: BTreeMap::from([("KERSH_SESSION".to_owned(), "x".to_owned())]),
            ..Runtime::default()
        };
        assert!(validate("w", &runtime).is_err());
        assert!(validate("../w", &Runtime::default()).is_err());
        let unknown: Result<Runtime, _> = serde_yaml_ng::from_str("secrets: [x]\n");
        assert!(unknown.is_err(), "an unknown field is refused");
    }

    #[test]
    fn add_writes_runtimes_yml_and_list_reads_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("state"), tmp.path().join("config"));
        let file = tmp.path().join("worker.yml");
        std::fs::write(
            &file,
            "harness: claude-code\nmode: interactive\nprofile: worker\n",
        )
        .unwrap();
        let written = add(&paths, "worker", &file).unwrap();
        assert_eq!(written, paths.runtimes());
        std::fs::write(&file, "harness: codex\n").unwrap();
        add(&paths, "coder", &file).unwrap();
        let entries = list(&paths).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["coder", "worker"]);
        assert_eq!(
            entries.get(1).unwrap().runtime.profile.as_deref(),
            Some("worker")
        );
    }
}
