//! The wrapper: the process in every session pane (`kersh session run
//! <id>`, hidden), between the session server and the harness.
//!
//! The host writes `launch.json` (what to run); the wrapper writes only
//! `note.json` (its pids, then how the harness exited) and, in headless
//! mode, the events file. Exit cases:
//!
//! - status 0 ends the session and closes the pane, in both modes;
//! - another status, or an interrupt typed into a headless pane, holds the
//!   pane open and the record `held`;
//! - SIGTERM, SIGHUP, or a hang-up of the pty leaves the record `running`,
//!   for restore.
//!
//! Headless, the harness runs in a session of its own with no controlling
//! terminal (so a tool that opens `/dev/tty` fails at once instead of
//! blocking), its stdin from the prompt file, its stdout and stderr
//! through pipes the wrapper reads. Only the wrapper writes to the pty:
//! one rendered line per event.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt as _;
use tokio::signal::unix::{SignalKind, signal};

use crate::paths::Paths;
use crate::session::{Harness, Mode};

/// What the host tells the wrapper to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchFile {
    pub id: String,
    /// Which launch of the session this is, from 1.
    pub launch: u32,
    pub harness: Harness,
    pub mode: Mode,
    /// The entry argv of the binary, for the setsid helper.
    pub entry: Vec<String>,
    pub argv: Vec<String>,
    /// The harness's whole environment.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// The prompt file a headless harness reads on stdin.
    pub stdin: Option<PathBuf>,
}

/// How the harness ended, as the wrapper saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    /// Status 0: the session ends.
    Ended,
    /// A failure or an interrupt: the pane holds.
    Held,
    /// A signal or hang-up: the record stays running, for restore.
    Signal,
}

/// The exit part of a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub kind: ExitKind,
    pub status: Option<i32>,
    pub reason: String,
    pub at: String,
}

/// `note.json`, the wrapper's only file besides the events file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub launch: u32,
    pub wrapper_pid: i32,
    pub harness_pid: Option<i32>,
    pub pgid: Option<i32>,
    pub boot_id: Option<String>,
    pub exit: Option<Exit>,
}

fn write_note(path: &Path, note: &Note) {
    if let Err(error) = crate::paths::write_json(path, note) {
        eprintln!("kersh wrapper: cannot write {}: {error}", path.display());
    }
}

