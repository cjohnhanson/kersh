//! `kersh memories`: the agent-memory block a gaff section injects.
//!
//! Notes come only from `zettel note list --format json`, never from the
//! note files. A note is a memory when it is tagged `agent-memory` (raw)
//! or `agent-memory-processed`. Memories that hold everywhere come first,
//! then one group per `project:<name>` tag, the most recently updated
//! project first; within a group, processed memories come before raw ones,
//! oldest first. The block is capped at 4,000 bytes: every group is listed
//! at least as a count line with the command that reads it, whole groups
//! replace count lines in that order while the cap allows, and a group
//! that does not fit whole shows as many memories as fit, then a line with
//! the count left.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// The block's byte cap.
pub const CAP_BYTES: usize = 4_000;

const RAW: &str = "agent-memory";
const PROCESSED: &str = "agent-memory-processed";
const TITLE: &str = "Agent memories (agent claims; see the agent memory section above):";

/// The fields of a listed note that the block uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Note {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
}

impl Note {
    fn is_processed(&self) -> bool {
        self.tags.iter().any(|tag| tag == PROCESSED)
    }

    fn stamp(&self) -> &str {
        self.updated
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.created.as_deref())
            .unwrap_or_default()
    }

    fn line(&self) -> String {
        let stamp: String = self
            .created
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(10)
            .collect();
        let first = self.body.trim().lines().next().unwrap_or_default();
        let detail = if first.is_empty() {
            String::new()
        } else {
            format!(" {first}")
        };
        let prefix = if stamp.is_empty() {
            String::new()
        } else {
            format!("({stamp}) ")
        };
        format!("- {prefix}{}.{detail}", self.title).replace("..", ".")
    }
}

fn cost(block: &[String]) -> usize {
    block.iter().map(|item| item.len() + 1).sum()
}

