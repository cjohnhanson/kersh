//! The harness the restore suite shares: a temp state directory under
//! `/tmp` (a Unix socket path has a length limit), a temp `HOME` whose
//! `.profile` builds the login `PATH`, and fake `claude` and `codex`
//! scripts on it. No real harness runs.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const CLAUDE_STREAM: &str = include_str!("../fixtures/claude-stream.jsonl");
pub const CODEX_STREAM: &str = include_str!("../fixtures/codex-stream.jsonl");

/// A fake Claude Code. `--version` prints `$HOME/claude-version` when it
/// exists. Like the real one, it keeps a transcript named by its
/// conversation id under `$HOME/.claude/projects/`, unless the prompt
/// says `notranscript`. Headless (`-p`) it reads the prompt on stdin:
/// `hang` prints init and sleeps, otherwise it replays the canned stream.
/// Interactive, the last argument decides: `exit0`, `exit3`, `exitlater`
/// to exit 0 after four seconds, `survivor` to leave a child in its
/// process group that ignores HUP and TERM (its pid in
/// `$HOME/fakelog/<session>.survivor`) and wait, `hupsurvivor` to do the
/// same and ignore HUP itself, or anything else to wait. Each log file is
/// written whole to a temp name and moved into place, so a test that polls
/// for it never reads a part of it. The `.args` file comes last, after the
/// transcript and the survivor: once it exists, the launch has set up all
/// it leaves on disk.
pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  if [ -f "$HOME/claude-version" ]; then cat "$HOME/claude-version"; else echo "2.1.999 (Claude Code)"; fi
  exit 0
fi
put() { cat > "$1.tmp" && mv "$1.tmp" "$1"; }
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
env | put "$log.env"
headless=0; sid=""; prev=""; last=""
for a in "$@"; do
  [ "$a" = "-p" ] && headless=1
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"; last="$a"
done
transcript() {
  if [ -n "$sid" ]; then
    mkdir -p "$HOME/.claude/projects/-w"
    : >> "$HOME/.claude/projects/-w/$sid.jsonl"
  fi
}
args() { printf '%s\n' "$@" | put "$log.args"; }
if [ "$headless" = 1 ]; then
  prompt=$(cat)
  printf '%s' "$prompt" | put "$log.stdin"
  case "$prompt" in *notranscript*) ;; *) transcript ;; esac
  args "$@"
  case "$prompt" in
    hang*) echo '{"type":"system","subtype":"init","model":"fake","session_id":"'"$sid"'"}'; exec sleep 600 ;;
  esac
  sed "s/SESSION_ID/$sid/g" "$HOME/claude-stream.jsonl"
  exit 0
fi
case "$last" in *notranscript*) ;; *) transcript ;; esac
survivor() {
  ( trap '' HUP TERM; exec sleep 600 ) </dev/null >/dev/null 2>&1 &
  echo "$!" | put "$HOME/fakelog/$KERSH_SESSION.survivor"
}
case "$last" in
  survivor) survivor ;;
  hupsurvivor) survivor; trap '' HUP ;;
esac
args "$@"
case "$last" in
  exit0) echo "fake claude says hi"; sleep 1; exit 0 ;;
  exit3) echo "fake claude fails"; exit 3 ;;
  exitlater) echo "fake claude exits soon"; sleep 4; exit 0 ;;
  *) echo "fake claude waiting"; exec sleep 600 ;;
esac
"#;

/// A fake Codex. `exec` replays the canned stream; interactive, it holds
/// a rollout file open the way Codex's own process does, and waits.
pub const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "codex-cli 0.999.0"; exit 0; fi
put() { cat > "$1.tmp" && mv "$1.tmp" "$1"; }
mkdir -p "$HOME/fakelog"
log="$HOME/fakelog/$KERSH_SESSION.$$"
if [ "$1" = "exec" ]; then
  put "$log.stdin"
  printf '%s\n' "$@" | put "$log.args"
  cat "$HOME/codex-stream.jsonl"
  exit 0
fi
d="$HOME/.codex/sessions/2026/11/24"
mkdir -p "$d"
exec 3>>"$d/rollout-2026-11-24T10-00-00-0199bbbb-cccc-7ddd-8eee-ffff00001111.jsonl"
printf '%s\n' "$@" | put "$log.args"
echo "fake codex waiting"
exec sleep 600
"#;

