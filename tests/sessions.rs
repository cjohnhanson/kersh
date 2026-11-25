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
/// `fail` exits 4, `spawn` starts a child and waits for it, and `spam`
/// exits 0 leaving a child that keeps printing. Interactive, the last
/// argument decides: `exit0`, `exit3`, `hold` to wait as itself, or
/// anything else to wait. Each log file is written whole to a temp name
/// and moved into place, so a test that polls for it never reads a part.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "2.1.999 (Claude Code)"; exit 0; fi
put() { cat > "$1.tmp" && mv "$1.tmp" "$1"; }
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
printf '%s\n' "$@" | put "$log.args"
env | put "$log.env"
headless=0; sid=""; prev=""; last=""
for a in "$@"; do
  [ "$a" = "-p" ] && headless=1
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"; last="$a"
done
if [ "$headless" = 1 ]; then
  prompt=$(cat)
  printf '%s' "$prompt" | put "$log.stdin"
  case "$prompt" in
    hang*) echo '{"type":"system","subtype":"init","model":"fake","session_id":"'"$sid"'"}'; exec sleep 600 ;;
    fail*) echo "boom" >&2; exit 4 ;;
    spawn*)
      sleep 600 &
      echo $! | put "$log.child"
      echo '{"type":"system","subtype":"init","model":"fake","session_id":"'"$sid"'"}'
      wait; exit 0 ;;
    spam*) ( while :; do echo tick; sleep 0.5; done ) & exit 0 ;;
  esac
  sed "s/SESSION_ID/$sid/g" "$HOME/claude-stream.jsonl"
  echo "a warning on stderr" >&2
  exit 0
fi
case "$last" in
  exit0) echo "fake claude says hi"; sleep 1; exit 0 ;;
  exit3) echo "fake claude fails"; exit 3 ;;
  hold) echo "fake claude holding"; sleep 600; exit 0 ;;
  *) echo "fake claude waiting"; exec sleep 600 ;;
esac
"#;

