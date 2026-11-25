//! Finding an interactive Codex session's conversation id.
//!
//! Codex chooses the id itself and prints it nowhere a TUI can be read.
//! Its own process holds the conversation's rollout file open
//! (`.../rollout-<time>-<uuid>.jsonl`), so the host lists the open files
//! of the harness process and its descendants and takes the uuid from the
//! file name. Only paths are read, never a file's contents.

use std::collections::BTreeSet;

/// The first program of `names` found in the usual system directories.
fn tool(names: &[&str]) -> Option<std::path::PathBuf> {
    names
        .iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

/// `pid` and every process descended from it.
fn descendants(pid: i32) -> BTreeSet<i32> {
    let mut all = BTreeSet::from([pid]);
    let Some(ps) = tool(&["/bin/ps", "/usr/bin/ps"]) else {
        return all;
    };
    let Ok(output) = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=,ppid="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return all;
    };
    let pairs: Vec<(i32, i32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    loop {
        let before = all.len();
        for (child, parent) in &pairs {
            if all.contains(parent) {
                all.insert(*child);
            }
        }
        if all.len() == before {
            return all;
        }
    }
}

/// Whether a process other than those in `except` names `id` in its
/// arguments.
#[must_use]
pub fn process_names(id: &str, except: &[i32]) -> bool {
    let Some(ps) = tool(&["/bin/ps", "/usr/bin/ps"]) else {
        return false;
    };
    let Ok(output) = std::process::Command::new(ps)
        .args(["-A", "-o", "pid=,command="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    let own = i32::try_from(std::process::id()).unwrap_or_default();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let line = line.trim_start();
        let (pid, command) = line.split_once(' ').unwrap_or((line, ""));
        let pid: i32 = pid.parse().unwrap_or_default();
        pid != own && !except.contains(&pid) && command.contains(id)
    })
}

/// The uuid in a rollout file name, if `path` names one.
#[must_use]
pub fn rollout_id(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let shaped = id.len() == 36
        && id.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    shaped.then(|| id.to_owned())
}

/// The conversation id of the Codex process `pid`, from the rollout file
/// it or a descendant holds open.
#[must_use]
pub fn codex_rollout(pid: i32) -> Option<String> {
    let lsof = tool(&["/usr/sbin/lsof", "/usr/bin/lsof", "/sbin/lsof"])?;
    let pids: Vec<String> = descendants(pid).iter().map(ToString::to_string).collect();
    let output = std::process::Command::new(lsof)
        .args(["-n", "-P", "-Fn", "-p", &pids.join(",")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .find_map(rollout_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uuid_comes_from_the_rollout_file_name() {
        assert_eq!(
            rollout_id(
                "/h/.codex/sessions/2026/11/21/rollout-2026-11-21T09-56-19-01a1fed2-a2b1-7920-bd65-ed031d9df9fb.jsonl"
            )
            .as_deref(),
            Some("01a1fed2-a2b1-7920-bd65-ed031d9df9fb")
        );
        assert_eq!(rollout_id("/h/notes.jsonl"), None);
        assert_eq!(rollout_id("/h/rollout-short.jsonl"), None);
    }
}
