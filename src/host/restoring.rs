//! Restore: bringing back sessions the session server lost.
//!
//! A session is lost when its pane is gone but its record still says
//! `running` (a signal or hang-up leaves it so) or `lost`. That happens
//! when the server dies, at a reboot, or when one session vanishes while
//! the server stays up. The host restores at its own start, when its
//! watcher finds the server gone, and when the watcher finds one pane
//! gone. Every restore runs under the writer lock:
//!
//! 1. fold the wrapper's exit note; an `ended` record is never restored;
//! 2. mark the record `lost`;
//! 3. with `restore: false`, stop there;
//! 4. wait for the old process group to empty (no wait after a reboot);
//! 5. check the build again, noting a change in the restore line;
//! 6. resume an interactive session whose transcript (Claude Code) or
//!    rollout (Codex) exists, with the runtime's `resume_prompt` as the
//!    prompt argument; hold everything else with its command ready.
//!
//! The pane comes back at its recorded size, in its directory, with its
//! environment, and with the last screen capture and the restore line
//! printed above the agent. Never two processes on one conversation: a
//! conversation another record or an outside process holds comes back
//! held.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use super::{Host, fold_note, read_note};
use crate::mux::Rmux;
use crate::session::{
    Harness, HeldRun, Mode, Record, Restore, RestoreOutcome, SessionError, State,
};
use crate::sessions::Sessions;
use crate::wrapper::Hold;

/// The cause named when the session server was found gone.
pub(super) const SERVER_STOPPED: &str = "the session server stopped";

/// How a session launches again.
pub(super) enum How {
    /// Continue the recorded conversation. The prompt is the argument of
    /// an interactive resume, or a headless run's stdin.
    Resume { prompt: Option<String> },
    /// The first launch again, with the first prompt.
    Fresh,
    /// Run nothing: show `message` and wait. RET runs `ret`, when set.
    Hold {
        message: String,
        ret: Option<HeldRun>,
    },
}

/// What a restore decided for one session.
enum Plan {
    Resume,
    Hold { why: String, ret: Option<HeldRun> },
}

/// One argument as a person would type it, shortened when long.
fn shown_arg(arg: &str) -> String {
    let short: String = if arg.chars().count() > 60 {
        let mut text: String = arg.chars().take(57).collect();
        text.push_str("...");
        text
    } else {
        arg.to_owned()
    };
    let plain = short
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+%".contains(c));
    if plain && !short.is_empty() {
        short
    } else {
        format!("'{}'", short.replace('\'', r"'\''"))
    }
}

