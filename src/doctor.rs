//! `kersh doctor`: what this machine has for kersh, checked against the
//! login environment that sessions start from.
//!
//! It reports the harnesses and their versions, the host and session
//! server with their versions, the `kersh-host` unit, gaff and each
//! harness's registration (the same checks `kersh setup` makes), whether
//! each harness and mode runs gaff's guard (from the probe results in
//! `tests/probes/RESULTS.md`), `kersh` on `PATH`, and the eng.rip pieces.
//! It reads files and runs `--version`; it writes nothing.
//!
//! A required piece is one without which a guarded session cannot run:
//! `kersh` and gaff on `PATH`, gaff's hooks for each harness found, and a
//! host and session server of this kersh's version when they run.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::runtime::Config;
use crate::session::{Harness, Mode};

/// What a check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// Present and of this version.
    Ok,
    Missing,
    /// Not running; it starts when needed.
    Stopped,
    /// Running another kersh version than this one.
    Mismatch,
    /// The harness and mode run gaff's guard.
    Guarded,
    /// The harness and mode run without gaff's guard.
    Unguarded,
    /// Whether the guard runs cannot be confirmed.
    Unconfirmed,
    /// The unit file is installed, and the service manager does not
    /// start it at login: launchd has not loaded it, or systemd does not
    /// have it enabled.
    Unloaded,
    /// The check does not apply to this configuration.
    NotApplicable,
}

impl CheckState {
    /// Whether the state satisfies a required check.
    #[must_use]
    pub const fn passes(self) -> bool {
        matches!(self, Self::Ok | Self::Guarded)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Stopped => "stopped",
            Self::Mismatch => "mismatch",
            Self::Guarded => "guarded",
            Self::Unguarded => "unguarded",
            Self::Unconfirmed => "unconfirmed",
            Self::Unloaded => "unloaded",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// One check, as `kersh doctor --json` prints it, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorItem {
    /// `harness`, `path`, `host`, `service`, `gaff`, `guard`, or `eng.rip`.
    pub area: String,
    pub check: String,
    pub state: CheckState,
    /// Whether doctor exits 1 when this check does not pass.
    pub required: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Everything doctor found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub items: Vec<DoctorItem>,
}

impl DoctorReport {
    /// Whether every required check passes.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.items
            .iter()
            .all(|item| !item.required || item.state.passes())
    }
}

fn item(area: &str, check: &str, state: CheckState, required: bool, detail: String) -> DoctorItem {
    DoctorItem {
        area: area.to_owned(),
        check: check.to_owned(),
        state,
        required,
        detail,
        path: None,
        version: None,
    }
}

fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// The first line `program --version` prints, run with `env`.
fn version_of(program: &Path, env: &[(String, String)]) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim().to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

/// A program on the login `PATH`, with its version.
fn program(
    area: &str,
    name: &str,
    required: bool,
    why: &str,
    env: &[(String, String)],
) -> DoctorItem {
    let path_var = env_value(env, "PATH").unwrap_or_default();
    match crate::harness::find_on_path(name, path_var) {
        Some(path) => {
            let version = version_of(&path, env);
            DoctorItem {
                path: Some(path.clone()),
                version,
                ..item(
                    area,
                    name,
                    CheckState::Ok,
                    required,
                    format!("{}; {why}", path.display()),
                )
            }
        }
        None => item(
            area,
            name,
            CheckState::Missing,
            required,
            format!("no `{name}` on the login PATH; {why}"),
        ),
    }
}

/// Whether a live process holds `path` with a lock.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared).is_err()
}

fn host_item(paths: &Paths, config: &Config) -> DoctorItem {
    let this = crate::proto::VERSION;
    if !lock_held(&paths.host_lock()) {
        let detail = if config.auto_start() {
            "no host runs; the first write starts one".to_owned()
        } else {
            "no host runs and auto_start is off; `kersh host` starts one".to_owned()
        };
        return item("host", "host", CheckState::Stopped, false, detail);
    }
    let Some(lock) = crate::host::read_lock(paths) else {
        return item(
            "host",
            "host",
            CheckState::Ok,
            true,
            format!(
                "a host runs; {} names no version",
                paths.host_lock().display()
            ),
        );
    };
    let (state, detail) = if lock.version == this {
        (
            CheckState::Ok,
            format!("pid {}, kersh {}", lock.pid, lock.version),
        )
    } else {
        (
            CheckState::Mismatch,
            format!(
                "pid {} runs kersh {} and this is kersh {this}; stop that host so one of this \
                 version starts",
                lock.pid, lock.version
            ),
        )
    };
    DoctorItem {
        version: Some(lock.version),
        ..item("host", "host", state, true, detail)
    }
}