/// A fake Codex. `exec` replays the canned stream; interactive, it holds
/// a rollout file open the way Codex's own process does, and waits.
const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli 0.999.0"; exit 0; fi
put() { cat > "$1.tmp" && mv "$1.tmp" "$1"; }
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
printf '%s\n' "$@" | put "$log.args"
if [ "$1" = "exec" ]; then
  put "$log.stdin"
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

    /// The fake harness's files for a session's latest launch: args, env,
    /// stdin. The latest is the newest file, since pids do not grow in
    /// name order.
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
                found.sort_by_key(|path| {
                    std::fs::metadata(path)
                        .and_then(|meta| meta.modified())
                        .ok()
                });
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
    assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{id}\n"));
    assert!(!env.state().join("sessions").join(&id).exists());

    let other = env.start(&["--harness", "claude-code"], "exit0");
    env.wait_state(&other, "ended");
    let output = env.kersh(&["rm", &other, "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let removed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(removed, serde_json::json!({ "id": other }));
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
    // Left running, the session is restored by the host that watches it;
    // this fake keeps no transcript, so the restore holds it.
    let record = env.wait(&id, "the restore after SIGTERM", |r| {
        r["restores"][0]["after"]
            .as_str()
            .is_some_and(|after| after.contains("SIGTERM"))
    });
    assert_eq!(record["restores"][0]["outcome"], "held");
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
    let record = env.wait(&id, "the restore after SIGHUP", |r| {
        r["restores"][0]["after"]
            .as_str()
            .is_some_and(|after| after.contains("SIGHUP"))
    });
    assert_eq!(record["restores"][0]["outcome"], "held");
}

#[test]
fn send_types_into_an_interactive_session_and_screen_reads_it_without_a_host() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let output = env.kersh_in(&["send", &id, "--prompt-file", "-"], "hello from send\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("sent to {id}\n")
    );
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
fn a_free_conversation_resumes_and_one_a_harness_runs_is_refused() {
    let env = Env::new();
    let free = uuid::Uuid::new_v4().to_string();
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--resume-conversation",
            &free,
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let id = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let record = env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    assert_eq!(record["conversation"]["id"], free.as_str());
    assert_eq!(record["conversation"]["source"], "caller");

    // A harness outside kersh, on another conversation.
    let taken = uuid::Uuid::new_v4().to_string();
    let mut outside = {
        use std::os::unix::process::CommandExt as _;
        Command::new(env.bin().join("claude"))
            .args(["--resume", &taken, "hold"])
            .env_clear()
            .env("HOME", env.home())
            .env("PATH", "/usr/bin:/bin")
            .env("KERSH_SESSION", "outside")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    env.fake_log("outside", ".args");
    let output = env.kersh_in(
        &[
            "start",
            "--harness",
            "claude-code",
            "--resume-conversation",
            &taken,
            "--prompt-file",
            "-",
        ],
        "wait",
    );
    let group = i32::try_from(outside.id()).unwrap();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .status();
    let _ = outside.wait();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!(
            "another process (pid {}) already runs conversation {taken}; \
             stop that process or pick another conversation",
            outside.id()
        )),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_running_session_restarts_while_its_own_harness_runs_the_conversation() {
    let env = Env::new();
    // `hold` keeps the fake running as `claude`, as a real harness does.
    let id = env.start(&["--harness", "claude-code"], "hold");
    let first = env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    env.fake_log(&id, ".args");
    let output = env.kersh(&["restart", &id]);
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.wait(&id, "the second launch's harness", |r| {
        r["launches"] == 2 && r["harness_pid"].is_i64()
    });
    assert_eq!(record["state"], "running");
    assert_ne!(record["harness_pid"], first["harness_pid"]);
}

/// Rewrite fields of a session's record on disk, as a record left
/// behind by a crash would read.
fn edit_record(env: &Env, id: &str, edit: impl FnOnce(&mut Value)) {
    let path = env.state().join("sessions").join(id).join("record.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut record);
    std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
}

#[test]
fn a_conversation_another_session_s_harness_runs_is_refused_on_resume() {
    let env = Env::new();
    let holder = env.start(&["--harness", "claude-code"], "hold");
    let record = env.wait(&holder, "the harness pid", |r| r["harness_pid"].is_i64());
    let uuid = record["conversation"]["id"].as_str().unwrap().to_owned();
    env.fake_log(&holder, ".args");
    let other = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&other, "the harness pid", |r| r["harness_pid"].is_i64());
    assert!(env.kersh(&["stop", &other]).status.success());
    // The holder's record no longer claims the conversation, so only its
    // harness process can tell the resume that the conversation is taken.
    edit_record(&env, &holder, |r| r["state"] = "ended".into());
    edit_record(&env, &other, |r| {
        r["conversation"]["id"] = uuid.clone().into();
    });
    let output = env.kersh_in(&["resume", &other, "--prompt-file", "-"], "wait");
    edit_record(&env, &holder, |r| r["state"] = "running".into());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("already runs conversation {uuid}")),
        "{}",
        stderr(&output)
    );
}

/// A holder that runs `uuid` as `hold`, a second session whose record
/// names `uuid` and the holder's wrapper pid after `edit`, and the
/// holder's record set to ended so only its process can refuse. Returns
/// the result of `relaunch` on the second session.
fn relaunch_over_a_holder_s_old_pids(
    relaunch: &str,
    stop_other: bool,
    edit: impl FnOnce(&mut Value),
) -> (Output, String) {
    let env = Env::new();
    let holder = env.start(&["--harness", "claude-code"], "hold");
    let record = env.wait(&holder, "the wrapper pid", |r| {
        r["harness_pid"].is_i64() && r["wrapper_pid"].is_i64()
    });
    let uuid = record["conversation"]["id"].as_str().unwrap().to_owned();
    let holder_wrapper = record["wrapper_pid"].clone();
    env.fake_log(&holder, ".args");
    let other = env.start(&["--harness", "claude-code"], "hold");
    env.wait(&other, "the harness pid", |r| r["harness_pid"].is_i64());
    env.fake_log(&other, ".args");
    if stop_other {
        assert!(env.kersh(&["stop", &other]).status.success());
    }
    edit_record(&env, &holder, |r| r["state"] = "ended".into());
    // The host folds a wrapper's note into its record and would undo the
    // edit below; a note of an older launch changes nothing.
    let note = env.state().join("sessions").join(&other).join("note.json");
    if let Ok(text) = std::fs::read_to_string(&note) {
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["launch"] = 0.into();
        std::fs::write(&note, serde_json::to_string(&value).unwrap()).unwrap();
    }
    edit_record(&env, &other, |r| {
        r["conversation"]["id"] = uuid.clone().into();
        r["wrapper_pid"] = holder_wrapper;
        edit(r);
    });
    let output = if relaunch == "resume" {
        env.kersh_in(&["resume", &other, "--prompt-file", "-"], "wait")
    } else {
        env.kersh(&[relaunch, &other])
    };
    edit_record(&env, &holder, |r| r["state"] = "running".into());
    (output, uuid)
}