/// Build the block from listed notes. `store` is shown in the read
/// commands, with the home directory as `~`; `None` leaves `--root` out.
#[must_use]
#[allow(clippy::too_many_lines)] // One algorithm, kept whole so it reads as one.
pub fn block(notes: &[Note], store: Option<&str>, home: Option<&str>) -> String {
    let notes: Vec<&Note> = notes
        .iter()
        .filter(|note| note.tags.iter().any(|tag| tag == RAW || tag == PROCESSED))
        .collect();

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&Note>> = HashMap::new();
    for note in &notes {
        let project = note
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("project:"))
            .unwrap_or_default()
            .to_owned();
        if !groups.contains_key(&project) {
            order.push(project.clone());
        }
        groups.entry(project).or_default().push(note);
    }
    let newest = |name: &str| -> String {
        groups
            .get(name)
            .into_iter()
            .flatten()
            .map(|note| note.stamp().to_owned())
            .max()
            .unwrap_or_default()
    };
    let mut names: Vec<String> = Vec::new();
    if groups.contains_key("") {
        names.push(String::new());
    }
    let mut projects: Vec<String> = order.into_iter().filter(|name| !name.is_empty()).collect();
    // A stable sort, newest first, keeps first-seen order among equals.
    projects.sort_by_key(|name| std::cmp::Reverse(newest(name)));
    names.extend(projects);

    let shown_store = match (store, home) {
        (Some(store), Some(home)) if !home.is_empty() => store.replacen(home, "~", 1),
        (Some(store), _) => store.to_owned(),
        (None, _) => String::new(),
    };
    let read_command = |name: &str| {
        let selector = if name.is_empty() {
            "-t agent-memory".to_owned()
        } else {
            format!("-t project:{name}")
        };
        if shown_store.is_empty() {
            format!("`zettel note list {selector}`")
        } else {
            format!("`zettel note list --root {shown_store} {selector}`")
        }
    };
    let heading = |name: &str| {
        if name.is_empty() {
            "Everywhere:".to_owned()
        } else {
            format!("Project {name}:")
        }
    };
    let ordered = |name: &str| -> Vec<&Note> {
        let mut list = groups.get(name).cloned().unwrap_or_default();
        list.sort_by(|a, b| (!a.is_processed(), a.stamp()).cmp(&(!b.is_processed(), b.stamp())));
        list
    };
    let short = |name: &str| -> Vec<String> {
        let count = groups.get(name).map_or(0, Vec::len);
        vec![format!(
            "{} {count} memories; read them with {}",
            heading(name),
            read_command(name)
        )]
    };
    let full = |name: &str| -> Vec<String> {
        let mut block = vec![heading(name)];
        block.extend(ordered(name).iter().map(|note| note.line()));
        block
    };

    // One block per name, in the same order as `names`.
    let mut blocks: Vec<Vec<String>> = names.iter().map(|name| short(name)).collect();
    let mut size = TITLE.len() + 1 + blocks.iter().map(|block| cost(block)).sum::<usize>();
    for (name, block) in names.iter().zip(blocks.iter_mut()) {
        let grown = full(name);
        let extra = cost(&grown).saturating_sub(cost(block));
        if size + extra <= CAP_BYTES {
            *block = grown;
            size += extra;
        }
    }

    for (name, block) in names.iter().zip(blocks.iter_mut()) {
        if *block != short(name) {
            continue;
        }
        let list = ordered(name);
        let mut shown: Vec<String> = Vec::new();
        for (index, note) in list.iter().enumerate() {
            let rest = list.len() - index - 1;
            let mut trial = vec![heading(name)];
            trial.extend(shown.iter().cloned());
            trial.push(note.line());
            if rest > 0 {
                trial.push(format!(
                    "- and {rest} more; read them with {}",
                    read_command(name)
                ));
            }
            if size - cost(block) + cost(&trial) > CAP_BYTES {
                break;
            }
            shown.push(note.line());
        }
        if !shown.is_empty() {
            let left = list.len() - shown.len();
            let mut grown = vec![heading(name)];
            grown.extend(shown);
            if left > 0 {
                grown.push(format!(
                    "- and {left} more; read them with {}",
                    read_command(name)
                ));
            }
            size = size - cost(block) + cost(&grown);
            *block = grown;
        }
    }

    let mut lines = vec![TITLE.to_owned()];
    lines.extend(blocks.into_iter().flatten());
    if notes.is_empty() {
        lines.push("None yet.".to_owned());
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// List the notes through `zettel` on `PATH` and build the block. A store
/// that does not exist yet gives an empty block, not a failure.
pub fn generate(root: Option<&Path>) -> Result<String, String> {
    let home = crate::store::user_home().map(|home| home.display().to_string());
    let store = root.map(|root| root.display().to_string());
    if let Some(root) = root
        && !root.join("zettel.yml").is_file()
    {
        return Ok(block(&[], store.as_deref(), home.as_deref()));
    }
    let mut command = std::process::Command::new("zettel");
    command.args(["note", "list", "--format", "json"]);
    if let Some(root) = root {
        command.arg("--root").arg(root);
    }
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run zettel: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`zettel note list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let notes: Vec<Note> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("zettel printed notes kersh cannot read: {error}"))?;
    Ok(block(&notes, store.as_deref(), home.as_deref()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn note(title: &str, tags: &[&str], created: &str, updated: Option<&str>) -> Note {
        Note {
            title: title.to_owned(),
            body: format!("{title} body\nsecond line"),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            created: Some(created.to_owned()),
            updated: updated.map(str::to_owned),
        }
    }

    #[test]
    fn groups_come_everywhere_first_then_newest_project_with_processed_first() {
        let notes = vec![
            note(
                "Old raw",
                &["agent-memory", "project:alpha"],
                "2026-01-01T00:00:00Z",
                None,
            ),
            note("Global", &["agent-memory"], "2026-02-01T00:00:00Z", None),
            note(
                "Merged",
                &["agent-memory-processed", "project:alpha"],
                "2026-01-05T00:00:00Z",
                None,
            ),
            note(
                "Beta fact",
                &["agent-memory", "project:beta"],
                "2026-03-01T00:00:00Z",
                None,
            ),
            note(
                "Not a memory",
                &["commitment"],
                "2026-03-01T00:00:00Z",
                None,
            ),
        ];
        let text = block(&notes, Some("/home/u/store"), Some("/home/u"));
        let expected = "\
Agent memories (agent claims; see the agent memory section above):
Everywhere:
- (2026-02-01) Global. Global body
Project beta:
- (2026-03-01) Beta fact. Beta fact body
Project alpha:
- (2026-01-05) Merged. Merged body
- (2026-01-01) Old raw. Old raw body
";
        assert_eq!(text, expected);
    }

    #[test]
    fn no_memories_says_none_yet() {
        assert_eq!(
            block(&[], None, None),
            "Agent memories (agent claims; see the agent memory section above):\nNone yet.\n"
        );
    }

    #[test]
    fn the_cap_holds_and_a_cut_group_ends_with_its_count_and_read_command() {
        let notes: Vec<Note> = (0..200)
            .map(|i| {
                note(
                    &format!("Fact number {i:03} with some words to take up room"),
                    &["agent-memory", "project:big"],
                    &format!("2026-01-01T00:{:02}:{:02}Z", i / 60, i % 60),
                    None,
                )
            })
            .collect();
        let text = block(&notes, Some("/home/u/store"), Some("/home/u"));
        assert!(text.len() <= CAP_BYTES + 1, "{} bytes", text.len());
        let last = text.lines().last().unwrap();
        assert!(
            last.starts_with("- and ")
                && last.contains("`zettel note list --root ~/store -t project:big`"),
            "{last}"
        );
        assert!(text.contains("Project big:\n- (2026-01-01) Fact number 000"));
    }

    #[test]
    fn a_group_that_cannot_fit_one_memory_stays_a_count_line() {
        let long = "x".repeat(5_000);
        let notes = vec![note(&long, &["agent-memory"], "2026-01-01", None)];
        let text = block(&notes, None, None);
        assert!(
            text.contains(
                "Everywhere: 1 memories; read them with `zettel note list -t agent-memory`"
            ),
            "{text}"
        );
    }

    #[test]
    fn double_dots_collapse() {
        let n = note("Ends with a dot.", &["agent-memory"], "", None);
        assert_eq!(n.line(), "- Ends with a dot. Ends with a dot. body");
    }
}
