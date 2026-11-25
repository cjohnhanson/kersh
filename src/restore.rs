//! What restore needs to know without opening a harness's files: whether
//! a conversation's transcript or rollout exists (names only), whether an
//! old process group still has a process, and which ended records are old
//! enough to delete.
//!
//! The host decides and acts; these are its checks.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::paths::Paths;
use crate::session::State;

/// How long an ended record, its events, and its temp dir stay.
pub const KEEP_ENDED: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a restore waits for an old process group to empty.
pub const GROUP_WAIT: Duration = Duration::from_secs(10);

fn var<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn home_dir(env: &[(String, String)], fallback: &str) -> Option<PathBuf> {
    var(env, "HOME").map(|home| Path::new(home).join(fallback))
}

/// Claude Code's config directory in `env`: `CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
#[must_use]
pub fn claude_dir(env: &[(String, String)]) -> Option<PathBuf> {
    var(env, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir(env, ".claude"))
}

/// Codex's home in `env`: `CODEX_HOME`, else `~/.codex`.
#[must_use]
pub fn codex_dir(env: &[(String, String)]) -> Option<PathBuf> {
    var(env, "CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir(env, ".codex"))
}

/// The subdirectories of `dir`, by name.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether Claude Code keeps a transcript of conversation `id`:
/// `<claude dir>/projects/*/<id>.jsonl`. The file is never opened.
#[must_use]
pub fn claude_transcript_exists(env: &[(String, String)], id: &str) -> bool {
    let Some(dir) = claude_dir(env) else {
        return false;
    };
    let name = format!("{id}.jsonl");
    subdirs(&dir.join("projects"))
        .iter()
        .any(|project| project.join(&name).is_file())
}

/// Whether Codex keeps a rollout of conversation `id`:
/// `<codex home>/sessions/*/*/*/rollout-*-<id>.jsonl`. Names only.
#[must_use]
pub fn codex_rollout_exists(env: &[(String, String)], id: &str) -> bool {
    let Some(dir) = codex_dir(env) else {
        return false;
    };
    let suffix = format!("-{id}.jsonl");
    for year in subdirs(&dir.join("sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                if entries.flatten().any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("rollout-") && name.ends_with(&suffix)
                }) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether process group `pgid` still has a process.
#[must_use]
pub fn group_alive(pgid: i32) -> bool {
    rustix::process::Pid::from_raw(pgid)
        .is_some_and(|pid| rustix::process::test_kill_process_group(pid).is_ok())
}

/// The time since the Unix epoch of an RFC 3339 UTC time as kersh writes
/// it (`2026-11-24T10:20:30.500Z`).
#[must_use]
pub fn parse_rfc3339(text: &str) -> Option<Duration> {
    let num = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    if text.get(4..5)? != "-" || text.get(10..11)? != "T" {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let millis = match text.get(19..20) {
        Some(".") => text
            .get(20..)?
            .trim_end_matches('Z')
            .get(..3)
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let secs = u64::try_from(secs).ok()?;
    Some(Duration::from_secs(secs) + Duration::from_millis(millis))
}

/// Days since 1970-01-01 of a proleptic Gregorian date, by Howard
/// Hinnant's `days_from_civil` algorithm.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Delete every ended session that ended more than [`KEEP_ENDED`] before
/// `now`: its record, events, captures, and temp dir. Returns the ids.
/// Called by the host under its writer lock.
pub fn prune(paths: &Paths, now: SystemTime) -> Vec<String> {
    let Ok(records) = crate::session::read_records(&paths.sessions()) else {
        return Vec::new();
    };
    let now = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let mut pruned = Vec::new();
    for record in records {
        if record.state != State::Ended {
            continue;
        }
        let Some(ended) = record.ended_at.as_deref().and_then(parse_rfc3339) else {
            continue;
        };
        if now.saturating_sub(ended) > KEEP_ENDED
            && std::fs::remove_dir_all(paths.session(&record.id)).is_ok()
        {
            pruned.push(record.id);
        }
    }
    pruned
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn env(home: &Path) -> Vec<(String, String)> {
        vec![("HOME".to_owned(), home.display().to_string())]
    }

    #[test]
    fn rfc3339_round_trips_with_the_writer() {
        for millis in [0, 951_782_400_000, 1_795_515_630_500] {
            let at = Duration::from_millis(millis);
            assert_eq!(parse_rfc3339(&crate::util::rfc3339(at)), Some(at));
        }
        assert_eq!(parse_rfc3339("not a time"), None);
    }

    #[test]
    fn a_transcript_is_found_by_name_under_any_project() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        assert!(!claude_transcript_exists(&env, "u-1"));
        let project = tmp.path().join(".claude/projects/-Users-a-w");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("u-1.jsonl"), "").unwrap();
        assert!(claude_transcript_exists(&env, "u-1"));
        assert!(!claude_transcript_exists(&env, "u-2"));
        let other = tmp.path().join("cfg");
        let with_dir = vec![("CLAUDE_CONFIG_DIR".to_owned(), other.display().to_string())];
        assert!(!claude_transcript_exists(&with_dir, "u-1"));
    }

    #[test]
    fn a_rollout_is_found_by_name_three_levels_down() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        let id = "0199bbbb-cccc-7ddd-8eee-ffff00001111";
        assert!(!codex_rollout_exists(&env, id));
        let day = tmp.path().join(".codex/sessions/2026/11/24");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join(format!("rollout-2026-11-24T10-00-00-{id}.jsonl")),
            "",
        )
        .unwrap();
        assert!(codex_rollout_exists(&env, id));
        assert!(!codex_rollout_exists(
            &env,
            "0199bbbb-cccc-7ddd-8eee-000000000000"
        ));
    }

    #[test]
    fn prune_deletes_ended_sessions_thirty_days_after_they_end() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("state"), tmp.path().join("config"));
        let ended_at = Duration::from_millis(1_795_515_630_500);
        let write = |id: &str, state: &str| {
            let dir = paths.session(id);
            std::fs::create_dir_all(dir.join("tmp")).unwrap();
            std::fs::write(dir.join("events.jsonl"), "").unwrap();
            let mut record = sample_record(id);
            record["state"] = state.into();
            record["ended_at"] = crate::util::rfc3339(ended_at).into();
            std::fs::write(paths.record(id), record.to_string()).unwrap();
        };
        write("old", "ended");
        write("kept", "held");
        let day = Duration::from_secs(86_400);
        let at = |since: Duration| SystemTime::UNIX_EPOCH + ended_at + since;

        assert!(prune(&paths, at(day * 29)).is_empty());
        assert!(paths.session("old").exists());
        assert_eq!(prune(&paths, at(day * 31)), ["old"]);
        assert!(!paths.session("old").exists(), "record, events, and tmp go");
        assert!(paths.session("kept").exists(), "only ended records go");
    }

    fn sample_record(id: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": crate::session::RECORD_SCHEMA,
            "id": id, "name": null, "harness": "claude-code", "mode": "interactive",
            "state": "ended",
            "build": {"installer": "path", "path": "/bin/claude", "version": "1"},
            "environment": {"kind": "local", "cwd": "/", "tempdir": null},
            "agent": null,
            "runtime": {"name": null, "settings": {
                "harness": "claude-code", "mode": "interactive", "model": null,
                "environment": "local", "profile": null, "resume_prompt": null,
                "tempdir": false, "env": {}}},
            "system_prompt": null, "conversation": null, "agent_flags": [],
            "first_prompt": null, "labels": {}, "group": null, "pgid": null,
            "boot_id": null, "wrapper_pid": null, "harness_pid": null,
            "rmux_session": format!("kersh-{id}"),
            "pane_size": {"cols": 80, "rows": 24},
            "created_at": "2026-11-01T00:00:00.000Z", "ended_at": null,
            "exit_status": null, "reason": null, "launches": 1
        })
    }
}
