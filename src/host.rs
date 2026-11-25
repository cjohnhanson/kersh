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

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
use crate::sessions::{SessionSpec, Sessions as _, Size};
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
    host.ensure_mux().await?;
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
        loop {
            watcher.watch_once().await;
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

    async fn mux(&self) -> Result<Rmux, SessionError> {
        crate::mux::check_wire(&self.paths).map_err(SessionError::failed)?;
        match Rmux::connect(&self.paths.mux_sock()).await {
            Ok(mux) => Ok(mux),
            Err(_) => self.ensure_mux().await.map_err(SessionError::failed),
        }
    }

    /// Connect to the session server, starting it when none answers.
    async fn ensure_mux(&self) -> Result<Rmux, String> {
        if let Ok(mux) = Rmux::connect(&self.paths.mux_sock()).await {
            crate::mux::check_wire(&self.paths)?;
            return Ok(mux);
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
                return Ok(mux);
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

    fn groups(&self) -> BTreeMap<String, u32> {
        std::fs::read_to_string(self.paths.groups())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Refuse a move into `running` at the group's limit. Called under the
    /// writer lock.
    fn gate(&self, group: Option<&str>, max: Option<u32>) -> Result<(), SessionError> {
        let Some(group) = group else {
            return Ok(());
        };
        let mut groups = self.groups();
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
        if let Some(profile) = &settings.profile {
            crate::guard::check_profile(
                settings.harness,
                settings.mode,
                profile,
                &launch.cwd,
                &self.login_env,
            )
            .map_err(SessionError::failed)?;
        }
        let build =
            harness::resolve(settings.harness, &self.login_env).map_err(SessionError::failed)?;
        let mux = self.mux().await?;

        let _guard = self.writer.lock().await;
        if let Some(conversation) = &launch.resume_conversation {
            self.check_conversation_free(conversation, None)?;
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
            restores: 0,
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
    fn write_launch(
        &self,
        record: &Record,
        env: &[(String, String)],
        prompt_arg: Option<&str>,
        resume: bool,
        stdin: Option<PathBuf>,
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
        };
        crate::paths::write_json(&self.paths.launch(&record.id), &file)
            .map_err(|error| SessionError::failed(error.to_string()))
    }

    async fn create_pane(&self, mux: &Rmux, record: &Record) -> Result<(), String> {
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

    /// Kill the record's pane, if it exists, and wait for its wrapper.
    async fn end_pane(&self, mux: &Rmux, record: &Record) {
        if mux.exists(&record.rmux_session).await.unwrap_or(false) {
            let _ = mux.kill(&record.rmux_session).await;
        }
        let Some(pid) = record.wrapper_pid.and_then(rustix::process::Pid::from_raw) else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while rustix::process::test_kill_process(pid).is_ok() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn stop(&self, id: &str) -> Result<Record, SessionError> {
        let mux = self.mux().await?;
        let _guard = self.writer.lock().await;
        let mut record = self.load(id)?;
        if matches!(record.state, State::Running | State::Held | State::Lost) {
            record.state = State::Ended;
            record.reason = Some("stopped".to_owned());
            record.ended_at = Some(crate::util::now_rfc3339());
            self.save(&record)?;
        }
        self.end_pane(&mux, &record).await;
        Ok(record)
    }

    async fn restart(&self, id: &str) -> Result<Record, SessionError> {
        let record = self.load(id)?;
        if record.mode == Mode::Headless {
            return Err(SessionError::usage(format!(
                "a headless session continues only with a new prompt: kersh resume {id} --prompt-file -"
            )));
        }
        // A running session already holds its place, so its restart is
        // not gated; any other move into running is.
        let gated = record.state != State::Running;
        self.relaunch(id, None, gated).await
    }

    async fn resume(&self, id: &str, prompt: Option<String>) -> Result<Record, SessionError> {
        let record = self.load(id)?;
        if record.state == State::Running {
            return Err(SessionError::usage(format!(
                "session {id} is running; `kersh restart {id}` restarts it"
            )));
        }
        let prompt = prompt.filter(|text| !text.trim().is_empty());
        if record.mode == Mode::Headless && prompt.is_none() {
            return Err(SessionError::usage(
                "resuming a headless session needs a prompt; give --prompt-file",
            ));
        }
        self.relaunch(id, prompt, true).await
    }

    async fn relaunch(
        &self,
        id: &str,
        prompt: Option<String>,
        gated: bool,
    ) -> Result<Record, SessionError> {
        let mux = self.mux().await?;
        let _guard = self.writer.lock().await;
        let mut record = self.load(id)?;
        if gated {
            self.gate(record.group.as_deref(), None)?;
        }
        if record.conversation.is_none() {
            record.conversation = self.discover_conversation(&record);
        }
        if record.conversation.is_none() {
            return Err(SessionError::failed(format!(
                "session {id} has no {} conversation to resume yet",
                record.harness
            )));
        }
        self.end_pane(&mux, &record).await;
        if let Some(conversation) = &record.conversation {
            self.check_conversation_free(&conversation.id, Some(&record.id))?;
        }
        let launch = record.launches + 1;
        let stdin = match &prompt {
            Some(text) => {
                let path = self.paths.session(id).join(format!("prompt-{launch}.md"));
                crate::paths::write_private(&path, text.as_bytes())
                    .map_err(|error| SessionError::failed(error.to_string()))?;
                Some(path)
            }
            None => None,
        };
        record.launches = launch;
        record.state = State::Running;
        record.reason = None;
        record.ended_at = None;
        record.exit_status = None;
        record.wrapper_pid = None;
        record.harness_pid = None;
        record.pgid = None;
        record.boot_id = crate::service::boot_id();
        let env = self.relaunch_env(&record)?;
        self.write_launch(&record, &env, prompt.as_deref(), true, stdin)?;
        self.save(&record)?;
        if let Err(error) = self.create_pane(&mux, &record).await {
            record.state = State::Held;
            record.reason = Some(format!("the session server refused the pane: {error}"));
            self.save(&record)?;
            return Err(SessionError::failed(
                record.reason.clone().unwrap_or_default(),
            ));
        }
        Ok(record)
    }

    /// Refuse a conversation that another live record holds, or that a
    /// process outside kersh names. Called under the writer lock.
    fn check_conversation_free(&self, id: &str, own: Option<&str>) -> Result<(), SessionError> {
        let records = crate::session::read_records(&self.paths.sessions())?;
        if let Some(other) = records.iter().find(|record| {
            Some(record.id.as_str()) != own
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
        if crate::discover::process_names(id, &[]) {
            return Err(SessionError::failed(format!(
                "another process already runs conversation {id}"
            )));
        }
        Ok(())
    }

    /// The environment of a relaunch: the last launch's, which already
    /// holds the login environment and kersh's variables.
    fn relaunch_env(&self, record: &Record) -> Result<Vec<(String, String)>, SessionError> {
        let text = std::fs::read_to_string(self.paths.launch(&record.id))
            .map_err(|error| SessionError::failed(error.to_string()))?;
        let file: LaunchFile =
            serde_json::from_str(&text).map_err(|error| SessionError::failed(error.to_string()))?;
        Ok(file.env)
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
            if let Some(note) = note.filter(|note| note.launch == current.launches) {
                current.wrapper_pid = Some(note.wrapper_pid);
                current.harness_pid = note.harness_pid;
                current.pgid = note.pgid;
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
