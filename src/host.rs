//! The host: the single writer of session records, one per user.
//!
//! `kersh host` takes `host.lock`, captures the login environment, starts
//! the session server when none answers, and serves the client protocol
//! on `host.sock`. Every write (start, stop, resume, restart, rm, and the
//! folding of each wrapper's exit note) happens under one lock, so two
//! starts cannot both take a group's last place.
//!
//! Reads do not need the host: `kersh ps`, `events`, and `screen` read the
//! files and the session server directly.
//!
//! The host holds a connection to the session server and watches it, so a
//! server that dies or a session that vanishes is restored at once, not
//! only at the next host start (see [`restoring`]).

mod restoring;

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{Mutex, broadcast};

use crate::harness::{self, Conversation as LaunchConversation, LaunchSpec};
use crate::mux::Rmux;
use crate::paths::Paths;
use crate::proto::{self, Launch, Op, Response};
use crate::runtime::Config;
use crate::session::{
    Conversation, ConversationSource, EnvironmentInfo, Harness, Mode, RECORD_SCHEMA, Record,
    RuntimeInfo, SessionError, State,
};
use crate::sessions::{SessionSpec, Sessions, Size};
use crate::wrapper::{ExitKind, LaunchFile, Note};

/// What the host writes to `host.lock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLock {
    pub pid: u32,
    pub entry: Vec<String>,
    pub version: String,
    pub schema: u32,
}

/// Read `host.lock`, if a host wrote one.
#[must_use]
pub fn read_lock(paths: &Paths) -> Option<HostLock> {
    let text = std::fs::read_to_string(paths.host_lock()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The launchd job label for a role in this state directory.
#[must_use]
pub fn label(paths: &Paths, role: &str) -> String {
    format!(
        "kersh.{role}.{}",
        crate::service::short_hash(&paths.state.display().to_string())
    )
}

/// Which command moves a session back into running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relaunch {
    /// `kersh resume`: a session that is not running.
    Resume,
    /// `kersh restart`: any interactive session.
    Restart,
}

struct Host {
    paths: Paths,
    config: Config,
    entry: Vec<String>,
    login_env: Vec<(String, String)>,
    writer: Mutex<()>,
    changes: broadcast::Sender<Record>,
    /// When each interactive Codex session was last searched for its
    /// rollout file.
    searched: std::sync::Mutex<HashMap<String, Instant>>,
    /// The live session server speaks another rmux wire version: session
    /// operations are refused until `kersh restart --mux`.
    skew: AtomicBool,
    /// When the watcher last tried to restore each session, so a restore
    /// that cannot create its pane is not retried on every tick.
    attempted: std::sync::Mutex<HashMap<String, Instant>>,
}

/// Run the host until it is killed. Returns at once, successfully, when
/// another host holds the lock.
pub fn run(paths: Paths) -> Result<(), String> {
    crate::paths::private_dir(&paths.state).map_err(|error| error.to_string())?;
    crate::paths::private_dir(&paths.sessions()).map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.host_lock())
        .map_err(|error| format!("{}: {error}", paths.host_lock().display()))?;
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        eprintln!("kersh: a host already runs for {}", paths.state.display());
        return Ok(());
    }
    let entry = crate::service::entry();
    write_lock(
        &lock,
        &HostLock {
            pid: std::process::id(),
            entry: entry.clone(),
            version: proto::VERSION.to_owned(),
            schema: proto::PROTOCOL_SCHEMA,
        },
    )?;
    let config = Config::load(&paths)?;
    let login_env = crate::login_env::capture(&entry)?;
    crate::paths::write_json(&paths.login_env(), &login_env).map_err(|error| error.to_string())?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let (changes, _) = broadcast::channel(256);
    let host = Arc::new(Host {
        paths,
        config,
        entry,
        login_env,
        writer: Mutex::new(()),
        changes,
        searched: std::sync::Mutex::new(HashMap::new()),
        skew: AtomicBool::new(false),
        attempted: std::sync::Mutex::new(HashMap::new()),
    });
    let result = runtime.block_on(serve(host));
    drop(lock);
    result
}

fn write_lock(file: &std::fs::File, lock: &HostLock) -> Result<(), String> {
    use std::io::{Seek as _, Write as _};
    let text = serde_json::to_string(lock).map_err(|error| error.to_string())?;
    let mut file = file;
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| file.write_all(text.as_bytes()))
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