fn assert_refused_as_held(output: &Output, uuid: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    assert!(
        stderr(output).contains(&format!("already runs conversation {uuid}")),
        "{}",
        stderr(output)
    );
}

#[test]
fn an_ended_record_s_old_wrapper_pid_hides_no_holder() {
    let (output, uuid) = relaunch_over_a_holder_s_old_pids("resume", true, |_| ());
    assert_refused_as_held(&output, &uuid);
}

#[test]
fn a_running_record_from_another_boot_hides_no_holder() {
    let (output, uuid) = relaunch_over_a_holder_s_old_pids("restart", false, |r| {
        r["boot_id"] = "another-boot".into();
    });
    assert_refused_as_held(&output, &uuid);
}

#[test]
fn a_running_record_from_an_unknown_boot_hides_no_holder() {
    let (output, uuid) = relaunch_over_a_holder_s_old_pids("restart", false, |r| {
        r["boot_id"] = Value::Null;
    });
    assert_refused_as_held(&output, &uuid);
}

#[test]
fn sigterm_to_a_headless_wrapper_stops_the_harness_children_too() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code", "--mode", "headless"], "spawn");
    let record = env.wait(&id, "the pids", |r| {
        r["wrapper_pid"].is_i64() && r["harness_pid"].is_i64()
    });
    let child: i32 = env.fake_log(&id, ".child").trim().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while events(&env, &id, 0).is_empty() {
        assert!(Instant::now() < deadline, "no init event");
        std::thread::sleep(Duration::from_millis(50));
    }
    kill(
        i32::try_from(record["wrapper_pid"].as_i64().unwrap()).unwrap(),
        "TERM",
    );
    env.wait(&id, "the signal reason", |r| r["reason"].is_string());
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(child) {
        if Instant::now() > deadline {
            kill(child, "KILL");
            panic!("the harness's child outlived the stop");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_headless_harness_that_leaves_a_chatty_child_still_ends() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code", "--mode", "headless"], "spam");
    let record = env.wait_state(&id, "ended");
    assert_eq!(record["exit_status"], 0);
}

#[test]
fn an_agent_name_that_leaves_the_store_is_refused() {
    let env = Env::new();
    let file = env.work().join("evil.md");
    std::fs::write(&file, "---\nname: ../../x\n---\nBody.\n").unwrap();
    let output = env.kersh(&["agent", "add", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not a valid agent name"),
        "{}",
        stderr(&output)
    );
    assert!(!env.home().join(".config/x").exists());
}

#[test]
fn ps_with_a_state_no_session_is_in_says_so() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait_state(&id, "running");
    let output = env.kersh(&["ps", "--state", "held"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "no sessions in state held.\n"
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
fn many_concurrent_starts_take_exactly_the_free_places() {
    let env = std::sync::Arc::new(Env::new());
    let start = ["start", "--harness", "claude-code", "--group", "g"];
    let output = env.kersh_in(
        &[&start[..], &["--max-running", "3", "--prompt-file", "-"]].concat(),
        "wait",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let env = std::sync::Arc::clone(&env);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                env.kersh_in(&[&start[..], &["--prompt-file", "-"]].concat(), "wait")
                    .status
                    .code()
            })
        })
        .collect();
    let codes: Vec<Option<i32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let started = codes.iter().filter(|code| **code == Some(0)).count();
    let refused = codes.iter().filter(|code| **code == Some(3)).count();
    assert_eq!((started, refused), (2, 10), "{codes:?}");
}

#[test]
fn the_group_limit_counts_only_running_sessions_of_its_own_group() {
    let env = Env::new();
    let group = |name: &str, prompt: &str, extra: &[&str]| {
        let mut args = vec!["start", "--harness", "claude-code", "--group", name];
        args.extend_from_slice(extra);
        args.extend(["--prompt-file", "-"]);
        env.kersh_in(&args, prompt)
    };
    // A held session of the group, and running sessions outside it.
    let output = group("g", "exit3", &["--max-running", "1"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let held = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    env.wait_state(&held, "held");
    let ungrouped = env.start(&["--harness", "claude-code"], "wait");
    env.wait_state(&ungrouped, "running");
    let output = group("other", "wait", &[]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = group("g", "wait", &[]);
    assert!(
        output.status.success(),
        "neither a held session nor another group's takes g's place: {}",
        stderr(&output)
    );
    let running = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    env.wait(&running, "the harness pid", |r| r["harness_pid"].is_i64());
    assert_eq!(group("g", "wait", &[]).status.code(), Some(3));

    // A running session's restart keeps its own place; a held session's
    // restart needs a free one.
    let output = env.kersh(&["restart", &running]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = env.kersh(&["restart", &held]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("1 of 1"), "{}", stderr(&output));

    let output = group("a/b", "wait", &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not a valid group name"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_harness_killed_by_a_signal_kersh_did_not_send_is_restored_with_the_signal_as_cause() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    let record = env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    kill(
        i32::try_from(record["harness_pid"].as_i64().unwrap()).unwrap(),
        "KILL",
    );
    // The signal leaves the record running, and the host's restore then
    // takes it; the restore names the signal as the cause.
    let record = env.wait(&id, "a restore", |r| {
        r["restores"].as_array().is_some_and(|l| !l.is_empty())
    });
    assert_eq!(
        record["restores"][0]["after"],
        "its pane closed (killed by signal 9)"
    );
}

#[test]
fn a_harness_that_cannot_start_holds_the_pane_with_the_cause() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    assert!(env.kersh(&["stop", &id]).status.success());
    // The relaunch runs the recorded build, which is now gone.
    std::fs::remove_file(env.bin().join("claude")).unwrap();
    let output = env.kersh_in(&["resume", &id, "--prompt-file", "-"], "wait");
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.wait_state(&id, "held");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("could not start the harness: "),
        "{reason}"
    );
    assert_eq!(record["exit_status"], Value::Null);
}

#[test]
fn sigterm_to_a_headless_wrapper_holds_the_session_and_stops_the_harness() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code", "--mode", "headless"], "hang");
    let record = env.wait(&id, "the pids", |r| {
        r["wrapper_pid"].is_i64() && r["harness_pid"].is_i64()
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while events(&env, &id, 0).is_empty() {
        assert!(Instant::now() < deadline, "no init event");
        std::thread::sleep(Duration::from_millis(50));
    }
    let harness = i32::try_from(record["harness_pid"].as_i64().unwrap()).unwrap();
    kill(
        i32::try_from(record["wrapper_pid"].as_i64().unwrap()).unwrap(),
        "TERM",
    );
    let record = env.wait(&id, "a restore", |r| {
        r["restores"].as_array().is_some_and(|l| !l.is_empty())
    });
    assert_eq!(record["restores"][0]["after"], "its pane closed (SIGTERM)");
    assert_eq!(record["state"], "held", "a headless run is not resumed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(harness) {
        assert!(
            Instant::now() < deadline,
            "the harness outlived its wrapper"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Post one launch to the host's REST API with the schema header
/// `schema`, and parse its answer: the record, or the error.
fn post_launch(
    env: &Env,
    launch: &Value,
    schema: u32,
) -> Result<Value, kersh::session::SessionError> {
    let response = reqwest::blocking::Client::builder()
        .unix_socket(env.state().join("host.sock"))
        .build()
        .unwrap()
        .post("http://kersh/v1/launches")
        .header(kersh::proto::SCHEMA_HEADER, schema.to_string())
        .header(kersh::proto::VERSION_HEADER, "9.9.9")
        .json(launch)
        .send()
        .unwrap();
    let ok = response.status().is_success();
    let body: Value = response.json().unwrap();
    if ok {
        Ok(body)
    } else {
        Err(serde_json::from_value(body).unwrap())
    }
}

/// Post one launch at this build's schema.
fn ask_host(env: &Env, launch: &Value) -> Result<Value, kersh::session::SessionError> {
    post_launch(env, launch, kersh::proto::PROTOCOL_SCHEMA)
}

/// A launch as a client that skips the CLI would send it.
fn raw_start(
    env: &Env,
    flags: &[&str],
    group: Option<&str>,
    max: Option<u32>,
    settings_env: &Value,
) -> Value {
    serde_json::json!({
        "settings": {
            "harness": "claude-code",
            "mode": "interactive",
            "model": null,
            "environment": "local",
            "profile": null,
            "resume_prompt": null,
            "tempdir": false,
            "env": settings_env,
        },
        "flags": flags,
        "agent": null,
        "runtime_name": null,
        "system_text": null,
        "cwd": env.work(),
        "labels": {},
        "group": group,
        "max_running": max,
        "name": null,
        "prompt": "wait",
        "resume_conversation": null,
        "size": null,
    })
}

#[test]
fn the_host_refuses_a_reserved_session_env() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait_state(&id, "running");
    for key in [
        "CLAUDE_CODE_SIMPLE",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "ANTHROPIC_BASE_URL",
        "OPENAI_BASE_URL",
        "GAFF_HANDLERS",
        "GAFF_PROFILE",
        "KERSH_SESSION",
        "XDG_CONFIG_HOME",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BUN_OPTIONS",
        "NODE_OPTIONS",
        "HOME",
        "PATH",
        "SHELL",
    ] {
        let request = raw_start(&env, &[], None, None, &serde_json::json!({ key: "x" }));
        let error = ask_host(&env, &request).unwrap_err();
        assert!(
            matches!(&error, kersh::session::SessionError::Usage { message }
                if message.contains(&format!("{key} is reserved:"))
                    && message.ends_with("remove it from the runtime's env")),
            "{key}: {error:?}"
        );
    }
    let request = raw_start(
        &env,
        &[],
        None,
        None,
        &serde_json::json!({ "RUST_LOG": "debug" }),
    );
    let record = ask_host(&env, &request).unwrap();
    let id = record["id"].as_str().unwrap().to_owned();
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    assert!(env.fake_log(&id, ".env").contains("RUST_LOG=debug"));
}

#[test]
fn a_gaff_profile_in_the_login_environment_never_reaches_an_unprofiled_session() {
    let env = Env::new();
    let profile = env.home().join(".profile");
    let mut text = std::fs::read_to_string(&profile).unwrap();
    text.push_str("export GAFF_PROFILE=leaked\n");
    std::fs::write(&profile, text).unwrap();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait(&id, "the harness pid", |r| r["harness_pid"].is_i64());
    let harness_env = env.fake_log(&id, ".env");
    assert!(harness_env.contains(&format!("KERSH_SESSION={id}")));
    assert!(!harness_env.contains("GAFF_PROFILE"), "{harness_env}");
}

#[test]
fn a_runtime_may_not_change_the_path_a_profile_check_reads() {
    let env = Env::new();
    let runtime = env.work().join("nogaff.yml");
    std::fs::write(
        &runtime,
        "harness: claude-code\nprofile: worker\nenv:\n  PATH: /usr/bin:/bin\n",
    )
    .unwrap();
    let output = env.kersh(&["runtime", "add", "nogaff", runtime.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("PATH is reserved:"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_host_refuses_a_bad_request_from_a_client_that_skips_the_cli() {
    let env = Env::new();
    // Any write starts the host.
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.wait_state(&id, "running");

    let start = |flags: &[&str], group: Option<&str>, max: Option<u32>| {
        raw_start(&env, flags, group, max, &serde_json::json!({}))
    };
    // The request is well formed: with nothing wrong in it, it starts.
    let record = ask_host(&env, &start(&[], None, None)).unwrap();
    assert_eq!(record["state"], "running");

    let error = ask_host(&env, &start(&["--bare"], None, None)).unwrap_err();
    assert!(
        matches!(&error, kersh::session::SessionError::Usage { message } if message.contains("`--bare`")),
        "{error:?}"
    );
    let error = ask_host(&env, &start(&[], None, Some(1))).unwrap_err();
    assert!(
        matches!(&error, kersh::session::SessionError::Usage { message } if message.contains("needs --group")),
        "{error:?}"
    );
    let error = post_launch(&env, &start(&[], None, None), 99).unwrap_err();
    assert!(
        matches!(
            &error,
            kersh::session::SessionError::Schema { found: 99, .. }
        ),
        "{error:?}"
    );
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