fn server_item(paths: &Paths) -> DoctorItem {
    if !crate::mux::lock_held(paths) {
        return item(
            "host",
            "session server",
            CheckState::Stopped,
            false,
            "no session server runs; the host starts one with the first session".to_owned(),
        );
    }
    let Some(lock) = crate::mux::read_lock(paths) else {
        return item(
            "host",
            "session server",
            CheckState::Ok,
            true,
            format!(
                "a session server runs; {} names no version",
                paths.mux_lock().display()
            ),
        );
    };
    let (state, detail) = match crate::mux::check_wire(paths) {
        Ok(()) => (
            CheckState::Ok,
            format!(
                "pid {}, kersh {}, rmux wire {}",
                lock.pid, lock.version, lock.wire_version
            ),
        ),
        Err(error) => (CheckState::Mismatch, error),
    };
    DoctorItem {
        version: Some(lock.version),
        ..item("host", "session server", state, true, detail)
    }
}

/// Whether each mode of `harness` runs gaff's guard, from the probe
/// results: rig calls `gaff hook` itself; Claude Code ran a registered
/// `PreToolUse` hook in both modes; `codex exec` runs a hook only once
/// Codex trusts it, and skips an untrusted one silently; interactive
/// Codex was not probed.
#[must_use]
pub fn guard_states(
    harness: Harness,
    registered: bool,
    gaff_found: bool,
) -> Vec<(Mode, CheckState, String)> {
    let modes = [Mode::Headless, Mode::Interactive];
    match harness {
        Harness::Rig => modes
            .into_iter()
            .map(|mode| {
                if gaff_found {
                    (
                        mode,
                        CheckState::Guarded,
                        "rig calls `gaff hook` on every bash call".to_owned(),
                    )
                } else {
                    (
                        mode,
                        CheckState::Unguarded,
                        "rig calls `gaff hook`, and no gaff is on the login PATH".to_owned(),
                    )
                }
            })
            .collect(),
        Harness::ClaudeCode => modes
            .into_iter()
            .map(|mode| {
                if registered {
                    (
                        mode,
                        CheckState::Guarded,
                        "Claude Code runs gaff's registered PreToolUse hook in this mode"
                            .to_owned(),
                    )
                } else {
                    (
                        mode,
                        CheckState::Unguarded,
                        "gaff's hooks are not registered; `kersh setup claude-code` registers \
                         them"
                            .to_owned(),
                    )
                }
            })
            .collect(),
        Harness::Codex => vec![
            if registered {
                (
                    Mode::Headless,
                    CheckState::Unconfirmed,
                    "`codex exec` runs gaff's hook only after Codex trusts it, skips an untrusted \
                     hook without a sign, and has no command that reports hook trust; review \
                     gaff's hook in Codex's hook browser to trust it"
                        .to_owned(),
                )
            } else {
                (
                    Mode::Headless,
                    CheckState::Unguarded,
                    "gaff's hooks are not registered; `kersh setup codex` registers them"
                        .to_owned(),
                )
            },
            (
                Mode::Interactive,
                CheckState::Unguarded,
                "kersh has not verified that interactive Codex runs gaff's hooks, so it refuses \
                 a profiled interactive Codex start"
                    .to_owned(),
            ),
        ],
    }
}