/// The conversation the fake Codex reports.
pub const CODEX_CONVERSATION: &str = "0199bbbb-cccc-7ddd-8eee-ffff00001111";

pub struct Env {
    pub root: tempfile::TempDir,
    hosts: Mutex<Vec<Child>>,
}

pub fn write_exe(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

pub fn kill(pid: i32, signal: &str) {
    let _ = Command::new("/bin/kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

pub fn alive(pid: i32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

impl Env {
    /// A fresh environment; `config` is added to `config.yml`.
    pub fn with_config(config: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("kr")
            .tempdir_in("/tmp")
            .unwrap();
        let env = Self {
            root,
            hosts: Mutex::new(Vec::new()),
        };
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
            format!("service_manager: none\npane:\n  cols: 100\n  rows: 30\n{config}"),
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

    pub fn new() -> Self {
        Self::with_config("")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("h")
    }

    pub fn bin(&self) -> PathBuf {
        self.root.path().join("b")
    }

    pub fn work(&self) -> PathBuf {
        self.root.path().join("w")
    }

    pub fn state(&self) -> PathBuf {
        self.root.path().join("s").join("kersh")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kersh"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env("SHELL", "/bin/sh")
            .env("XDG_STATE_HOME", self.root.path().join("s"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .current_dir(self.work());
        command
    }

    pub fn kersh(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    pub fn kersh_in(&self, args: &[&str], input: &str) -> Output {
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
    pub fn start(&self, args: &[&str], prompt: &str) -> String {
        let mut all = vec!["start"];
        all.extend_from_slice(args);
        all.extend(["--prompt-file", "-"]);
        let output = self.kersh_in(&all, prompt);
        assert!(
            output.status.success(),
            "start {args:?} exited {:?}: {}",
            output.status.code(),
            stderr(&output)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub fn record(&self, id: &str) -> Value {
        let output = self.kersh(&["ps", id, "--json"]);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn host_log(&self) -> String {
        std::fs::read_to_string(self.state().join("host.log")).unwrap_or_default()
    }

    /// Poll the record until `check` holds.
    pub fn wait(&self, id: &str, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let record = self.record(id);
            if check(&record) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {record:#}\nhost log:\n{}",
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until the session has been restored `count` times and has
    /// settled in `state`.
    pub fn wait_restored(&self, id: &str, count: usize, state: &str) -> Value {
        self.wait(id, &format!("restore {count} to {state}"), |r| {
            r["restores"].as_array().map_or(0, Vec::len) == count && r["state"] == state
        })
    }

    /// Wait for a running harness that has logged its launch: the fake
    /// writes `.args` after its transcript, rollout, and survivor, so a
    /// crash after this finds them on disk.
    pub fn running_with_pid(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let record = self.wait(id, "a running harness", |r| {
                r["state"] == "running" && r["harness_pid"].is_i64()
            });
            let pid = record["harness_pid"].as_i64().unwrap();
            let args = self.home().join("fakelog").join(format!("{id}.{pid}.args"));
            if args.is_file() {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "harness {pid} of {id} never logged its args"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every argument list the fake harnesses logged for `id`, oldest
    /// first, with the pid that logged it.
    pub fn launches(&self, id: &str) -> Vec<(i32, Vec<String>)> {
        let dir = self.home().join("fakelog");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found: Vec<(std::time::SystemTime, i32, Vec<String>)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rest = name.strip_prefix(&format!("{id}."))?;
                let pid: i32 = rest.strip_suffix(".args")?.parse().ok()?;
                let text = std::fs::read_to_string(entry.path()).ok()?;
                let time = entry.metadata().ok()?.modified().ok()?;
                Some((time, pid, text.lines().map(str::to_owned).collect()))
            })
            .collect();
        found.sort();
        found
            .into_iter()
            .map(|(_, pid, args)| (pid, args))
            .collect()
    }

    /// Wait for the `n`th logged launch of `id` (from 1) and return its
    /// arguments.
    pub fn launch_args(&self, id: &str, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let launches = self.launches(id);
            if let Some((_, args)) = launches.get(n - 1)
                && !args.is_empty()
            {
                return args.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no launch {n} of {id}: {launches:?}\nhost log:\n{}",
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Live fake-harness processes whose arguments name `conversation`.
    pub fn live_on(&self, conversation: &str) -> Vec<i32> {
        let dir = self.home().join("fakelog");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let pid: i32 = name
                    .strip_suffix(".args")?
                    .rsplit('.')
                    .next()?
                    .parse()
                    .ok()?;
                let text = std::fs::read_to_string(entry.path()).ok()?;
                (text.lines().any(|line| line == conversation) && alive(pid)).then_some(pid)
            })
            .collect()
    }

    pub fn pid_from(path: &Path) -> Option<i32> {
        let text = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        i32::try_from(value["pid"].as_i64()?).ok()
    }

    pub fn host_pid(&self) -> Option<i32> {
        Self::pid_from(&self.state().join("host.lock"))
    }

    pub fn mux_pid(&self) -> Option<i32> {
        Self::pid_from(&self.state().join("mux.lock"))
    }

    fn reap(&self) {
        if let Ok(mut hosts) = self.hosts.lock() {
            for child in hosts.iter_mut() {
                let _ = child.try_wait();
            }
        }
    }

    pub fn stop_host(&self) {
        if let Some(pid) = self.host_pid() {
            kill(pid, "TERM");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.reap();
                if !alive(pid) || Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = std::fs::remove_file(self.state().join("host.sock"));
    }

    /// `kill -9` the session server and wait for it to be gone.
    pub fn kill_mux_hard(&self) -> i32 {
        let pid = self.mux_pid().unwrap();
        kill(pid, "KILL");
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(Instant::now() < deadline, "the session server outlived -9");
            std::thread::sleep(Duration::from_millis(50));
        }
        pid
    }

    /// Start a host in the background, as the service would at login, and
    /// wait until it answers.
    pub fn start_host(&self) {
        std::fs::create_dir_all(self.state()).unwrap();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state().join("host.log"))
            .unwrap();
        let child = self
            .command(&["host"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        self.hosts.lock().unwrap().push(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.host_pid() == Some(pid)
                && std::os::unix::net::UnixStream::connect(self.state().join("host.sock")).is_ok()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the host did not start:\n{}",
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid of the child a `survivor` launch of `id` left, once written.
    pub fn survivor(&self, id: &str) -> i32 {
        let path = self.home().join("fakelog").join(format!("{id}.survivor"));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "no survivor for {id}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `kill -9` every child a `survivor` launch left.
    fn kill_survivors(&self) {
        let Ok(entries) = std::fs::read_dir(self.home().join("fakelog")) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "survivor")
                && let Some(pid) = std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
            {
                kill(pid, "KILL");
            }
        }
    }

    /// The session server's view of a session, read directly.
    fn rmux_name(&self, id: &str) -> String {
        self.record(id)["rmux_session"].as_str().unwrap().to_owned()
    }

    /// Type raw bytes into a session's pane, as a person at its terminal.
    pub fn type_into(&self, id: &str, text: &str) {
        let name = self.rmux_name(id);
        let socket = self.state().join("mux.sock");
        let text = text.to_owned();
        kersh::mux::block_on(async move {
            use kersh::sessions::Sessions as _;
            let mux = kersh::mux::Rmux::connect(&socket).await.unwrap();
            mux.send_text(&name, &text).await.unwrap();
        })
        .unwrap();
    }

    /// End one session in the session server, leaving the server up.
    pub fn kill_session(&self, id: &str) {
        let name = self.rmux_name(id);
        let socket = self.state().join("mux.sock");
        kersh::mux::block_on(async move {
            use kersh::sessions::Sessions as _;
            let mux = kersh::mux::Rmux::connect(&socket).await.unwrap();
            mux.kill(&name).await.unwrap();
        })
        .unwrap();
    }

    /// The pane's text with its line breaks taken out, once it contains
    /// `needle`: a long line wraps at the pane's width.
    pub fn wait_screen_flat(&self, id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self.kersh(&["screen", id]);
            let text = String::from_utf8_lossy(&screen.stdout).replace('\n', "");
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the screen never showed {needle:?}: {text}{}\nhost log:\n{}",
                stderr(&screen),
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The pane's text, once it contains `needle`.
    pub fn wait_screen(&self, id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self.kersh(&["screen", id]);
            let text = String::from_utf8_lossy(&screen.stdout).into_owned();
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the screen never showed {needle:?}: {text}{}\nhost log:\n{}",
                stderr(&screen),
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_survivors();
        self.stop_host();
        if let Some(pid) = self.mux_pid() {
            kill(pid, "TERM");
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if alive(pid) {
                kill(pid, "KILL");
            }
        }
        self.reap();
    }
}
