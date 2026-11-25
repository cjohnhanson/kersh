//! Sessions end to end: a real session server on a temp socket, the host
//! started by the first write with `service_manager: none`, and fake
//! `claude` and `codex` scripts on the login `PATH`. No real harness runs.
//!
//! Each test gets its own short state directory under `/tmp` (a Unix
//! socket path has a length limit), a temp `HOME` whose `.profile` builds
//! the login `PATH`, and a caller environment carrying
//! `CLAUDE_CODE_CHILD_SESSION`, which must never reach a session.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const CLAUDE_STREAM: &str = include_str!("fixtures/claude-stream.jsonl");
const CODEX_STREAM: &str = include_str!("fixtures/codex-stream.jsonl");

/// A fake Claude Code. Headless (`-p`), it reads the prompt on stdin:
/// `stream` replays the canned stream, `hang` prints init and sleeps,
/// `fail` exits 4. Interactive, the last argument decides: `exit0`,
/// `exit3`, or anything else to wait.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2.1.999 (Claude Code)"; exit 0; fi
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
printf '%s\n' "$@" > "$log.args"
env > "$log.env"
headless=0; sid=""; prev=""; last=""
for a in "$@"; do
  [ "$a" = "-p" ] && headless=1
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"; last="$a"
done
if [ "$headless" = 1 ]; then
  prompt=$(cat)
  printf '%s' "$prompt" > "$log.stdin"
  case "$prompt" in
    hang*) echo '{"type":"system","subtype":"init","model":"fake","session_id":"'"$sid"'"}'; exec sleep 600 ;;
    fail*) echo "boom" >&2; exit 4 ;;
  esac
  sed "s/SESSION_ID/$sid/g" "$HOME/claude-stream.jsonl"
  echo "a warning on stderr" >&2
  exit 0
fi
case "$last" in
  exit0) echo "fake claude says hi"; sleep 1; exit 0 ;;
  exit3) echo "fake claude fails"; exit 3 ;;
  *) echo "fake claude waiting"; exec sleep 600 ;;
esac
"#;

/// A fake Codex. `exec` replays the canned stream; interactive, it holds
/// a rollout file open the way Codex's own process does, and waits.
const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli 0.999.0"; exit 0; fi
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
printf '%s\n' "$@" > "$log.args"
if [ "$1" = "exec" ]; then
  cat > "$log.stdin"
  cat "$HOME/codex-stream.jsonl"
  exit 0
fi
d="$HOME/.codex/sessions/2026/11/24"
mkdir -p "$d"
exec 3>>"$d/rollout-2026-11-24T10-00-00-0199bbbb-cccc-7ddd-8eee-ffff00001111.jsonl"
echo "fake codex waiting"
sleep 600
"#;

struct Env {
    root: tempfile::TempDir,
}

