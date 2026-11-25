//! `kersh setup <harness>`: check and complete a harness's wiring into
//! gaff, the notes store, and the skill library.
//!
//! Registration is gaff's: setup runs `gaff init --host <harness>` and
//! writes no harness settings itself. It then checks each piece the
//! wiring needs and reports it present or missing, so a machine where a
//! piece is missing is told so instead of being told it is set up.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::Harness;

/// One checked piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupItem {
    pub piece: String,
    pub present: bool,
    pub detail: String,
}

/// What setup found, as `kersh setup --json` prints it, one item per
/// line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReport {
    pub harness: Harness,
    pub items: Vec<SetupItem>,
}

impl SetupReport {
    /// Whether every piece is present.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.items.iter().all(|item| item.present)
    }
}

/// The gaff sections the wiring carries.
const SECTIONS: &[&str] = &[
    "memory-howto",
    "agent-memories",
    "prime-tisket",
    "prime-zettel",
    "prime-gaff",
    "prime-almanac",
    "prime-tundish",
    "ecosystem",
];

/// The gaff guards the wiring carries.
const GUARDS: &[&str] = &["memory-needs-root", "no-self-approval"];

fn item(piece: &str, present: bool, detail: impl Into<String>) -> SetupItem {
    SetupItem {
        piece: piece.to_owned(),
        present,
        detail: detail.into(),
    }
}

fn gaff_config(env: &[(String, String)], home: &Path) -> PathBuf {
    env.iter()
        .find(|(key, value)| key == "XDG_CONFIG_HOME" && !value.is_empty())
        .map_or_else(|| home.join(".config"), |(_, value)| PathBuf::from(value))
        .join("gaff")
        .join("gaff.yml")
}

/// Names under a top-level list of `{name: ...}` maps in gaff's config.
fn names_in(config: &serde_yaml_ng::Value, key: &str) -> Vec<String> {
    config
        .get(key)
        .and_then(serde_yaml_ng::Value::as_sequence)
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.get("name").and_then(serde_yaml_ng::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Run gaff's own registration, then check every piece.
pub fn setup(harness: Harness, cwd: &Path) -> Result<SetupReport, String> {
    if harness == Harness::Rig {
        return Err("rig calls gaff itself; it needs no setup".to_owned());
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    let home = crate::store::user_home().unwrap_or_else(|| PathBuf::from("/"));
    let mut items = vec![gaff_init(harness, cwd)];
    items.extend(harness_items(harness, &env));
    items.extend(carried(&env, &home));
    Ok(SetupReport { harness, items })
}

fn gaff_init(harness: Harness, cwd: &Path) -> SetupItem {
    let init = std::process::Command::new("gaff")
        .args(["init", "--host", harness.as_str()])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    match init {
        Ok(output) if output.status.success() => item(
            "gaff init",
            true,
            format!(
                "`gaff init --host {harness}` ran in {}: {}",
                cwd.display(),
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        ),
        Ok(output) => item(
            "gaff init",
            false,
            format!(
                "`gaff init --host {harness}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(error) => item("gaff init", false, format!("cannot run gaff: {error}")),
    }
}

/// The checks for one harness under `env`: gaff's user-level hooks, and
/// for Claude Code its auto memory. They read files and write nothing.
#[must_use]
pub fn harness_items(harness: Harness, env: &[(String, String)]) -> Vec<SetupItem> {
    let mut items = Vec::new();
    let files = crate::guard::user_hook_files(harness, env);
    let shown = files
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let registered = crate::guard::registered_for_user(harness, env);
    items.push(item(
        "user-level hooks",
        registered,
        if registered {
            format!("gaff hook registered in {shown}")
        } else {
            format!(
                "no gaff hook in {shown}; `gaff init` registers only the repository's settings, \
                 so a session outside this repository runs unguarded"
            )
        },
    ));

    if harness == Harness::ClaudeCode {
        let settings = files.first().cloned().unwrap_or_default();
        let off = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("autoMemoryEnabled")
                    .and_then(serde_json::Value::as_bool)
            })
            == Some(false);
        items.push(item(
            "auto memory off",
            off,
            if off {
                format!("autoMemoryEnabled is false in {}", settings.display())
            } else {
                format!(
                    "autoMemoryEnabled is not false in {}; sessions kersh starts pass it off anyway",
                    settings.display()
                )
            },
        ));
    }
    items
}

/// The carried eng.rip rules in gaff's user config under `env`: the
/// sections, the guards, `transitions.agent_may_set`, and the tundish
/// handler. They read files and write nothing.
#[must_use]
pub fn carried(env: &[(String, String)], home: &Path) -> Vec<SetupItem> {
    let mut items = Vec::new();
    let config_path = gaff_config(env, home);
    let config = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|text| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text).ok());
    let sections = config
        .as_ref()
        .map(|c| names_in(c, "sections"))
        .unwrap_or_default();
    let guards = config
        .as_ref()
        .map(|c| names_in(c, "guards"))
        .unwrap_or_default();
    for section in SECTIONS {
        let present = sections.iter().any(|name| name == section);
        items.push(item(
            &format!("section {section}"),
            present,
            format!(
                "{} in {}",
                if present { "declared" } else { "not declared" },
                config_path.display()
            ),
        ));
    }
    for guard in GUARDS {
        let present = guards.iter().any(|name| name == guard);
        items.push(item(
            &format!("guard {guard}"),
            present,
            format!(
                "{} in {}",
                if present { "declared" } else { "not declared" },
                config_path.display()
            ),
        ));
    }
    let may_set = config
        .as_ref()
        .and_then(|c| c.get("transitions"))
        .and_then(|t| t.get("agent_may_set"))
        .is_some();
    items.push(item(
        "transitions.agent_may_set",
        may_set,
        format!(
            "{} in {}",
            if may_set { "declared" } else { "not declared" },
            config_path.display()
        ),
    ));

    let handlers = config_path.with_file_name("handlers.yml");
    let tundish =
        std::fs::read_to_string(&handlers).is_ok_and(|text| text.contains("tundish pull"));
    items.push(item(
        "tundish handler",
        tundish,
        format!(
            "{} in {}",
            if tundish {
                "a `tundish pull` handler is declared"
            } else {
                "no `tundish pull` handler"
            },
            handlers.display()
        ),
    ));
    items
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_from_a_list_of_maps() {
        let config: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("guards:\n  - name: a\n  - name: b\nsections: []\n").unwrap();
        assert_eq!(names_in(&config, "guards"), ["a", "b"]);
        assert!(names_in(&config, "sections").is_empty());
        assert!(names_in(&config, "missing").is_empty());
    }
}
