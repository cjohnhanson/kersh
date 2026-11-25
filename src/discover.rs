//! Finding harness processes from the process table.
//!
//! Two questions are answered here, from `ps` and `lsof` output only:
//!
//! - Which conversation an interactive Codex session runs. Codex chooses
//!   the id itself and prints it nowhere a TUI can be read. Its own
//!   process holds the conversation's rollout file open
//!   (`.../rollout-<time>-<uuid>.jsonl`), so the host lists the open
//!   files of the harness process and its descendants and takes the uuid
//!   from the file name. Only paths are read, never a file's contents.
//! - Whether a harness process already runs a conversation. A process
//!   runs it when its program is the harness (the first word of its
//!   command line, or the second under an interpreter) and the id is one
//!   of its arguments, alone or after `=` (`--resume=<id>`). The session
//!   being relaunched is not another holder: its wrapper, everything
//!   below the wrapper, and the harness's process group are left out.
//!   When `ps` cannot be run the answer is an error, never "free".

use std::collections::BTreeSet;

/// The first program of `names` found in the usual system directories.
fn tool(names: &[&str]) -> Option<std::path::PathBuf> {
    names
        .iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

/// One line of the process table.
struct Process {
    pid: i32,
    ppid: i32,
    pgid: i32,
    command: String,
}

/// The system `ps`.
fn system_ps() -> Option<std::path::PathBuf> {
    tool(&["/bin/ps", "/usr/bin/ps"])
}

/// Every process, as the `ps` at `ps` lists them. `-ww` asks both BSD ps
/// and procps for the whole command line, never one cut to a width.
fn list(ps: &std::path::Path) -> Result<Vec<Process>, String> {
    let output = std::process::Command::new(ps)
        .args(["-A", "-ww", "-o", "pid=,ppid=,pgid=,command="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot list processes with {}: {error}", ps.display()))?;
    if !output.status.success() {
        return Err(format!(
            "cannot list processes with {}: it exited with {}",
            ps.display(),
            output.status
        ));
    }
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// Every process, or none when `ps` cannot be run; for discovery, where
/// nothing found is a safe answer.
fn processes() -> Vec<Process> {
    system_ps()
        .and_then(|ps| list(&ps).ok())
        .unwrap_or_default()
}

/// The process table in `text`, one `pid ppid pgid command` per line.
fn parse(text: &str) -> Vec<Process> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line;
            let mut number = || {
                let (word, tail) = rest
                    .trim_start()
                    .split_once(char::is_whitespace)
                    .unwrap_or((rest.trim_start(), ""));
                rest = tail;
                word.parse::<i32>().ok()
            };
            let mut process = Process {
                pid: number()?,
                ppid: number()?,
                pgid: number()?,
                command: String::new(),
            };
            rest.trim_start().clone_into(&mut process.command);
            Some(process)
        })
        .collect()
}

/// `roots` and every process in `table` descended from one of them.
fn tree(table: &[Process], roots: &[i32]) -> BTreeSet<i32> {
    let mut all: BTreeSet<i32> = roots.iter().copied().collect();
    loop {
        let before = all.len();
        for process in table {
            if all.contains(&process.ppid) {
                all.insert(process.pid);
            }
        }
        if all.len() == before {
            return all;
        }
    }
}

/// `pid` and every process descended from it.
fn descendants(pid: i32) -> BTreeSet<i32> {
    tree(&processes(), &[pid])
}

/// The processes a session's own launch runs, which never hold its
/// conversation against it.
#[derive(Debug, Default, Clone)]
pub struct Own {
    /// Processes left out with all their descendants: the wrapper and the
    /// harness.
    pub roots: Vec<i32>,
    /// The harness's process group.
    pub pgid: Option<i32>,
}

/// Whether the command line `command` (as `ps` prints it) is `program`
/// running with `id` among its arguments, alone or as the value of a
/// `--flag=<id>`. The program is the first word, or the second when an
/// interpreter runs it as a script (`node /usr/local/bin/claude`,
/// `/bin/sh /path/claude`). Any other process that only mentions the id,
/// such as `kersh start --resume-conversation <id>`, is not the harness.
#[must_use]
pub fn command_runs(command: &str, program: &str, id: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let is_program = |word: &&str| word.rsplit('/').next() == Some(program);
    let runs_program = words.first().is_some_and(is_program)
        || words
            .get(1)
            .is_some_and(|word| !word.starts_with('-') && is_program(word));
    let names_id = |word: &&str| {
        *word == id
            || (word.starts_with('-') && word.split_once('=').is_some_and(|(_, value)| value == id))
    };
    runs_program && words.iter().skip(1).any(names_id)
}

/// The pid of a `program` process that runs conversation `id`, leaving
/// out this process and the processes of `own`.
pub fn holder(program: &str, id: &str, own: &Own) -> Result<Option<i32>, String> {
    holder_in(system_ps().as_deref(), program, id, own)
}

/// [`holder`], from the process table the `ps` at `ps` lists.
fn holder_in(
    ps: Option<&std::path::Path>,
    program: &str,
    id: &str,
    own: &Own,
) -> Result<Option<i32>, String> {
    let ps = ps.ok_or("cannot list processes: no ps in /bin or /usr/bin")?;
    let table = list(ps)?;
    let this = i32::try_from(std::process::id()).unwrap_or_default();
    let excluded = tree(&table, &own.roots);
    Ok(table
        .iter()
        .find(|process| {
            process.pid != this
                && !excluded.contains(&process.pid)
                && own.pgid != Some(process.pgid)
                && command_runs(&process.command, program, id)
        })
        .map(|process| process.pid))
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
#[allow(clippy::unwrap_used)]
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

    #[test]
    fn only_the_harness_itself_runs_a_conversation() {
        let id = "01a1fed2-a2b1-7920-bd65-ed031d9df9fb";
        assert!(command_runs(&format!("claude --resume {id}"), "claude", id));
        assert!(command_runs(
            &format!("/home/u/.local/bin/claude --resume {id}"),
            "claude",
            id
        ));
        assert!(command_runs(
            &format!("node /usr/local/bin/claude --resume {id}"),
            "claude",
            id
        ));
        assert!(!command_runs(
            &format!("/usr/local/bin/kersh start --harness claude-code --resume-conversation {id}"),
            "claude",
            id
        ));
        assert!(!command_runs(&format!("grep {id} notes"), "claude", id));
        assert!(!command_runs(&format!("codex resume {id}"), "claude", id));
        assert!(!command_runs("claude --resume other", "claude", id));
    }

    #[test]
    fn an_id_joined_to_its_flag_with_equals_still_counts() {
        let id = "01a1fed2-a2b1-7920-bd65-ed031d9df9fb";
        assert!(command_runs(&format!("claude --resume={id}"), "claude", id));
        assert!(command_runs(
            &format!("node /usr/local/bin/claude --session-id={id} hold"),
            "claude",
            id
        ));
        assert!(!command_runs(
            &format!("claude --resume={id}x"),
            "claude",
            id
        ));
    }

    /// A fake `ps` at `dir/ps` running `body`.
    fn fake_ps(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("ps");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_long_command_line_is_read_whole() {
        let id = "01a1fed2-a2b1-7920-bd65-ed031d9df9fb";
        let tmp = tempfile::tempdir().unwrap();
        // procps and BSD ps both cut the command column to the terminal
        // width unless asked for unlimited width with `-ww`.
        let ps = fake_ps(
            tmp.path(),
            &format!(
                "case \" $* \" in *' -ww '*) echo '41 1 41 claude --resume {id}' ;; \
                 *) echo '41 1 41 claude --resume' ;; esac"
            ),
        );
        assert_eq!(
            holder_in(Some(&ps), "claude", id, &Own::default()),
            Ok(Some(41))
        );
    }

    #[test]
    fn a_process_table_that_cannot_be_read_is_an_error() {
        let id = "01a1fed2-a2b1-7920-bd65-ed031d9df9fb";
        let tmp = tempfile::tempdir().unwrap();
        let failing = fake_ps(tmp.path(), "echo '41 1 41 claude --resume x'; exit 1");
        let missing = tmp.path().join("no-ps");
        for ps in [None, Some(missing.as_path()), Some(failing.as_path())] {
            let result = holder_in(ps, "claude", id, &Own::default());
            assert!(
                result
                    .as_ref()
                    .is_err_and(|error| error.contains("cannot list processes")),
                "{ps:?}: {result:?}"
            );
        }
    }

    #[test]
    fn the_table_parses_each_line_and_skips_the_rest() {
        let table =
            parse("  1     0     1 /sbin/launchd\n 52 1 52 claude --resume a b\nbad line\n");
        let rows: Vec<(i32, i32, i32, &str)> = table
            .iter()
            .map(|p| (p.pid, p.ppid, p.pgid, p.command.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (1, 0, 1, "/sbin/launchd"),
                (52, 1, 52, "claude --resume a b")
            ]
        );
    }
}