fn shown_command(argv: &[String]) -> String {
    argv.iter()
        .enumerate()
        .map(|(i, arg)| {
            if i == 0 {
                std::path::Path::new(arg)
                    .file_name()
                    .map_or_else(|| arg.clone(), |name| name.to_string_lossy().into_owned())
            } else {
                shown_arg(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Host {
    /// Launch the record again in a fresh pane, as `how` says. Called
    /// under the writer lock, with the old pane already gone.
    pub(super) async fn launch_again(
        &self,
        mux: &impl Sessions,
        record: &mut Record,
        how: How,
        preface: Option<String>,
    ) -> Result<(), SessionError> {
        let launch = record.launches + 1;
        let first_text = record
            .first_prompt
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let (prompt_arg, resume, stdin, hold) = match how {
            How::Resume { prompt } => {
                let stdin = match (&prompt, record.mode) {
                    (Some(text), Mode::Headless) => {
                        let path = self
                            .paths
                            .session(&record.id)
                            .join(format!("prompt-{launch}.md"));
                        crate::paths::write_private(&path, text.as_bytes())
                            .map_err(|error| SessionError::failed(error.to_string()))?;
                        Some(path)
                    }
                    _ => None,
                };
                (prompt, true, stdin, None)
            }
            How::Fresh => (first_text, false, record.first_prompt.clone(), None),
            How::Hold { message, ret } => {
                record.held_run = ret;
                let prompt = match ret {
                    Some(HeldRun::Fresh) => first_text,
                    _ => record.runtime.settings.resume_prompt.clone(),
                };
                (
                    prompt,
                    ret == Some(HeldRun::Resume),
                    record.first_prompt.clone(),
                    Some(Hold {
                        message,
                        ret: ret.is_some(),
                    }),
                )
            }
        };
        record.launches = launch;
        record.wrapper_pid = None;
        record.harness_pid = None;
        record.exit_status = None;
        record.ended_at = None;
        if hold.is_some() {
            record.state = State::Held;
        } else {
            record.state = State::Running;
            record.reason = None;
            record.held_run = None;
            record.pgid = None;
            record.pgid_launch = None;
            record.boot_id = crate::service::boot_id();
        }
        let env = self.relaunch_env(record);
        self.write_launch(
            record,
            &env,
            prompt_arg.as_deref(),
            resume,
            stdin,
            preface,
            hold,
        )?;
        self.save(record)?;
        if let Err(error) = self.create_pane(mux, record).await {
            record.state = State::Held;
            record.reason = Some(format!("the session server refused the pane: {error}"));
            self.save(record)?;
            return Err(SessionError::failed(
                record.reason.clone().unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /// The last capture and the restore line, printed above the agent.
    fn preface(&self, record: &Record, line: &str) -> String {
        let mut text = std::fs::read_to_string(self.paths.capture(&record.id)).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = writeln!(text, "[kersh] {line}");
        text
    }
}

/// The argv the held command would run, for the pane to show.
fn held_argv(record: &Record, ret: HeldRun, entry: &[String]) -> Vec<String> {
    let system_text = record
        .system_prompt
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    let conversation = match (&record.conversation, ret) {
        (Some(conversation), HeldRun::Resume) => {
            crate::harness::Conversation::Resume(&conversation.id)
        }
        (Some(conversation), HeldRun::Fresh) => {
            crate::harness::Conversation::New(Some(&conversation.id))
        }
        (None, _) => crate::harness::Conversation::New(None),
    };
    let prompt = match ret {
        HeldRun::Fresh => record.first_prompt.as_ref().map(|_| "<first prompt>"),
        HeldRun::Resume => record.runtime.settings.resume_prompt.as_deref(),
    };
    crate::harness::argv(&crate::harness::LaunchSpec {
        harness: record.harness,
        mode: record.mode,
        program: &record.build.path,
        model: record.runtime.settings.model.as_deref(),
        system_prompt: record.system_prompt.as_deref(),
        system_text: system_text.as_deref(),
        flags: &record.agent_flags,
        conversation,
        prompt_arg: (record.mode == Mode::Interactive)
            .then_some(prompt)
            .flatten(),
        rig: super::rig_run(record, entry),
    })
}

/// The text of a held pane: why, what RET runs, and the commands.
fn hold_message(record: &Record, why: &str, ret: Option<HeldRun>, entry: &[String]) -> String {
    let id = &record.id;
    let mut text = format!("[kersh] held: {why}.\n");
    if let Some(ret) = ret {
        let command = shown_command(&held_argv(record, ret, entry));
        let what = match (ret, record.mode) {
            // A rig run keeps no conversation: RET runs it again.
            (HeldRun::Fresh, _) if record.harness == Harness::Rig => "RET runs it again".to_owned(),
            (HeldRun::Fresh, Mode::Headless) => "RET runs the first prompt again".to_owned(),
            (HeldRun::Fresh, Mode::Interactive) => {
                "RET runs the first prompt again in a new conversation".to_owned()
            }
            (HeldRun::Resume, _) => "RET resumes it".to_owned(),
        };
        let stdin = if record.mode == Mode::Headless {
            " < <first prompt>"
        } else {
            ""
        };
        let _ = writeln!(text, "[kersh] {what}: {command}{stdin}");
    }
    let _ = match record.mode {
        Mode::Headless if record.harness == Harness::Rig => {
            writeln!(text, "[kersh] `kersh stop {id}` ends it.")
        }
        Mode::Headless => writeln!(
            text,
            "[kersh] `kersh resume {id} --prompt-file -` continues it with a new prompt; \
                 `kersh stop {id}` ends it."
        ),
        Mode::Interactive => writeln!(
            text,
            "[kersh] `kersh resume {id}` resumes its conversation; `kersh stop {id}` ends it."
        ),
    };
    text
}

/// The record's process group, unless a reboot since then ended every
/// process of it.
fn recorded_group(record: &Record) -> Option<i32> {
    let boot = crate::service::boot_id();
    if record.boot_id.is_some() && boot.is_some() && record.boot_id != boot {
        return None;
    }
    record.pgid
}

/// The recorded group when the current launch reported it: a running
/// pane's, or a pane its wrapper held after a failure. Such a group
/// empties when the pane ends. A running record's group is always its
/// current launch's, since a launch into running clears the group; this
/// also covers a record written before the launch was recorded.
pub(super) fn current_group(record: &Record) -> Option<i32> {
    let pgid = recorded_group(record)?;
    (record.pgid_launch == Some(record.launches) || record.state == State::Running).then_some(pgid)
}

/// The recorded group when an earlier launch reported it, or no launch
/// is recorded: a pane a restore holds runs no harness and reports no
/// group, so a process in that group is the old session's.
pub(super) fn old_group(record: &Record) -> Option<i32> {
    let pgid = recorded_group(record)?;
    current_group(record).is_none().then_some(pgid)
}

/// Whether process group `pgid` empties within
/// [`crate::restore::GROUP_WAIT`].
pub(super) async fn group_empties(pgid: i32) -> bool {
    let deadline = Instant::now() + crate::restore::GROUP_WAIT;
    while crate::restore::group_alive(pgid) {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

/// The refusal while a process of the old session's group runs.
pub(super) fn old_group_runs(pgid: i32) -> SessionError {
    SessionError::failed(format!(
        "a process from the old session still runs in process group {pgid}; \
         end it first: kill -- -{pgid}"
    ))
}

/// The refusal when the current launch's group outlives its pane.
pub(super) fn group_outlives_pane(id: &str, pgid: i32) -> SessionError {
    SessionError::failed(format!(
        "session {id}'s pane ended, but a process of it still runs in process group {pgid}; \
         a restore holds the session until it ends: kill -- -{pgid}"
    ))
}

impl Host {
    /// Wait for the record's process group to empty, whichever launch
    /// reported it: the pane is gone, so any process left in it is the
    /// old session's. False when no process of it remains (the group is
    /// then cleared); true when one still runs after
    /// [`crate::restore::GROUP_WAIT`].
    async fn wait_old_group(&self, record: &mut Record) -> bool {
        if let Some(pgid) = recorded_group(record)
            && !group_empties(pgid).await
        {
            return true;
        }
        record.pgid = None;
        record.pgid_launch = None;
        false
    }

    /// Whether the harness has a turn on disk for the record's
    /// conversation: a Claude Code transcript or a Codex rollout, by name.
    fn turn_on_disk(&self, record: &Record) -> bool {
        let Some(conversation) = &record.conversation else {
            return false;
        };
        let env = self.relaunch_env(record);
        match record.harness {
            Harness::ClaudeCode => crate::restore::claude_transcript_exists(&env, &conversation.id),
            Harness::Codex => crate::restore::codex_rollout_exists(&env, &conversation.id),
            Harness::Rig => false,
        }
    }

    /// Restore every session whose pane is gone. `cause` names what took
    /// the panes down when it is known (the server stopped, a restart).
    /// Called under the writer lock.
    pub(super) async fn restore_locked(&self, mux: &Rmux, cause: Option<&str>) {
        let names: HashSet<String> = match mux.list().await {
            Ok(list) => list.into_iter().map(|info| info.name).collect(),
            Err(error) => {
                eprintln!("kersh host: cannot list sessions to restore: {error}");
                return;
            }
        };
        let records = self.background_records();
        let enabled = self.config.restore();
        let boot = crate::service::boot_id();
        // Every vanished session is marked lost before any comes back, so
        // a restore never takes a conversation for one that is still
        // recorded as running elsewhere.
        let mut lost = Vec::new();
        for mut record in records {
            if !matches!(record.state, State::Running | State::Lost | State::Held)
                || names.contains(&record.rmux_session)
            {
                continue;
            }
            let before = record.clone();
            if let Some(note) = read_note(&self.paths, &record.id) {
                fold_note(&mut record, note);
            }
            if record.state == State::Ended {
                self.save_or_log(&record);
                continue;
            }
            let rebooted = record.boot_id.is_some() && boot.is_some() && record.boot_id != boot;
            let earlier = record
                .reason
                .as_deref()
                .filter(|_| record.state == State::Lost)
                .and_then(|reason| reason.strip_prefix("lost after "))
                .map(str::to_owned);
            let after = if rebooted {
                "a reboot".to_owned()
            } else if let Some(earlier) = earlier {
                earlier
            } else if let Some(cause) = cause {
                cause.to_owned()
            } else {
                let why = record
                    .reason
                    .as_deref()
                    .and_then(|reason| reason.split(';').next())
                    .filter(|_| record.state == State::Running)
                    .map_or_else(String::new, |reason| format!(" ({reason})"));
                format!("its pane closed{why}")
            };
            if let Ok(mut attempted) = self.attempted.lock() {
                attempted.insert(record.id.clone(), Instant::now());
            }
            if record.state == State::Held {
                if !enabled {
                    if record != before {
                        self.save_or_log(&record);
                    }
                    continue;
                }
                // A held pane comes back held, so a failure waiting for a
                // person survives the server too.
                let why = record.reason.clone().unwrap_or_else(|| "held".to_owned());
                let ret = record.held_run;
                let message = hold_message(&record, &why, ret, &self.entry);
                let at = crate::util::now_rfc3339();
                let line = format!("held at {at} after {after}: {why}");
                record.restores.push(Restore {
                    at,
                    after: after.clone(),
                    outcome: RestoreOutcome::Held,
                    build_change: None,
                    line: line.clone(),
                });
                let preface = self.preface(&record, &line);
                if let Err(error) = self
                    .launch_again(mux, &mut record, How::Hold { message, ret }, Some(preface))
                    .await
                {
                    eprintln!("kersh host: cannot restore {}: {error}", record.id);
                }
                continue;
            }
            if record.state == State::Running {
                record.state = State::Lost;
                record.reason = Some(format!("lost after {after}"));
                self.save_or_log(&record);
            }
            if enabled {
                lost.push((record, after));
            }
        }
        for (mut record, after) in lost {
            if let Err(error) = self.restore_one(mux, &mut record, &after).await {
                eprintln!("kersh host: cannot restore {}: {error}", record.id);
            }
        }
    }

    /// Whether a lost session resumes or holds, and what RET then runs.
    fn decide(&self, record: &Record, old_alive: bool, build_error: Option<String>) -> Plan {
        let turn_ran = self.turn_on_disk(record);
        let interactive = record.mode == Mode::Interactive;
        let again = match (interactive, turn_ran) {
            (true, true) => Some(HeldRun::Resume),
            (_, false) => Some(HeldRun::Fresh),
            (false, true) => None,
        };
        let hold = |why: String| Plan::Hold { why, ret: again };
        if let Some(error) = build_error {
            return hold(format!("its harness does not run: {error}"));
        }
        if old_alive {
            return hold(format!(
                "a process from the old session may still be running (process group {}); \
                 RET is refused until it ends",
                record.pgid.unwrap_or_default()
            ));
        }
        if !interactive {
            return hold("a headless run is not resumed by itself".to_owned());
        }
        if !turn_ran {
            let what = match record.harness {
                Harness::Codex => "rollout",
                _ => "transcript",
            };
            return hold(format!(
                "no {what} of its conversation exists, so its first turn may not have run"
            ));
        }
        let conversation = record
            .conversation
            .as_ref()
            .map(|conversation| conversation.id.clone())
            .unwrap_or_default();
        // A restore skips a record that does not read, as every
        // background path does; the process table still guards the
        // conversation.
        let records = self.background_records();
        match super::conversation_free_among(&records, record.harness, &conversation, Some(record))
        {
            Ok(()) => Plan::Resume,
            Err(error) => Plan::Hold {
                why: error.to_string(),
                ret: Some(HeldRun::Resume),
            },
        }
    }

    /// Bring one lost session back: resumed, or held with its command.
    async fn restore_one(
        &self,
        mux: &Rmux,
        record: &mut Record,
        after: &str,
    ) -> Result<(), SessionError> {
        let old_alive = self.wait_old_group(record).await;
        let mut build_change = None;
        let build = match record.harness {
            Harness::Rig => Ok(crate::harness::rig_build(&self.entry)),
            other => crate::harness::recheck(other, &record.build, &self.login_env),
        };
        let build_error = match build {
            Ok(build) => {
                if build != record.build {
                    let describe = |build: &crate::session::Build| {
                        format!("{} ({})", build.version, build.path.display())
                    };
                    build_change = Some(format!(
                        "{} -> {}",
                        describe(&record.build),
                        describe(&build)
                    ));
                    record.build = build;
                }
                None
            }
            Err(error) => Some(error),
        };
        let plan = self.decide(record, old_alive, build_error);
        let at = crate::util::now_rfc3339();
        let build_note = build_change
            .as_ref()
            .map_or_else(String::new, |change| format!("; build changed: {change}"));
        let resume_prompt = record.runtime.settings.resume_prompt.clone();
        let (outcome, line, how) = match plan {
            Plan::Resume => {
                let waiting = if resume_prompt.is_none() {
                    "; waiting at its prompt"
                } else {
                    ""
                };
                (
                    RestoreOutcome::Resumed,
                    format!(
                        "restored at {at} after {after}; tools that were running did not finish\
                         {waiting}{build_note}"
                    ),
                    How::Resume {
                        prompt: resume_prompt,
                    },
                )
            }
            Plan::Hold { why, ret } => {
                let message = hold_message(record, &why, ret, &self.entry);
                record.reason = Some(why.clone());
                (
                    RestoreOutcome::Held,
                    format!("held at {at} after {after}: {why}{build_note}"),
                    How::Hold { message, ret },
                )
            }
        };
        record.restores.push(Restore {
            at,
            after: after.to_owned(),
            outcome,
            build_change,
            line: line.clone(),
        });
        let preface = self.preface(record, &line);
        self.launch_again(mux, record, how, Some(preface)).await
    }

    /// RET in a held pane: run what it holds, gated like a resume.
    pub(super) async fn held_return(&self, id: &str) -> Result<(), SessionError> {
        let mux = self.mux().await?;
        let _guard = self.writer.lock().await;
        let mut record = self.load(id)?;
        let Some(ret) = record.held_run.filter(|_| record.state == State::Held) else {
            let resume = match record.mode {
                Mode::Headless => format!("`kersh resume {id} --prompt-file -` continues it"),
                Mode::Interactive => format!("`kersh resume {id}` resumes it"),
            };
            let next = match record.state {
                State::Held => format!("{resume}; `kersh stop {id}` ends it"),
                State::Running => format!("`kersh stop {id}` ends it"),
                State::Lost | State::Ended => resume,
            };
            return Err(SessionError::failed(format!(
                "session {id} is {} with nothing for RET to run; {next}",
                record.state
            )));
        };
        // Only a pane a restore held offers RET, and its hold wrapper
        // reports no group, so a recorded group is an earlier launch's:
        // it is checked with the pane still up.
        if let Some(pgid) = old_group(&record)
            && crate::restore::group_alive(pgid)
        {
            return Err(old_group_runs(pgid));
        }
        record.pgid = None;
        record.pgid_launch = None;
        self.gate(record.group.as_deref(), None)?;
        if let Some(conversation) = &record.conversation {
            self.check_conversation_free(record.harness, &conversation.id, Some(&record))?;
        }
        let how = match ret {
            HeldRun::Fresh => How::Fresh,
            HeldRun::Resume => How::Resume {
                prompt: (record.mode == Mode::Interactive)
                    .then(|| record.runtime.settings.resume_prompt.clone())
                    .flatten(),
            },
        };
        self.end_pane(&mux, &record).await?;
        self.launch_again(&mux, &mut record, how, None).await
    }

    /// Stop the session server by its recorded pid, start a new one, and
    /// restore every session from the captures already on disk.
    pub(super) async fn restart_mux(&self) -> Result<Vec<Record>, SessionError> {
        let _guard = self.writer.lock().await;
        if !self.skew.load(std::sync::atomic::Ordering::SeqCst) {
            // A server this build can drive gets one last capture first.
            if let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await {
                for record in &self.background_records() {
                    self.capture_one(&mux, record).await;
                }
            }
        }
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || {
            crate::mux::stop_server(&paths, Duration::from_secs(15))
        })
        .await
        .map_err(|error| SessionError::failed(error.to_string()))?
        .map_err(SessionError::failed)?;
        self.skew.store(false, std::sync::atomic::Ordering::SeqCst);
        let (mux, _) = self.ensure_mux().await.map_err(SessionError::failed)?;
        self.restore_locked(&mux, Some("kersh restart --mux")).await;
        crate::session::read_records(&self.paths.sessions())
    }

    /// One watcher tick: through the held connection, see whether the
    /// server is gone or a session vanished, and restore.
    pub(super) async fn watch_server(&self, server: &mut Option<Rmux>) {
        if self.skew.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let listed = match server.as_ref() {
            Some(mux) => mux.list().await.ok(),
            None => None,
        };
        let names: HashSet<String> = if let Some(list) = listed {
            list.into_iter().map(|info| info.name).collect()
        } else {
            *server = None;
            if let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await {
                let Ok(list) = mux.list().await else {
                    return;
                };
                *server = Some(mux);
                list.into_iter().map(|info| info.name).collect()
            } else {
                let _guard = self.writer.lock().await;
                match self.ensure_mux().await {
                    Ok((mux, started)) => {
                        let cause = started.then_some(SERVER_STOPPED);
                        self.restore_locked(&mux, cause).await;
                        *server = Some(mux);
                    }
                    Err(error) => eprintln!("kersh host: {error}"),
                }
                return;
            }
        };
        let records = self.background_records();
        let enabled = self.config.restore();
        let due = |id: &str| {
            self.attempted.lock().is_ok_and(|attempted| {
                attempted
                    .get(id)
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
            })
        };
        let vanished = records.iter().any(|record| {
            !names.contains(&record.rmux_session)
                && match record.state {
                    State::Running => true,
                    State::Lost | State::Held => enabled && due(&record.id),
                    State::Ended => false,
                }
        });
        if !vanished {
            return;
        }
        let _guard = self.writer.lock().await;
        if let Some(mux) = server.as_ref() {
            self.restore_locked(mux, None).await;
        }
    }

    /// Write the session's screen to its capture file, when it has one.
    pub(super) async fn capture_one(&self, mux: &impl Sessions, record: &Record) {
        if !matches!(record.state, State::Running | State::Held) {
            return;
        }
        if !mux.exists(&record.rmux_session).await.unwrap_or(false) {
            return;
        }
        let Ok(mut lines) = mux.snapshot(&record.rmux_session).await else {
            return;
        };
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return;
        }
        let mut text = lines
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        let _ = crate::paths::write_private(&self.paths.capture(&record.id), text.as_bytes());
    }

    /// The periodic capture of every live session.
    pub(super) async fn capture_all(&self) {
        if self.skew.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await else {
            return;
        };
        for record in &self.background_records() {
            self.capture_one(&mux, record).await;
        }
    }

    /// Delete ended sessions 30 days after they ended. Called under the
    /// writer lock.
    pub(super) fn prune_ended(&self) {
        let records = self.background_records();
        for id in crate::restore::prune(&self.paths, records, std::time::SystemTime::now()) {
            eprintln!("kersh host: removed session {id}, ended more than 30 days ago");
        }
    }
}