/// The checks, against `login`, the login environment.
#[must_use]
#[allow(clippy::too_many_lines)] // One block per area, in report order.
pub fn doctor(paths: &Paths, config: &Config, login: &[(String, String)]) -> DoctorReport {
    let mut items = Vec::new();

    let builds = crate::harness::list(login);
    items.push(item(
        "harness",
        Harness::Rig.as_str(),
        CheckState::Ok,
        false,
        "built in".to_owned(),
    ));
    for build in &builds {
        items.push(DoctorItem {
            path: build.path.clone(),
            version: build.version.clone(),
            ..item(
                "harness",
                build.harness.as_str(),
                if build.path.is_some() {
                    CheckState::Ok
                } else {
                    CheckState::Missing
                },
                false,
                build.path.as_ref().map_or_else(
                    || {
                        build
                            .error
                            .clone()
                            .unwrap_or_else(|| "not found".to_owned())
                    },
                    |path| path.display().to_string(),
                ),
            )
        });
    }

    items.push(program(
        "path",
        "kersh",
        true,
        "gaff's hook-agent runner calls `kersh run` by name",
        login,
    ));
    let gaff = program(
        "path",
        "gaff",
        true,
        "gaff registers and runs every guard",
        login,
    );
    let gaff_found = gaff.state == CheckState::Ok;
    items.push(gaff);

    items.push(host_item(paths, config));
    items.push(server_item(paths));
    let service = crate::service::status(config.service_manager(), paths);
    items.push(DoctorItem {
        path: service.unit.clone(),
        ..item(
            "service",
            crate::service::UNIT,
            match (&service.unit, service.installed, service.loaded) {
                (None, _, _) => CheckState::NotApplicable,
                (Some(_), true, true) => CheckState::Ok,
                (Some(_), true, false) => CheckState::Unloaded,
                (Some(_), false, _) => CheckState::Missing,
            },
            false,
            service.detail,
        )
    });

    let mut guards = Vec::new();
    for (mode, state, detail) in guard_states(Harness::Rig, false, gaff_found) {
        guards.push(item("guard", &format!("rig {mode}"), state, false, detail));
    }
    for build in builds.iter().filter(|build| build.path.is_some()) {
        let harness = build.harness;
        let mut registered = false;
        for found in crate::setup::harness_items(harness, login) {
            if found.piece == "user-level hooks" {
                registered = found.present;
                items.push(item(
                    "gaff",
                    &format!("{harness} hooks"),
                    if found.present {
                        CheckState::Ok
                    } else {
                        CheckState::Missing
                    },
                    true,
                    found.detail,
                ));
            } else {
                items.push(item(
                    "eng.rip",
                    &format!("{harness} {}", found.piece),
                    if found.present {
                        CheckState::Ok
                    } else {
                        CheckState::Missing
                    },
                    false,
                    found.detail,
                ));
            }
        }
        for (mode, state, detail) in guard_states(harness, registered, gaff_found) {
            guards.push(item(
                "guard",
                &format!("{harness} {mode}"),
                state,
                false,
                detail,
            ));
        }
    }
    items.extend(guards);

    for (name, why) in [
        ("zettel", "zettel holds agent memory"),
        ("almanac", "almanac holds the skills"),
        ("tisket", "tisket holds tasks and plans"),
        (
            "tundish",
            "tundish delivers queued items to a working agent",
        ),
    ] {
        items.push(program("eng.rip", name, false, why, login));
    }
    items.push(memory_item(config, &items));
    let home = env_value(login, "HOME").map_or_else(
        || crate::store::user_home().unwrap_or_else(|| PathBuf::from("/")),
        PathBuf::from,
    );
    for found in crate::setup::carried(login, &home) {
        items.push(item(
            "eng.rip",
            &found.piece,
            if found.present {
                CheckState::Ok
            } else {
                CheckState::Missing
            },
            false,
            found.detail,
        ));
    }

    DoctorReport { items }
}

fn memory_item(config: &Config, items: &[DoctorItem]) -> DoctorItem {
    let Some(root) = &config.memory.root else {
        let zettel = items
            .iter()
            .any(|found| found.check == "zettel" && found.state == CheckState::Ok);
        return item(
            "eng.rip",
            "memory root",
            if zettel {
                CheckState::Ok
            } else {
                CheckState::Missing
            },
            false,
            "memory.root is not set; `kersh memories` reads zettel's root store".to_owned(),
        );
    };
    let present = root.join("zettel.yml").is_file();
    DoctorItem {
        path: Some(root.clone()),
        ..item(
            "eng.rip",
            "memory root",
            if present {
                CheckState::Ok
            } else {
                CheckState::Missing
            },
            false,
            if present {
                format!("memory.root is the zettel store {}", root.display())
            } else {
                format!(
                    "memory.root is {}, which holds no zettel.yml",
                    root.display()
                )
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(harness: Harness, registered: bool, gaff: bool) -> Vec<(Mode, CheckState)> {
        guard_states(harness, registered, gaff)
            .into_iter()
            .map(|(mode, state, _)| (mode, state))
            .collect()
    }

    #[test]
    fn the_guard_states_follow_the_probe_results() {
        use CheckState::{Guarded, Unconfirmed, Unguarded};
        use Mode::{Headless, Interactive};
        assert_eq!(
            states(Harness::ClaudeCode, true, true),
            [(Headless, Guarded), (Interactive, Guarded)]
        );
        assert_eq!(
            states(Harness::ClaudeCode, false, true),
            [(Headless, Unguarded), (Interactive, Unguarded)]
        );
        assert_eq!(
            states(Harness::Codex, true, true),
            [(Headless, Unconfirmed), (Interactive, Unguarded)]
        );
        assert_eq!(
            states(Harness::Codex, false, true),
            [(Headless, Unguarded), (Interactive, Unguarded)]
        );
        assert_eq!(
            states(Harness::Rig, false, true),
            [(Headless, Guarded), (Interactive, Guarded)]
        );
        assert_eq!(
            states(Harness::Rig, false, false),
            [(Headless, Unguarded), (Interactive, Unguarded)]
        );
    }

    #[test]
    fn only_a_failed_required_check_fails_the_report() {
        let mut report = DoctorReport {
            items: vec![item(
                "guard",
                "codex interactive",
                CheckState::Unguarded,
                false,
                String::new(),
            )],
        };
        assert!(report.ok());
        report.items.push(item(
            "path",
            "gaff",
            CheckState::Missing,
            true,
            String::new(),
        ));
        assert!(!report.ok());
    }
}