/// Write to the pane, ignoring a hung-up terminal.
fn say(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

fn pid_i32(pid: u32) -> i32 {
    i32::try_from(pid).unwrap_or(i32::MAX)
}

/// Run `argv` in a new session with no controlling terminal. The process
/// becomes the harness by `exec`, so its pid is the session's.
#[must_use]
pub fn setsid_exec(argv: &[String]) -> std::process::ExitCode {
    use std::os::unix::process::CommandExt as _;
    let Some((program, rest)) = argv.split_first() else {
        eprintln!("kersh: setsid-exec needs a program");
        return std::process::ExitCode::from(2);
    };
    if let Err(error) = rustix::process::setsid() {
        eprintln!("kersh: setsid failed: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let error = std::process::Command::new(program).args(rest).exec();
    eprintln!("kersh: cannot run `{program}`: {error}");
    std::process::ExitCode::from(127)
}

/// The wrapper's entry point.
#[must_use]
pub fn run(paths: &Paths, id: &str) -> std::process::ExitCode {
    let launch_path = paths.launch(id);
    let launch: LaunchFile = match std::fs::read_to_string(&launch_path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
    {
        Ok(launch) => launch,
        Err(error) => {
            say(&format!(
                "kersh: cannot read {}: {error}\r\n",
                launch_path.display()
            ));
            std::thread::sleep(Duration::from_secs(5));
            return std::process::ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            say(&format!("kersh: cannot start the wrapper: {error}\r\n"));
            return std::process::ExitCode::FAILURE;
        }
    };
    let wrapper = Wrapper {
        note_path: paths.note(id),
        events_path: paths.events(id),
        note: Note {
            launch: launch.launch,
            wrapper_pid: pid_i32(std::process::id()),
            harness_pid: None,
            pgid: None,
            boot_id: crate::service::boot_id(),
            exit: None,
        },
        launch,
    };
    runtime.block_on(wrapper.run())
}

struct Wrapper {
    launch: LaunchFile,
    note_path: PathBuf,
    events_path: PathBuf,
    note: Note,
}

/// What ended the harness, from the wrapper's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// It exited by itself.
    Exited,
    /// The wrapper forwarded an interrupt typed into the pane.
    Interrupted,
    /// The wrapper got SIGTERM or SIGHUP.
    Signalled(&'static str),
}

impl Wrapper {
    async fn run(mut self) -> std::process::ExitCode {
        write_note(&self.note_path, &self.note);
        let outcome = match self.launch.mode {
            Mode::Headless => self.headless().await,
            Mode::Interactive => self.interactive().await,
        };
        let (cause, status) = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.finish(
                    ExitKind::Held,
                    None,
                    format!("could not start the harness: {error}"),
                );
                return self.hold().await;
            }
        };
        let code = status.and_then(|status| status.code());
        let killed_by = std::os::unix::process::ExitStatusExt::signal(&status.unwrap_or_default());
        match cause {
            Cause::Signalled(name) => {
                self.finish(
                    ExitKind::Signal,
                    code,
                    format!("{name}; left running for restore"),
                );
                std::process::ExitCode::SUCCESS
            }
            Cause::Interrupted => {
                self.finish(ExitKind::Held, code, "interrupted".to_owned());
                self.hold().await
            }
            Cause::Exited if code == Some(0) => {
                self.finish(ExitKind::Ended, Some(0), "exit status 0".to_owned());
                std::process::ExitCode::SUCCESS
            }
            Cause::Exited => match (code, status.is_some()) {
                (Some(code), _) => {
                    self.finish(ExitKind::Held, Some(code), format!("exit status {code}"));
                    self.hold().await
                }
                (None, true) => {
                    // Killed by a signal the wrapper did not send: left
                    // for restore like a signal to the wrapper.
                    let reason = killed_by.map_or_else(
                        || "killed by a signal; left running for restore".to_owned(),
                        |sig| format!("killed by signal {sig}; left running for restore"),
                    );
                    self.finish(ExitKind::Signal, None, reason);
                    std::process::ExitCode::SUCCESS
                }
                (None, false) => {
                    self.finish(ExitKind::Held, None, "the harness vanished".to_owned());
                    self.hold().await
                }
            },
        }
    }

    fn finish(&mut self, kind: ExitKind, status: Option<i32>, reason: String) {
        self.note.exit = Some(Exit {
            kind,
            status,
            reason,
            at: crate::util::now_rfc3339(),
        });
        write_note(&self.note_path, &self.note);
    }

    /// Keep the pane open with the reason on screen until the session is
    /// stopped or resumed.
    async fn hold(&self) -> std::process::ExitCode {
        let reason = self
            .note
            .exit
            .as_ref()
            .map_or("held", |exit| exit.reason.as_str());
        let id = &self.launch.id;
        say(&format!(
            "\r\n[kersh] held: {reason}. `kersh resume {id}` continues it; `kersh stop {id}` ends it.\r\n"
        ));
        let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            return std::process::ExitCode::FAILURE;
        };
        // An interrupt in a held pane does nothing.
        let _int = signal(SignalKind::interrupt());
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        std::process::ExitCode::SUCCESS
    }

    fn command(&self, argv: &[String]) -> Result<tokio::process::Command, String> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| "the launch has no program".to_owned())?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(rest)
            .env_clear()
            .envs(self.launch.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.launch.cwd);
        // The pane's terminal type reaches an interactive harness.
        if self.launch.mode == Mode::Interactive
            && let Some(term) = std::env::var_os("TERM")
        {
            command.env("TERM", term);
        }
        Ok(command)
    }

    async fn interactive(&mut self) -> Result<(Cause, Option<std::process::ExitStatus>), String> {
        // C-c reaches the harness from the terminal; the wrapper ignores
        // it. The handler is installed before the spawn, and exec resets a
        // handled signal to its default, so the harness gets the default.
        let mut int = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
        let mut term = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
        let mut hup = signal(SignalKind::hangup()).map_err(|error| error.to_string())?;
        let mut child = self
            .command(&self.launch.argv.clone())?
            .spawn()
            .map_err(|error| error.to_string())?;
        let pid = child.id().map(pid_i32);
        self.note.harness_pid = pid;
        self.note.pgid = Some(rustix::process::getpgrp().as_raw_nonzero().get());
        write_note(&self.note_path, &self.note);
        loop {
            tokio::select! {
                status = child.wait() => {
                    return Ok((Cause::Exited, status.ok()));
                }
                _ = int.recv() => {}
                _ = term.recv() => {
                    return Ok((Cause::Signalled("SIGTERM"), stop_child(&mut child, pid, rustix::process::Signal::TERM, false).await));
                }
                _ = hup.recv() => {
                    return Ok((Cause::Signalled("SIGHUP"), stop_child(&mut child, pid, rustix::process::Signal::HUP, false).await));
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)] // One select loop over the pipes and the signals.
    async fn headless(&mut self) -> Result<(Cause, Option<std::process::ExitStatus>), String> {
        let mut writer =
            crate::events::Writer::open(&self.events_path).map_err(|error| error.to_string())?;
        let mut int = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
        let mut term = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
        let mut hup = signal(SignalKind::hangup()).map_err(|error| error.to_string())?;

        let mut argv = self.launch.entry.clone();
        argv.extend([
            "session".to_owned(),
            "setsid-exec".to_owned(),
            "--".to_owned(),
        ]);
        argv.extend(self.launch.argv.iter().cloned());
        let mut command = self.command(&argv)?;
        let stdin = match &self.launch.stdin {
            Some(path) => std::process::Stdio::from(
                std::fs::File::open(path)
                    .map_err(|error| format!("{}: {error}", path.display()))?,
            ),
            None => std::process::Stdio::null(),
        };
        command
            .stdin(stdin)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id().map(pid_i32);
        self.note.harness_pid = pid;
        self.note.pgid = pid;
        write_note(&self.note_path, &self.note);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(bool, String)>();
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send((false, line)).is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send((true, line)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let harness = self.launch.harness;
        let mut cause = Cause::Exited;
        let mut status = None;
        let mut open = true;
        // Set once, when the harness exits; a pipe that keeps talking does
        // not move it.
        let mut drain_until: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                line = rx.recv(), if open => {
                    match line {
                        Some((is_err, line)) => {
                            let parsed = if is_err {
                                crate::events::parse_stderr(&line)
                            } else {
                                crate::events::parse_stdout(harness, &line)
                            };
                            match writer.append(parsed) {
                                Ok(event) => say(&format!("{}\r\n", event.summary)),
                                Err(error) => say(&format!("kersh: cannot record an event: {error}\r\n")),
                            }
                        }
                        None => open = false,
                    }
                }
                done = child.wait(), if status.is_none() => {
                    status = Some(done.ok());
                    drain_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                }
                // A background process the harness left may hold its
                // pipes open; stop reading a short while after the exit.
                () = sleep_until(drain_until), if drain_until.is_some() && open => {
                    break;
                }
                _ = int.recv() => {
                    if cause == Cause::Exited {
                        cause = Cause::Interrupted;
                    }
                    if let Some(pgid) = pid {
                        signal_group(pgid, rustix::process::Signal::INT);
                    }
                }
                _ = term.recv() => {
                    cause = Cause::Signalled("SIGTERM");
                    status = Some(stop_child(&mut child, pid, rustix::process::Signal::TERM, true).await);
                    break;
                }
                _ = hup.recv() => {
                    cause = Cause::Signalled("SIGHUP");
                    status = Some(stop_child(&mut child, pid, rustix::process::Signal::HUP, true).await);
                    break;
                }
            }
            if !open && status.is_some() {
                break;
            }
        }
        Ok((cause, status.flatten()))
    }
}

/// Sleep until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn signal_group(pgid: i32, sig: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(pid, sig);
    }
}

/// Forward `sig` to the harness (its group when `group`), wait a few
/// seconds, then kill it.
async fn stop_child(
    child: &mut tokio::process::Child,
    pid: Option<i32>,
    sig: rustix::process::Signal,
    group: bool,
) -> Option<std::process::ExitStatus> {
    if let Some(raw) = pid
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = if group {
            rustix::process::kill_process_group(pid, sig)
        } else {
            rustix::process::kill_process(pid, sig)
        };
    }
    if let Ok(status) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        return status.ok();
    }
    if group && let Some(raw) = pid {
        signal_group(raw, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    child.wait().await.ok()
}