fn write_exe(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Env {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("ks")
            .tempdir_in("/tmp")
            .unwrap();
        let env = Self { root };
        for dir in [
            env.home(),
            env.bin(),
            env.work(),
            env.home().join(".config/kersh"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            env.home().join(".config/kersh/config.yml"),
            "service_manager: none\npane:\n  cols: 100\n  rows: 30\n",
        )
        .unwrap();
        std::fs::write(
            env.home().join(".profile"),
            format!(
                "export PATH=\"{}:/usr/bin:/bin:/usr/sbin:/sbin\"\n",
                env.bin().display()
            ),
        )
        .unwrap();
        std::fs::write(env.home().join("claude-stream.jsonl"), CLAUDE_STREAM).unwrap();
        std::fs::write(env.home().join("codex-stream.jsonl"), CODEX_STREAM).unwrap();
        write_exe(&env.bin().join("claude"), FAKE_CLAUDE);
        write_exe(&env.bin().join("codex"), FAKE_CODEX);
        env
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("h")
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("b")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("w")
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("s").join("kersh")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kersh"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("SHELL", "/bin/sh")
            .env("XDG_STATE_HOME", self.root.path().join("s"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            // A caller inside an agent session: none of this may reach a
            // session.
            .env("CLAUDE_CODE_CHILD_SESSION", "1")
            .env("CLAUDECODE", "1")
            .env("KERSH_CALLER_ONLY", "leak")
            .current_dir(self.work());
        command
    }

    fn kersh(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn kersh_in(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write as _;
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Start a session and return its id.
    fn start(&self, args: &[&str], prompt: &str) -> String {
        let mut all = vec!["start"];
        all.extend_from_slice(args);
        all.extend(["--prompt-file", "-"]);
        let output = self.kersh_in(&all, prompt);
        assert!(
            output.status.success(),
            "start {args:?} exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn record(&self, id: &str) -> Value {
        let output = self.kersh(&["ps", id, "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Poll the record until `check` holds.
    fn wait(&self, id: &str, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let record = self.record(id);
            if check(&record) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {record:#}\nhost log:\n{}",
                std::fs::read_to_string(self.state().join("host.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_state(&self, id: &str, state: &str) -> Value {
        self.wait(id, state, |record| record["state"] == state)
    }

    /// The fake harness's files for a session: args, env, stdin.
    fn fake_log(&self, id: &str, suffix: &str) -> String {
        let dir = self.home().join("fakelog");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut found: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        let name = path.file_name().unwrap().to_string_lossy().into_owned();
                        name.starts_with(&format!("{id}.")) && name.ends_with(suffix)
                    })
                    .collect();
                found.sort();
                if let Some(path) = found.last()
                    && let Ok(text) = std::fs::read_to_string(path)
                    && !text.is_empty()
                {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "no {suffix} for {id}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid_from(path: &Path) -> Option<i32> {
        let text = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        i32::try_from(value["pid"].as_i64()?).ok()
    }

    fn stop_host(&self) {
        if let Some(pid) = Self::pid_from(&self.state().join("host.lock")) {
            kill(pid, "TERM");
            let deadline = Instant::now() + Duration::from_secs(10);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// Type raw bytes into a session's pane, as a person at its terminal.
    fn type_into(&self, id: &str, text: &str) {
        let record = self.record(id);
        let name = record["rmux_session"].as_str().unwrap().to_owned();
        let socket = self.state().join("mux.sock");
        let text = text.to_owned();
        kersh::mux::block_on(async move {
            use kersh::sessions::Sessions as _;
            let mux = kersh::mux::Rmux::connect(&socket).await.unwrap();
            mux.send_text(&name, &text).await.unwrap();
        })
        .unwrap();
    }
}

fn kill(pid: i32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status();
}

fn alive(pid: i32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_host();
        if let Some(pid) = Self::pid_from(&self.state().join("mux.lock")) {
            kill(pid, "TERM");
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if alive(pid) {
                kill(pid, "KILL");
            }
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn events(env: &Env, id: &str, since: u64) -> Vec<Value> {
    let output = env.kersh(&["events", id, "--since", &since.to_string()]);
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_headless_claude_run_replays_its_stream_as_numbered_events_and_ends() {
    let env = Env::new();
    let id = env.start(
        &[
            "--harness",
            "claude-code",
            "--mode",
            "headless",
            "--label",
            "card=tk-1 two=a b=c",
            "--label",
            "note= ünï ",
        ],
        "stream",
    );
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["exit_status"], 0);
    assert_eq!(
        record["labels"]["card"], "tk-1 two=a b=c",
        "labels are echoed untouched"
    );
    assert_eq!(record["labels"]["note"], " ünï ");
    assert_eq!(record["build"]["version"], "2.1.999 (Claude Code)");
    assert_eq!(record["build"]["installer"], "path");
    assert_eq!(record["conversation"]["source"], "launch");

    let all = events(&env, &id, 0);
    let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(
        seqs,
        (1..=u64::try_from(all.len()).unwrap()).collect::<Vec<_>>()
    );
    let kinds: Vec<&str> = all.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let init = kinds.iter().position(|k| *k == "init").unwrap();
    assert!(init >= 2, "hook events come before init: {kinds:?}");
    let tool = all.iter().find(|e| e["kind"] == "tool").unwrap();
    assert_eq!(tool["summary"], "tool Bash: cargo test");
    let result = all.iter().find(|e| e["kind"] == "result").unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["cost_usd"], 0.41);
    assert!(
        all.iter()
            .any(|e| e["kind"] == "stderr" && e["raw"] == "a warning on stderr"),
        "{kinds:?}"
    );
    assert_eq!(events(&env, &id, 7).len(), all.len() - 7);
    assert!(
        record["progress"].as_str().is_some(),
        "ps carries the last progress line"
    );

    // The harness got the prompt on stdin and kersh's own flags.
    assert_eq!(env.fake_log(&id, ".stdin"), "stream");
    let args = env.fake_log(&id, ".args");
    let args: Vec<&str> = args.lines().collect();
    assert!(args.contains(&"-p"));
    assert!(
        args.windows(2)
            .any(|w| w == ["--settings", r#"{"autoMemoryEnabled": false}"#])
    );
    let uuid = record["conversation"]["id"].as_str().unwrap();
    assert!(args.windows(2).any(|w| w == ["--session-id", uuid]));
    assert!(!args.contains(&"--bare"));

    // The session started from the login environment, not the caller's.
    let harness_env = env.fake_log(&id, ".env");
    assert!(
        !harness_env.contains("CLAUDE_CODE_CHILD_SESSION"),
        "{harness_env}"
    );
    assert!(!harness_env.contains("CLAUDECODE="), "{harness_env}");
    assert!(!harness_env.contains("KERSH_CALLER_ONLY"), "{harness_env}");
    assert!(harness_env.contains(&format!("KERSH_SESSION={id}")));
    assert!(harness_env.contains("COLORTERM=truecolor"));
    assert!(harness_env.contains("COLORFGBG=15;0"));
    assert!(harness_env.contains(&format!("PATH={}", env.bin().display())));

    // Reads work with the host stopped.
    env.stop_host();
    assert_eq!(env.record(&id)["state"], "ended");
    assert_eq!(events(&env, &id, 0).len(), all.len());
}

#[test]
fn a_headless_start_without_a_prompt_is_a_usage_error() {
    let env = Env::new();
    let output = env.kersh(&["start", "--harness", "claude-code", "--mode", "headless"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("needs a prompt"));
}

#[test]
fn a_headless_failure_holds_and_resume_continues_the_conversation() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code", "--mode", "headless"], "fail");
    let record = env.wait_state(&id, "held");
    assert_eq!(record["exit_status"], 4);
    assert_eq!(record["reason"], "exit status 4");

    let output = env.kersh(&["resume", &id]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a headless resume needs a prompt"
    );

    let output = env.kersh_in(&["resume", &id, "--prompt-file", "-", "--json"], "stream");
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["launches"], 2);
    let args = env.fake_log(&id, ".args");
    let uuid = record["conversation"]["id"].as_str().unwrap();
    assert!(
        args.lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w == ["--resume", uuid]),
        "{args}"
    );
    let all = events(&env, &id, 0);
    let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(
        seqs,
        (1..=u64::try_from(all.len()).unwrap()).collect::<Vec<_>>(),
        "numbering continues across launches"
    );
}

#[test]
fn an_interrupt_typed_into_a_headless_pane_holds_it_as_interrupted() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code", "--mode", "headless"], "hang");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    // The init line has been read once an event exists.
    let deadline = Instant::now() + Duration::from_secs(20);
    while events(&env, &id, 0).is_empty() {
        assert!(Instant::now() < deadline, "no init event");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.type_into(&id, "\u{3}");
    let record = env.wait_state(&id, "held");
    assert_eq!(record["reason"], "interrupted");
    let screen = env.kersh(&["screen", &id]);
    assert!(
        String::from_utf8_lossy(&screen.stdout).contains("held: interrupted"),
        "{}",
        String::from_utf8_lossy(&screen.stdout)
    );
    let output = env.kersh(&["stop", &id]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(env.record(&id)["state"], "ended");
}

#[test]
fn an_interactive_exit_0_ends_the_session_and_closes_the_pane() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "exit0");
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["mode"], "interactive");
    assert_eq!(record["exit_status"], 0);
    let args = env.fake_log(&id, ".args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        &args[args.len() - 2..],
        ["--", "exit0"],
        "the prompt is the argument"
    );
    assert!(!args.contains(&"-p"));
    // The pane is gone.
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.kersh(&["screen", &id]).status.success() {
        assert!(Instant::now() < deadline, "the pane stayed open");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn an_interactive_failure_holds_the_pane_and_rm_waits_for_an_end() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "exit3");
    let record = env.wait_state(&id, "held");
    assert_eq!(record["exit_status"], 3);
    let screen = env.kersh(&["screen", &id, "--json"]);
    assert!(screen.status.success(), "{}", stderr(&screen));
    let screen: Value = serde_json::from_slice(&screen.stdout).unwrap();
    assert_eq!(screen["cols"], 100, "the pane has the configured size");
    let text = screen["lines"].to_string();
    assert!(text.contains("held: exit status 3"), "{text}");

    let output = env.kersh(&["rm", &id]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a held session is not removed"
    );
    assert!(env.kersh(&["stop", &id]).status.success());
    let output = env.kersh(&["rm", &id]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!env.state().join("sessions").join(&id).exists());
}

#[test]
fn sigterm_to_the_wrapper_leaves_the_record_running_for_restore() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    let record = env.wait(&id, "the wrapper pid", |r| {
        r["wrapper_pid"].is_i64() && r["harness_pid"].is_i64()
    });
    let wrapper = i32::try_from(record["wrapper_pid"].as_i64().unwrap()).unwrap();
    let harness = i32::try_from(record["harness_pid"].as_i64().unwrap()).unwrap();
    kill(wrapper, "TERM");
    let record = env.wait(&id, "the signal reason", |r| {
        r["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("SIGTERM"))
    });
    assert_eq!(record["state"], "running");
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(harness) {
        assert!(
            Instant::now() < deadline,
            "the harness outlived its wrapper"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sighup_to_the_wrapper_leaves_the_record_running_too() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    let record = env.wait(&id, "the wrapper pid", |r| r["wrapper_pid"].is_i64());
    kill(
        i32::try_from(record["wrapper_pid"].as_i64().unwrap()).unwrap(),
        "HUP",
    );
    let record = env.wait(&id, "the signal reason", |r| {
        r["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("SIGHUP"))
    });
    assert_eq!(record["state"], "running");
}

#[test]
fn send_types_into_an_interactive_session_and_screen_reads_it_without_a_host() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let output = env.kersh_in(&["send", &id, "--prompt-file", "-"], "hello from send\n");
    assert!(output.status.success(), "{}", stderr(&output));
    env.stop_host();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = env.kersh(&["screen", &id]);
        assert!(screen.status.success(), "{}", stderr(&screen));
        let text = String::from_utf8_lossy(&screen.stdout).into_owned();
        if text.contains("hello from send") {
            assert!(text.contains("fake claude waiting"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "the text never showed: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // A write starts a new host.
    let output = env.kersh(&["stop", &id, "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["state"], "ended");
    assert_eq!(record["reason"], "stopped");

    let output = env.kersh(&["send", &id, "--prompt-file", "-"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "send needs a running session"
    );
}

#[test]
fn an_interactive_resume_passes_the_conversation_and_the_prompt() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    assert!(env.kersh(&["stop", &id]).status.success());
    let output = env.kersh_in(&["resume", &id, "--prompt-file", "-"], "exit0");
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["launches"], 2);
    assert_eq!(record["exit_status"], 0);
    let uuid = record["conversation"]["id"].as_str().unwrap();
    let args = env.fake_log(&id, ".args");
    let args: Vec<&str> = args.lines().collect();
    assert!(args.windows(2).any(|w| w == ["--resume", uuid]), "{args:?}");
    assert_eq!(&args[args.len() - 2..], ["--", "exit0"]);
}

#[test]
fn a_conversation_another_session_holds_is_refused() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    let record = env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let uuid = record["conversation"]["id"].as_str().unwrap().to_owned();
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--resume-conversation",
            &uuid,
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already holds"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_group_limit_lets_one_of_two_concurrent_starts_take_the_last_place() {
    let env = Env::new();
    let first = env.start(
        &[
            "--harness",
            "claude-code",
            "--group",
            "g",
            "--max-running",
            "2",
        ],
        "wait",
    );
    env.wait_state(&first, "running");
    let env = std::sync::Arc::new(env);
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let env = std::sync::Arc::clone(&env);
            std::thread::spawn(move || {
                env.kersh_in(
                    &[
                        "start",
                        "--harness",
                        "claude-code",
                        "--group",
                        "g",
                        "--max-running",
                        "2",
                        "--prompt-file",
                        "-",
                    ],
                    "wait",
                )
            })
        })
        .collect();
    let outputs: Vec<Output> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let mut codes: Vec<Option<i32>> = outputs.iter().map(|o| o.status.code()).collect();
    codes.sort();
    assert_eq!(
        codes,
        [Some(0), Some(3)],
        "{:?}",
        outputs.iter().map(stderr).collect::<Vec<_>>()
    );
    let refused = outputs.iter().find(|o| o.status.code() == Some(3)).unwrap();
    assert!(stderr(refused).contains("2 of 2"), "{}", stderr(refused));

    let groups: Value =
        serde_json::from_str(&std::fs::read_to_string(env.state().join("groups.json")).unwrap())
            .unwrap();
    assert_eq!(groups["g"], 2, "groups.json keeps the limit");

    // The limit holds for a start that names only the group, and frees
    // when a session ends.
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--group",
            "g",
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(env.kersh(&["stop", &first]).status.success());
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--group",
            "g",
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // A resume into the full group is gated too.
    let output = env.kersh(&["resume", &first]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
}

#[test]
fn a_record_of_a_newer_schema_is_refused() {
    let env = Env::new();
    let dir = env.state().join("sessions").join("future");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("record.json"), r#"{"schema": 99, "id": "future"}"#).unwrap();
    let output = env.kersh(&["ps", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("schema 99"), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
}

#[test]
fn codex_ids_come_from_its_stream_and_from_its_open_rollout_file() {
    let env = Env::new();
    let headless = env.start(&["--harness", "codex", "--mode", "headless"], "do it");
    let record = env.wait_state(&headless, "ended");
    assert_eq!(
        record["conversation"]["id"],
        "0199aaaa-1111-7222-8333-444455556666"
    );
    assert_eq!(record["conversation"]["source"], "stream");
    let kinds: Vec<String> = events(&env, &headless, 0)
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        ["init", "turn", "tool", "tool_result", "text", "result"]
    );
    let args = env.fake_log(&headless, ".args");
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(&args[..2], ["exec", "--json"]);
    assert_eq!(args.last(), Some(&"-"));
    assert!(args.windows(2).any(|w| w == ["--disable", "memories"]));

    let interactive = env.start(&["--harness", "codex"], "go");
    let record = env.wait(&interactive, "the rollout id", |r| {
        r["conversation"].is_object()
    });
    assert_eq!(
        record["conversation"]["id"],
        "0199bbbb-cccc-7ddd-8eee-ffff00001111"
    );
    assert_eq!(record["conversation"]["source"], "rollout");
}

#[test]
fn ps_follow_streams_each_record_change() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let mut follow = env
        .command(&["ps", "--follow", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = follow.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let first: Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(20)).unwrap()).unwrap();
    assert_eq!(first["id"], id.as_str());
    assert!(env.kersh(&["stop", &id]).status.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let line = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        let record: Value = serde_json::from_str(&line).unwrap();
        if record["state"] == "ended" {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    let _ = follow.kill();
    let _ = follow.wait();
}

#[test]
fn agent_and_runtime_files_are_written_by_kersh_and_drive_a_start() {
    let env = Env::new();
    let file = env.work().join("fixer.md");
    std::fs::write(
        &file,
        "---\nname: fixer\ndescription: Fixes a test.\nruntime: worker\n---\nYou fix one failing test.\n",
    )
    .unwrap();
    let output = env.kersh(&["agent", "add", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let written = env.home().join(".config/kersh/agents/fixer/AGENT.md");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        written.display().to_string()
    );

    let runtime = env.work().join("worker.yml");
    std::fs::write(
        &runtime,
        "harness: claude-code\nmode: headless\nmodel: opus\n",
    )
    .unwrap();
    let output = env.kersh(&["runtime", "add", "worker", runtime.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = env.kersh(&["runtime", "list", "--json"]);
    let line: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(line["name"], "worker");
    assert_eq!(line["harness"], "claude-code");

    std::fs::write(&runtime, "harness: claude-code\nflags: [--bare]\n").unwrap();
    let output = env.kersh(&["runtime", "add", "bad", runtime.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a hook-skipping flag is refused"
    );

    let id = env.start(&["fixer"], "stream");
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["agent"]["name"], "fixer");
    assert_eq!(record["runtime"]["name"], "worker");
    assert_eq!(record["runtime"]["settings"]["model"], "opus");
    let system = std::fs::read_to_string(record["system_prompt"].as_str().unwrap()).unwrap();
    assert_eq!(system, "You fix one failing test.");
    let args = env.fake_log(&id, ".args");
    assert!(
        args.lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w == ["--model", "opus"])
    );
}

#[test]
fn harness_list_reports_each_build_on_the_login_path() {
    let env = Env::new();
    let output = env.kersh(&["harness", "list", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["harness"], "claude-code");
    assert_eq!(lines[0]["version"], "2.1.999 (Claude Code)");
    assert_eq!(lines[1]["harness"], "codex");
    assert_eq!(lines[1]["version"], "codex-cli 0.999.0");
}

#[test]
fn memories_reads_notes_only_through_zettel() {
    let env = Env::new();
    let store = env.root.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("zettel.yml"), "").unwrap();
    let notes = r#"[
      {"title":"Use uv","body":"Run python through uv.","tags":["agent-memory","feedback"],"created":"2026-11-01T00:00:00Z"},
      {"title":"Bellows quirk","body":"Tests need a socket.","tags":["agent-memory-processed","project:bellows"],"created":"2026-11-02T00:00:00Z"},
      {"title":"A commitment","body":"x","tags":["commitment"],"created":"2026-11-03T00:00:00Z"}
    ]"#;
    std::fs::write(env.root.path().join("notes.json"), notes).unwrap();
    write_exe(
        &env.bin().join("zettel"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\ncat \"{}\"\n",
            env.root.path().join("zettel.args").display(),
            env.root.path().join("notes.json").display()
        ),
    );
    let output = env.kersh(&["memories", "--root", store.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Agent memories (agent claims; see the agent memory section above):\n\
         Everywhere:\n\
         - (2026-11-01) Use uv. Run python through uv.\n\
         Project bellows:\n\
         - (2026-11-02) Bellows quirk. Tests need a socket.\n"
    );
    let args = std::fs::read_to_string(env.root.path().join("zettel.args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "note",
            "list",
            "--format",
            "json",
            "--root",
            store.to_str().unwrap()
        ]
    );
}

#[test]
fn setup_runs_gaff_init_and_reports_what_is_missing() {
    let env = Env::new();
    write_exe(
        &env.bin().join("gaff"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\necho registered\n",
            env.root.path().join("gaff.args").display()
        ),
    );
    let output = env.kersh(&["setup", "claude-code", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "missing pieces are not success"
    );
    let items: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let init = items.iter().find(|i| i["piece"] == "gaff init").unwrap();
    assert_eq!(init["present"], true);
    let hooks = items
        .iter()
        .find(|i| i["piece"] == "user-level hooks")
        .unwrap();
    assert_eq!(hooks["present"], false);
    assert!(
        items
            .iter()
            .any(|i| i["piece"] == "section agent-memories" && i["present"] == false)
    );
    let args = std::fs::read_to_string(env.root.path().join("gaff.args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["init", "--host", "claude-code"]
    );
    // kersh wrote no harness settings itself.
    assert!(!env.home().join(".claude").exists());
}

#[test]
fn a_profiled_start_needs_gaff_to_know_the_profile_and_its_hooks() {
    let env = Env::new();
    write_exe(
        &env.bin().join("gaff"),
        "#!/bin/sh\nif [ \"$1 $2\" = \"profile list\" ]; then printf 'worker\\tagent or human\\n'; fi\n",
    );
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--profile",
            "nope",
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no profile `nope`"),
        "{}",
        stderr(&output)
    );
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--profile",
            "worker",
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("kersh setup claude-code"),
        "{}",
        stderr(&output)
    );

    std::fs::create_dir_all(env.home().join(".claude")).unwrap();
    std::fs::write(
        env.home().join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"gaff hook"}]}]}}"#,
    )
    .unwrap();
    let id = env.start(&["--harness", "claude-code", "--profile", "worker"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let harness_env = env.fake_log(&id, ".env");
    assert!(harness_env.contains("GAFF_PROFILE=worker"), "{harness_env}");
}