async fn serve(host: Arc<Host>) -> Result<(), String> {
    match host.ensure_mux().await {
        Ok((mux, started)) => {
            let _guard = host.writer.lock().await;
            host.prune_ended();
            let cause = started.then_some(restoring::SERVER_STOPPED);
            host.restore_locked(&mux, cause).await;
        }
        // A server of another wire version keeps its agents running; the
        // host lists from the records and waits for `kersh restart --mux`.
        Err(error) if host.skew.load(Ordering::SeqCst) => eprintln!("kersh host: {error}"),
        Err(error) => return Err(error),
    }
    let socket = host.paths.host_sock();
    // Under the lock, a socket file left behind is stale.
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|error| format!("{}: {error}", socket.display()))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!("kersh host {} serving {}", proto::VERSION, socket.display());
    let watcher = Arc::clone(&host);
    tokio::spawn(async move {
        let mut server = None;
        let mut captured = Instant::now();
        let mut pruned = Instant::now();
        loop {
            watcher.watch_once().await;
            watcher.watch_server(&mut server).await;
            if captured.elapsed() >= watcher.config.capture_every() {
                watcher.capture_all().await;
                captured = Instant::now();
            }
            if pruned.elapsed() >= Duration::from_secs(3600) {
                let _guard = watcher.writer.lock().await;
                watcher.prune_ended();
                pruned = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    loop {
        let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let host = Arc::clone(&host);
        tokio::spawn(async move {
            if let Err(error) = host.connection(stream).await {
                eprintln!("kersh host: a connection failed: {error}");
            }
        });
    }
}

async fn send_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Apply a wrapper's note to its record: the pids it reported, then how
/// the harness exited. A note of an older launch changes nothing.
fn fold_note(current: &mut Record, note: Note) {
    if note.launch != current.launches {
        return;
    }
    current.wrapper_pid = Some(note.wrapper_pid);
    current.harness_pid = note.harness_pid;
    // A held pane runs no harness; the old group stays recorded until
    // it is seen empty.
    if note.pgid.is_some() {
        current.pgid = note.pgid;
    }
    if note.boot_id.is_some() {
        current.boot_id = note.boot_id;
    }
    if let Some(exit) = note.exit
        && current.state == State::Running
    {
        match exit.kind {
            ExitKind::Ended => {
                current.state = State::Ended;
                current.ended_at = Some(exit.at);
                current.exit_status = exit.status;
                current.reason = Some(exit.reason);
            }
            ExitKind::Held => {
                current.state = State::Held;
                current.exit_status = exit.status;
                current.reason = Some(exit.reason);
            }
            ExitKind::Signal => {
                current.reason = Some(exit.reason);
            }
        }
    }
}

/// Whether conversation `id` is free, from the process-table search for
/// a holder. A table that cannot be read refuses: kersh never starts a
/// second harness on a conversation it cannot see.
fn holder_verdict(id: &str, holder: Result<Option<i32>, String>) -> Result<(), SessionError> {
    match holder {
        Ok(None) => Ok(()),
        Ok(Some(pid)) => Err(SessionError::failed(format!(
            "another process (pid {pid}) already runs conversation {id}; \
             stop that process or pick another conversation"
        ))),
        Err(error) => Err(SessionError::failed(format!(
            "cannot tell whether another process runs conversation {id}: {error}; \
             check that `ps` runs for this user, then run the command again"
        ))),
    }
}

/// Read a session's note, if its wrapper wrote one.
fn read_note(paths: &Paths, id: &str) -> Option<Note> {
    std::fs::read_to_string(paths.note(id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn to_value(value: &impl Serialize) -> Result<Value, SessionError> {
    serde_json::to_value(value).map_err(|error| SessionError::failed(error.to_string()))
}

impl Host {
    async fn connection(&self, stream: tokio::net::UnixStream) -> std::io::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await? {
            let request = match proto::parse_request(&line) {
                Ok(request) => request,
                Err(error) => {
                    send_line(&mut write, &Response::err(error)).await?;
                    continue;
                }
            };
            if request.op == Op::Follow {
                return self.follow(&mut write).await;
            }
            let response = match self.handle(request.op).await {
                Ok(value) => Response::ok(value),
                Err(error) => Response::err(error),
            };
            send_line(&mut write, &response).await?;
        }
        Ok(())
    }

    async fn follow(&self, write: &mut tokio::net::unix::OwnedWriteHalf) -> std::io::Result<()> {
        let mut changes = self.changes.subscribe();
        let send_all = async |write: &mut tokio::net::unix::OwnedWriteHalf| {
            let records = crate::session::read_records(&self.paths.sessions()).unwrap_or_default();
            for record in records {
                let value = serde_json::to_value(&record).unwrap_or(Value::Null);
                send_line(write, &Response::ok(value)).await?;
            }
            Ok::<(), std::io::Error>(())
        };
        send_all(write).await?;
        loop {
            match changes.recv().await {
                Ok(record) => {
                    let value = serde_json::to_value(&record).unwrap_or(Value::Null);
                    send_line(write, &Response::ok(value)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => send_all(write).await?,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    async fn handle(&self, op: Op) -> Result<Value, SessionError> {
        match op {
            Op::Ping => Ok(Value::String(proto::VERSION.to_owned())),
            Op::Follow => Err(SessionError::usage("follow is a stream")),
            Op::Start(launch) => to_value(&self.start(*launch).await?),
            Op::Stop { id } => to_value(&self.stop(&id).await?),
            Op::Resume { id, prompt } => to_value(&self.resume(&id, prompt).await?),
            Op::Restart { id } => to_value(&self.restart(&id).await?),
            Op::Send { id, text } => {
                self.send(&id, &text).await?;
                Ok(Value::Null)
            }
            Op::Rm { id } => {
                self.rm(&id).await?;
                Ok(Value::Null)
            }
            Op::HarnessList => to_value(&harness::list(&self.login_env)),
            Op::HeldReturn { id } => {
                self.held_return(&id).await?;
                Ok(Value::Null)
            }
            Op::Capture { id } => {
                let record = self.load(&id)?;
                let mux = self.mux().await?;
                self.capture_one(&mux, &record).await;
                Ok(Value::Null)
            }
            Op::RestartMux => to_value(&self.restart_mux().await?),
        }
    }

    fn save(&self, record: &Record) -> Result<(), SessionError> {
        crate::paths::write_json(&self.paths.record(&record.id), record)
            .map_err(|error| SessionError::failed(format!("cannot write the record: {error}")))?;
        let _ = self.changes.send(record.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Record, SessionError> {
        let path = self.paths.record(id);
        if !path.is_file() || !crate::runtime::safe_name(id) {
            return Err(SessionError::NotFound { id: id.to_owned() });
        }
        crate::session::read_record(&path)
    }

    /// The session server for a session operation. A server of another
    /// wire version is refused with the way past it.
    async fn mux(&self) -> Result<Rmux, SessionError> {
        if let Err(error) = crate::mux::check_wire(&self.paths) {
            self.skew.store(true, Ordering::SeqCst);
            return Err(SessionError::failed(error));
        }
        self.skew.store(false, Ordering::SeqCst);
        match Rmux::connect(&self.paths.mux_sock()).await {
            Ok(mux) => Ok(mux),
            Err(_) => self
                .ensure_mux()
                .await
                .map(|(mux, _)| mux)
                .map_err(SessionError::failed),
        }
    }

    /// Connect to the session server, starting it when none answers. The
    /// flag says whether this call started it.
    async fn ensure_mux(&self) -> Result<(Rmux, bool), String> {
        if let Err(error) = crate::mux::check_wire(&self.paths) {
            self.skew.store(true, Ordering::SeqCst);
            return Err(error);
        }
        if let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await {
            return Ok((mux, false));
        }
        let mut argv = self.entry.clone();
        argv.extend([
            "mux".to_owned(),
            "--state".to_owned(),
            self.paths.state.display().to_string(),
        ]);
        crate::service::spawn(
            self.config.service_manager(),
            &label(&self.paths, "mux"),
            &argv,
            &self.login_env,
            &self.paths.state.join("mux.log"),
            &self.paths.state,
        )?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await {
                return Ok((mux, true));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "the session server did not start; see {}",
                    self.paths.state.join("mux.log").display()
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Each group's limit. A missing file has none; a file that cannot be
    /// read or parsed is an error, so no limit is lifted and no write
    /// drops another group's.
    fn groups(&self) -> Result<BTreeMap<String, u32>, SessionError> {
        let path = self.paths.groups();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => {
                return Err(SessionError::failed(format!("{}: {error}", path.display())));
            }
        };
        serde_json::from_str(&text)
            .map_err(|error| SessionError::failed(format!("{}: {error}", path.display())))
    }

    /// Refuse a move into `running` at the group's limit. Called under the
    /// writer lock.
    fn gate(&self, group: Option<&str>, max: Option<u32>) -> Result<(), SessionError> {
        let Some(group) = group else {
            return Ok(());
        };
        let mut groups = self.groups()?;
        if let Some(max) = max {
            groups.insert(group.to_owned(), max);
            crate::paths::write_json(&self.paths.groups(), &groups)
                .map_err(|error| SessionError::failed(error.to_string()))?;
        }
        let Some(&max) = groups.get(group) else {
            return Ok(());
        };
        let running = crate::session::read_records(&self.paths.sessions())?
            .iter()
            .filter(|record| {
                record.group.as_deref() == Some(group) && record.state == State::Running
            })
            .count();
        let running = u32::try_from(running).unwrap_or(u32::MAX);
        if running >= max {
            return Err(SessionError::AtLimit {
                group: group.to_owned(),
                running,
                max,
            });
        }
        Ok(())
    }

    /// The session's whole environment: the login environment, then
    /// kersh's variables, then the runtime's.
    fn session_env(
        &self,
        id: &str,
        launch: &Launch,
        tempdir: Option<&Path>,
    ) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = self.login_env.iter().cloned().collect();
        env.remove("GAFF_PROFILE");
        env.insert("KERSH_SESSION".to_owned(), id.to_owned());
        env.insert("COLORTERM".to_owned(), "truecolor".to_owned());
        env.insert(
            "COLORFGBG".to_owned(),
            self.config
                .colorfgbg
                .clone()
                .unwrap_or_else(|| "15;0".to_owned()),
        );
        if let Some(root) = &self.config.memory.root {
            env.insert("KERSH_MEMORY_ROOT".to_owned(), root.display().to_string());
        }
        if let Some(profile) = &launch.settings.profile {
            env.insert("GAFF_PROFILE".to_owned(), profile.clone());
        }
        if let Some(dir) = tempdir {
            env.insert("TMPDIR".to_owned(), dir.display().to_string());
        }
        env.extend(launch.settings.env.clone());
        env.into_iter().collect()
    }

    #[allow(clippy::too_many_lines)] // The checks, then the files, in launch order.
    async fn start(&self, launch: Launch) -> Result<Record, SessionError> {
        let settings = &launch.settings;
        if settings.harness == Harness::Rig {
            return Err(SessionError::usage(
                "a rig agent does not run in a session yet; `kersh run` runs it in the foreground",
            ));
        }
        let prompt = launch.prompt.clone().filter(|text| !text.trim().is_empty());
        if settings.mode == Mode::Headless && prompt.is_none() {
            return Err(SessionError::usage(
                "a headless start needs a prompt; give --prompt-file",
            ));
        }
        if launch.max_running.is_some() && launch.group.is_none() {
            return Err(SessionError::usage("--max-running needs --group"));
        }
        if let Some(group) = &launch.group
            && !crate::runtime::safe_name(group)
        {
            return Err(SessionError::usage(format!(
                "`{group}` is not a valid group name"
            )));
        }
        if !launch.cwd.is_dir() {
            return Err(SessionError::usage(format!(
                "the directory {} does not exist",
                launch.cwd.display()
            )));
        }
        harness::check_flags(settings.harness, &launch.flags).map_err(SessionError::usage)?;
        // Any client may send settings, so the host refuses the variables
        // a runtime may not set even when the CLI already did.
        for key in settings.env.keys() {
            crate::runtime::check_env_key(key).map_err(SessionError::usage)?;
        }
        if let Some(profile) = &settings.profile {
            // The check reads the environment the harness gets, not the
            // login environment it is built from.
            let env = self.session_env("", &launch, None);
            crate::guard::check_profile(
                settings.harness,
                settings.mode,
                profile,
                &launch.cwd,
                &env,
            )
            .map_err(SessionError::failed)?;
        }
        let build =
            harness::resolve(settings.harness, &self.login_env).map_err(SessionError::failed)?;
        let mux = self.mux().await?;

        let _guard = self.writer.lock().await;
        if let Some(conversation) = &launch.resume_conversation {
            self.check_conversation_free(settings.harness, conversation, None)?;
        }
        self.gate(launch.group.as_deref(), launch.max_running)?;

        let id = loop {
            let id = crate::util::short_id();
            if !self.paths.session(&id).exists() {
                break id;
            }
        };
        let dir = self.paths.session(&id);
        crate::paths::private_dir(&dir).map_err(|error| SessionError::failed(error.to_string()))?;
        let tempdir = if settings.tempdir {
            let tmp = dir.join("tmp");
            crate::paths::private_dir(&tmp)
                .map_err(|error| SessionError::failed(error.to_string()))?;
            Some(tmp)
        } else {
            None
        };
        let system_prompt = match launch
            .system_text
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(text) => {
                let path = dir.join("system.md");
                crate::paths::write_private(&path, text.as_bytes())
                    .map_err(|error| SessionError::failed(error.to_string()))?;
                Some(path)
            }
            None => None,
        };
        let first_prompt = match &prompt {
            Some(text) => {
                let path = dir.join("prompt-1.md");
                crate::paths::write_private(&path, text.as_bytes())
                    .map_err(|error| SessionError::failed(error.to_string()))?;
                Some(path)
            }
            None => None,
        };
        let conversation = match (&launch.resume_conversation, settings.harness) {
            (Some(id), _) => Some(Conversation {
                id: id.clone(),
                source: ConversationSource::Caller,
            }),
            (None, Harness::ClaudeCode) => Some(Conversation {
                id: uuid::Uuid::new_v4().to_string(),
                source: ConversationSource::Launch,
            }),
            (None, _) => None,
        };
        let size = launch.size.or(self.config.pane).unwrap_or_default();
        let mut record = Record {
            schema: RECORD_SCHEMA,
            id: id.clone(),
            name: launch.name.clone(),
            harness: settings.harness,
            mode: settings.mode,
            state: State::Running,
            build,
            environment: EnvironmentInfo {
                kind: settings.environment.clone(),
                cwd: launch.cwd.clone(),
                tempdir: tempdir.clone(),
            },
            agent: launch.agent.clone(),
            runtime: RuntimeInfo {
                name: launch.runtime_name.clone(),
                settings: settings.clone(),
            },
            system_prompt,
            conversation,
            agent_flags: launch.flags.clone(),
            first_prompt,
            labels: launch.labels.clone(),
            group: launch.group.clone(),
            pgid: None,
            boot_id: crate::service::boot_id(),
            wrapper_pid: None,
            harness_pid: None,
            rmux_session: format!("kersh-{id}"),
            pane_size: size,
            created_at: crate::util::now_rfc3339(),
            ended_at: None,
            exit_status: None,
            reason: None,
            restores: Vec::new(),
            held_run: None,
            launches: 1,
            progress: None,
        };
        let env = self.session_env(&id, &launch, tempdir.as_deref());
        let resume = launch.resume_conversation.is_some();
        self.write_launch(
            &record,
            &env,
            prompt.as_deref(),
            resume,
            record.first_prompt.clone(),
            None,
            None,
        )?;
        self.save(&record)?;
        if let Err(error) = self.create_pane(&mux, &record).await {
            record.state = State::Ended;
            record.reason = Some(format!("the session server refused the pane: {error}"));
            record.ended_at = Some(crate::util::now_rfc3339());
            self.save(&record)?;
            return Err(SessionError::failed(
                record.reason.clone().unwrap_or_default(),
            ));
        }
        Ok(record)
    }

    /// Write `launch.json` for the record's current launch.
    #[allow(clippy::too_many_arguments)] // Each part of one launch file.
    fn write_launch(
        &self,
        record: &Record,
        env: &[(String, String)],
        prompt_arg: Option<&str>,
        resume: bool,
        stdin: Option<PathBuf>,
        preface: Option<String>,
        hold: Option<crate::wrapper::Hold>,
    ) -> Result<(), SessionError> {
        let system_text = record
            .system_prompt
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        let conversation = match (&record.conversation, resume) {
            (Some(conversation), true) => LaunchConversation::Resume(&conversation.id),
            (Some(conversation), false) => LaunchConversation::New(Some(&conversation.id)),
            (None, _) => LaunchConversation::New(None),
        };
        let spec = LaunchSpec {
            harness: record.harness,
            mode: record.mode,
            program: &record.build.path,
            model: record.runtime.settings.model.as_deref(),
            system_prompt: record.system_prompt.as_deref(),
            system_text: system_text.as_deref(),
            flags: &record.agent_flags,
            conversation,
            prompt_arg: if record.mode == Mode::Interactive {
                prompt_arg
            } else {
                None
            },
        };
        let file = LaunchFile {
            id: record.id.clone(),
            launch: record.launches,
            harness: record.harness,
            mode: record.mode,
            entry: self.entry.clone(),
            argv: harness::argv(&spec),
            env: env.to_vec(),
            cwd: record.environment.cwd.clone(),
            stdin: if record.mode == Mode::Headless {
                stdin
            } else {
                None
            },
            preface,
            hold,
        };
        crate::paths::write_json(&self.paths.launch(&record.id), &file)
            .map_err(|error| SessionError::failed(error.to_string()))
    }

    async fn create_pane(&self, mux: &impl Sessions, record: &Record) -> Result<(), String> {
        let mut argv = self.entry.clone();
        argv.extend([
            "session".to_owned(),
            "run".to_owned(),
            "--state".to_owned(),
            self.paths.state.display().to_string(),
            record.id.clone(),
        ]);
        let spec = SessionSpec {
            name: record.rmux_session.clone(),
            argv,
            env: vec![("KERSH_SESSION".to_owned(), record.id.clone())],
            cwd: record.environment.cwd.clone(),
            size: Size {
                cols: record.pane_size.cols,
                rows: record.pane_size.rows,
            },
        };
        mux.create(&spec).await.map_err(|error| error.to_string())
    }

    /// Kill the record's pane, if it exists, and wait for its wrapper. A
    /// session server that cannot answer is an error: the harness may
    /// still run.
    async fn end_pane(&self, mux: &impl Sessions, record: &Record) -> Result<(), SessionError> {
        let unreachable = |error: &dyn std::fmt::Display| {
            SessionError::failed(format!(
                "cannot end session {}'s pane, so it is left as it was: {error}",
                record.id
            ))
        };
        if mux
            .exists(&record.rmux_session)
            .await
            .map_err(|error| unreachable(&error))?
        {
            mux.kill(&record.rmux_session)
                .await
                .map_err(|error| unreachable(&error))?;
        }
        let Some(pid) = record.wrapper_pid.and_then(rustix::process::Pid::from_raw) else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while rustix::process::test_kill_process(pid).is_ok() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<Record, SessionError> {
        let mux = self.mux().await?;
        self.stop_on(&mux, id).await
    }

    async fn stop_on(&self, mux: &impl Sessions, id: &str) -> Result<Record, SessionError> {
        let _guard = self.writer.lock().await;
        let mut record = self.load(id)?;
        // A clean stop is one of the times the screen is captured.
        self.capture_one(mux, &record).await;
        self.end_pane(mux, &record).await?;
        if matches!(record.state, State::Running | State::Held | State::Lost) {
            record.state = State::Ended;
            record.reason = Some("stopped".to_owned());
            record.ended_at = Some(crate::util::now_rfc3339());
            record.held_run = None;
            self.save(&record)?;
        }
        Ok(record)
    }

    async fn restart(&self, id: &str) -> Result<Record, SessionError> {
        let mux = self.mux().await?;
        self.restart_on(&mux, id).await
    }

    async fn restart_on(&self, mux: &impl Sessions, id: &str) -> Result<Record, SessionError> {
        let record = self.load(id)?;
        if record.mode == Mode::Headless {
            return Err(SessionError::usage(format!(
                "a headless session continues only with a new prompt: kersh resume {id} --prompt-file -"
            )));
        }
        self.relaunch(mux, id, None, Relaunch::Restart).await
    }

    async fn resume(&self, id: &str, prompt: Option<String>) -> Result<Record, SessionError> {
        let mux = self.mux().await?;
        self.resume_on(&mux, id, prompt).await
    }

    async fn resume_on(
        &self,
        mux: &impl Sessions,
        id: &str,
        prompt: Option<String>,
    ) -> Result<Record, SessionError> {
        let record = self.load(id)?;
        let prompt = prompt.filter(|text| !text.trim().is_empty());
        if record.mode == Mode::Headless && prompt.is_none() {
            return Err(SessionError::usage(
                "resuming a headless session needs a prompt; give --prompt-file",
            ));
        }
        self.relaunch(mux, id, prompt, Relaunch::Resume).await
    }

    async fn relaunch(
        &self,
        mux: &impl Sessions,
        id: &str,
        prompt: Option<String>,
        kind: Relaunch,
    ) -> Result<Record, SessionError> {
        let _guard = self.writer.lock().await;
        let mut record = self.load(id)?;
        // The state is read under the lock, so a session cannot move into
        // or out of running between the check and the launch.
        if kind == Relaunch::Resume && record.state == State::Running {
            return Err(SessionError::usage(format!(
                "session {id} is running; `kersh restart {id}` restarts it"
            )));
        }
        // A running session already holds its place, so its restart is
        // not gated; any other move into running is.
        if record.state != State::Running {
            self.gate(record.group.as_deref(), None)?;
        }
        if record.conversation.is_none() {
            record.conversation = self.discover_conversation(&record);
        }
        let Some(conversation) = &record.conversation else {
            return Err(SessionError::failed(format!(
                "session {id} has no {} conversation to resume yet",
                record.harness
            )));
        };
        // Before the pane goes: a refused relaunch leaves the session as
        // it was.
        self.check_conversation_free(record.harness, &conversation.id, Some(&record))?;
        self.end_pane(mux, &record).await?;
        if let Some(pgid) = record.pgid
            && crate::restore::group_alive(pgid)
        {
            return Err(SessionError::failed(format!(
                "a process from the old session still runs in process group {pgid}; end it first"
            )));
        }
        self.launch_again(mux, &mut record, restoring::How::Resume { prompt }, None)
            .await?;
        Ok(record)
    }

    /// Refuse a conversation that another live record holds, or that a
    /// harness process runs other than `own`'s own launch: its wrapper,
    /// the wrapper's descendants, and its harness's process group, when
    /// `own` is running or held and from this boot. A process table that
    /// cannot be read refuses too. Called under the writer lock.
    fn check_conversation_free(
        &self,
        harness: Harness,
        id: &str,
        own: Option<&Record>,
    ) -> Result<(), SessionError> {
        let records = crate::session::read_records(&self.paths.sessions())?;
        if let Some(other) = records.iter().find(|record| {
            Some(record.id.as_str()) != own.map(|own| own.id.as_str())
                && matches!(record.state, State::Running | State::Held)
                && record
                    .conversation
                    .as_ref()
                    .is_some_and(|conversation| conversation.id == id)
        }) {
            return Err(SessionError::failed(format!(
                "session {} already holds conversation {id}",
                other.id
            )));
        }
        // Only a live record's process ids are its own: an ended record's
        // may name other processes now, and so may ids from another boot
        // or from a boot either side cannot name.
        let boot = crate::service::boot_id();
        let own = own
            .filter(|record| matches!(record.state, State::Running | State::Held))
            .filter(|record| match (&record.boot_id, &boot) {
                (Some(then), Some(now)) => then == now,
                _ => false,
            })
            .map(|record| crate::discover::Own {
                roots: record
                    .wrapper_pid
                    .into_iter()
                    .chain(record.harness_pid)
                    .collect(),
                pgid: record.pgid,
            })
            .unwrap_or_default();
        holder_verdict(id, crate::discover::holder(harness.program(), id, &own))
    }

    /// The environment of a relaunch: the last launch's, which already
    /// holds the login environment and kersh's variables. Without one, the
    /// login environment.
    fn relaunch_env(&self, record: &Record) -> Vec<(String, String)> {
        let env = std::fs::read_to_string(self.paths.launch(&record.id))
            .ok()
            .and_then(|text| serde_json::from_str::<LaunchFile>(&text).ok())
            .map_or_else(|| self.login_env.clone(), |file| file.env);
        let mut env: BTreeMap<String, String> = env.into_iter().collect();
        env.insert("KERSH_SESSION".to_owned(), record.id.clone());
        env.into_iter().collect()
    }

    async fn send(&self, id: &str, text: &str) -> Result<(), SessionError> {
        let record = self.load(id)?;
        if record.mode != Mode::Interactive || record.state != State::Running {
            return Err(SessionError::failed(format!(
                "session {id} is {} {}; send types only into a running interactive session",
                record.state, record.mode
            )));
        }
        let mux = self.mux().await?;
        mux.send_text(
            &record.rmux_session,
            &format!("\u{1b}[200~{text}\u{1b}[201~"),
        )
        .await
        .map_err(|error| SessionError::failed(error.to_string()))?;
        mux.send_key(&record.rmux_session, "Enter")
            .await
            .map_err(|error| SessionError::failed(error.to_string()))
    }

    async fn rm(&self, id: &str) -> Result<(), SessionError> {
        let _guard = self.writer.lock().await;
        let record = self.load(id)?;
        if !matches!(record.state, State::Ended | State::Lost) {
            return Err(SessionError::failed(format!(
                "session {id} is {}; `kersh stop {id}` ends it first",
                record.state
            )));
        }
        std::fs::remove_dir_all(self.paths.session(id))
            .map_err(|error| SessionError::failed(error.to_string()))
    }

    /// The conversation a harness reported, for a record that has none.
    fn discover_conversation(&self, record: &Record) -> Option<Conversation> {
        if record.harness != Harness::Codex {
            return None;
        }
        match record.mode {
            Mode::Headless => crate::events::read(&self.paths.events(&record.id), 0)
                .ok()?
                .into_iter()
                .find_map(|event| event.conversation)
                .map(|id| Conversation {
                    id,
                    source: ConversationSource::Stream,
                }),
            Mode::Interactive => {
                let pid = record.harness_pid?;
                crate::discover::codex_rollout(pid).map(|id| Conversation {
                    id,
                    source: ConversationSource::Rollout,
                })
            }
        }
    }

    /// Fold every live session's note into its record.
    async fn watch_once(&self) {
        let Ok(records) = crate::session::read_records(&self.paths.sessions()) else {
            return;
        };
        for record in records {
            if !matches!(record.state, State::Running | State::Held) {
                continue;
            }
            let note = std::fs::read_to_string(self.paths.note(&record.id))
                .ok()
                .and_then(|text| serde_json::from_str::<Note>(&text).ok());
            let wants_conversation =
                record.conversation.is_none() && record.harness == Harness::Codex;
            let note_changes = note.as_ref().is_some_and(|note| {
                let unfolded_exit = note.exit.as_ref().is_some_and(|exit| {
                    record.state == State::Running
                        && !(exit.kind == ExitKind::Signal
                            && record.reason.as_deref() == Some(exit.reason.as_str()))
                });
                note.launch == record.launches
                    && (record.wrapper_pid != Some(note.wrapper_pid)
                        || record.harness_pid != note.harness_pid
                        || unfolded_exit)
            });
            if !note_changes && !wants_conversation {
                continue;
            }
            if wants_conversation && !note_changes && !self.due_for_search(&record.id) {
                continue;
            }
            let _guard = self.writer.lock().await;
            let Ok(mut current) = self.load(&record.id) else {
                continue;
            };
            if current != record {
                continue;
            }
            if let Some(note) = note {
                fold_note(&mut current, note);
            }
            if current.conversation.is_none() {
                current.conversation = self.discover_conversation(&current);
            }
            if current != record {
                let _ = self.save(&current);
            }
        }
    }

    fn due_for_search(&self, id: &str) -> bool {
        let Ok(mut searched) = self.searched.lock() else {
            return false;
        };
        let now = Instant::now();
        match searched.get(id) {
            Some(last) if now.duration_since(*last) < Duration::from_secs(1) => false,
            _ => {
                searched.insert(id.to_owned(), now);
                true
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::session::Build;

    #[derive(Debug, thiserror::Error)]
    #[error("fake session server: {0}")]
    struct FakeError(&'static str);

    /// A session server that records kills and creates, and fails every
    /// call when `fail` is set.
    #[derive(Default)]
    struct Fake {
        fail: bool,
        killed: std::sync::Mutex<Vec<String>>,
        created: std::sync::Mutex<Vec<String>>,
    }

    impl Fake {
        fn check(&self) -> Result<(), FakeError> {
            if self.fail {
                Err(FakeError("unreachable"))
            } else {
                Ok(())
            }
        }
    }

    // The double answers at once; `async fn` keeps it shaped like the
    // real implementation.
    #[allow(clippy::unused_async_trait_impl)]
    impl Sessions for Fake {
        type Error = FakeError;

        async fn create(&self, spec: &SessionSpec) -> Result<(), FakeError> {
            self.check()?;
            self.created.lock().unwrap().push(spec.name.clone());
            Ok(())
        }

        async fn list(&self) -> Result<Vec<crate::sessions::SessionInfo>, FakeError> {
            self.check().map(|()| Vec::new())
        }

        async fn exists(&self, _name: &str) -> Result<bool, FakeError> {
            self.check().map(|()| true)
        }

        async fn snapshot(&self, _name: &str) -> Result<Vec<String>, FakeError> {
            self.check().map(|()| Vec::new())
        }

        async fn send_text(&self, _name: &str, _text: &str) -> Result<(), FakeError> {
            self.check()
        }

        async fn send_key(&self, _name: &str, _key: &str) -> Result<(), FakeError> {
            self.check()
        }

        async fn kill(&self, name: &str) -> Result<(), FakeError> {
            self.check()?;
            self.killed.lock().unwrap().push(name.to_owned());
            Ok(())
        }
    }

    fn host(dir: &Path) -> Host {
        let paths = Paths::new(dir.join("state"), dir.join("config"));
        crate::paths::private_dir(&paths.state).unwrap();
        crate::paths::private_dir(&paths.sessions()).unwrap();
        Host {
            paths,
            config: Config::default(),
            entry: vec!["/usr/bin/false".to_owned()],
            login_env: Vec::new(),
            writer: Mutex::new(()),
            changes: broadcast::channel(16).0,
            searched: std::sync::Mutex::new(HashMap::new()),
            skew: AtomicBool::new(false),
            attempted: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn record(id: &str, state: State, group: Option<&str>, conversation: &str) -> Record {
        Record {
            schema: RECORD_SCHEMA,
            id: id.to_owned(),
            name: None,
            harness: Harness::ClaudeCode,
            mode: Mode::Interactive,
            state,
            build: Build {
                installer: "path".to_owned(),
                path: PathBuf::from("/usr/bin/false"),
                version: "0".to_owned(),
            },
            environment: EnvironmentInfo {
                kind: "local".to_owned(),
                cwd: PathBuf::from("/"),
                tempdir: None,
            },
            agent: None,
            runtime: RuntimeInfo {
                name: None,
                settings: crate::session::Settings {
                    harness: Harness::ClaudeCode,
                    mode: Mode::Interactive,
                    model: None,
                    environment: "local".to_owned(),
                    profile: None,
                    resume_prompt: None,
                    tempdir: false,
                    env: BTreeMap::new(),
                },
            },
            system_prompt: None,
            conversation: Some(Conversation {
                id: conversation.to_owned(),
                source: ConversationSource::Launch,
            }),
            agent_flags: Vec::new(),
            first_prompt: None,
            labels: BTreeMap::new(),
            group: group.map(str::to_owned),
            pgid: None,
            boot_id: None,
            wrapper_pid: None,
            harness_pid: None,
            rmux_session: format!("kersh-{id}"),
            pane_size: crate::session::PaneSize::default(),
            created_at: crate::util::now_rfc3339(),
            ended_at: None,
            exit_status: None,
            reason: None,
            restores: Vec::new(),
            held_run: None,
            launches: 1,
            progress: None,
        }
    }

    /// Save `record` with a launch file, as a start leaves it.
    fn put(host: &Host, record: &Record) {
        crate::paths::private_dir(&host.paths.session(&record.id)).unwrap();
        host.write_launch(record, &[], None, false, None, None, None)
            .unwrap();
        host.save(record).unwrap();
    }

    fn conversation_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[tokio::test]
    async fn a_stop_the_session_server_cannot_answer_leaves_the_record_running() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        let running = record("a", State::Running, Some("g"), &conversation_id());
        put(&host, &running);
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        let error = host.stop_on(&fake, "a").await.unwrap_err();
        assert!(error.to_string().contains("unreachable"), "{error}");
        assert_eq!(host.load("a").unwrap(), running);
    }

    #[test]
    fn a_process_table_that_cannot_be_read_refuses_the_conversation() {
        holder_verdict("c", Ok(None)).unwrap();
        let held = holder_verdict("c", Ok(Some(42))).unwrap_err();
        assert!(held.to_string().contains("pid 42"), "{held}");
        let unread = holder_verdict("c", Err("no ps".to_owned())).unwrap_err();
        assert!(unread.to_string().contains("no ps"), "{unread}");
        assert!(unread.to_string().contains("`ps` runs"), "{unread}");
    }

    #[test]
    fn an_unreadable_groups_file_is_an_error_and_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        host.gate(Some("g"), None).unwrap();
        std::fs::write(host.paths.groups(), "{not json").unwrap();
        let error = host.gate(Some("g"), None).unwrap_err();
        assert!(error.to_string().contains("groups.json"), "{error}");
        assert!(host.gate(Some("other"), Some(2)).is_err());
        assert_eq!(
            std::fs::read_to_string(host.paths.groups()).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn an_id_that_leaves_the_sessions_directory_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        let outside = host.paths.state.join("evil");
        std::fs::create_dir_all(&outside).unwrap();
        let ended = record("../evil", State::Ended, None, &conversation_id());
        crate::paths::write_json(&outside.join("record.json"), &ended).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime.block_on(host.rm("../evil")).unwrap_err();
        assert!(matches!(error, SessionError::NotFound { .. }), "{error:?}");
        assert!(outside.join("record.json").is_file());
    }

    #[tokio::test]
    async fn resume_refuses_a_session_that_became_running_while_it_waited() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        let held = record("a", State::Held, None, &conversation_id());
        put(&host, &held);
        let fake = Fake::default();
        let guard = host.writer.lock().await;
        let (result, ()) = tokio::join!(host.resume_on(&fake, "a", None), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut running = held.clone();
            running.state = State::Running;
            host.save(&running).unwrap();
            drop(guard);
        });
        let error = result.unwrap_err();
        assert!(error.to_string().contains("is running"), "{error}");
        assert!(fake.killed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_gates_a_session_that_stopped_running_while_it_waited() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        host.gate(Some("g"), Some(1)).unwrap();
        let first = record("a", State::Running, Some("g"), &conversation_id());
        put(&host, &first);
        let fake = Fake::default();
        let guard = host.writer.lock().await;
        let (result, ()) = tokio::join!(host.restart_on(&fake, "a"), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut held = first.clone();
            held.state = State::Held;
            host.save(&held).unwrap();
            put(
                &host,
                &record("b", State::Running, Some("g"), &conversation_id()),
            );
            drop(guard);
        });
        let error = result.unwrap_err();
        assert!(matches!(error, SessionError::AtLimit { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_resume_onto_a_held_conversation_leaves_the_pane_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        let shared = conversation_id();
        put(&host, &record("a", State::Held, None, &shared));
        put(&host, &record("b", State::Running, None, &shared));
        let fake = Fake::default();
        let error = host.resume_on(&fake, "a", None).await.unwrap_err();
        assert!(error.to_string().contains("already holds"), "{error}");
        assert!(
            fake.killed.lock().unwrap().is_empty(),
            "the pane was killed"
        );
        assert_eq!(host.load("a").unwrap().state, State::Held);
    }

    #[tokio::test]
    async fn resume_and_restart_relaunch_through_the_session_server() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(tmp.path());
        put(&host, &record("a", State::Held, None, &conversation_id()));
        let fake = Fake::default();
        let resumed = host.resume_on(&fake, "a", None).await.unwrap();
        assert_eq!((resumed.state, resumed.launches), (State::Running, 2));
        assert_eq!(*fake.killed.lock().unwrap(), ["kersh-a"]);
        assert_eq!(*fake.created.lock().unwrap(), ["kersh-a"]);
        let error = host.resume_on(&fake, "a", None).await.unwrap_err();
        assert!(error.to_string().contains("is running"), "{error}");
        let restarted = host.restart_on(&fake, "a").await.unwrap();
        assert_eq!(restarted.launches, 3);
    }
}
