//! The session calls on [`Client`]: one per session command, with the
//! same data the command prints as JSON.
//!
//! Writes go to the host over `host.sock`; a client that needs a write
//! and finds no host starts one, unless `auto_start: false`. Reads (`ps`,
//! `events`, `screen`) need no host: they read the records and events
//! files and the session server directly. The calls block; an async
//! caller runs them on a blocking thread.

use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Client;
use crate::events::Event;
use crate::paths::Paths;
use crate::proto::{self, Launch, Op, Request};
use crate::runtime::{Config, RuntimeEntry};
use crate::session::{
    AgentRef, HarnessBuild, Mode, Record, Screen, SessionError, StartRequest, State,
};
use crate::setup::SetupReport;

/// Record changes from the host, as [`Client::ps_follow`] yields them:
/// every record first, then each change.
#[derive(Debug)]
pub struct RecordStream {
    reader: std::io::BufReader<UnixStream>,
}

impl Iterator for RecordStream {
    type Item = Result<Record, SessionError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(proto::parse_response(&line).and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|error| SessionError::failed(format!("a bad record: {error}")))
            })),
            Err(error) => Some(Err(SessionError::failed(error.to_string()))),
        }
    }
}

fn from_value<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, SessionError> {
    serde_json::from_value(value)
        .map_err(|error| SessionError::failed(format!("the host sent a bad result: {error}")))
}

fn failed(error: impl std::fmt::Display) -> SessionError {
    SessionError::failed(error.to_string())
}

impl Client {
    /// Use these state and config directories instead of the user's.
    #[must_use]
    pub fn with_paths(mut self, paths: Paths) -> Self {
        self.paths = paths;
        self
    }

    /// The state and config directories this client uses.
    #[must_use]
    pub const fn paths(&self) -> &Paths {
        &self.paths
    }

    fn connect_host(&self, start: bool) -> Result<UnixStream, SessionError> {
        let socket = self.paths.host_sock();
        if let Ok(stream) = UnixStream::connect(&socket) {
            return Ok(stream);
        }
        if !start {
            return Err(SessionError::failed("no host runs"));
        }
        let config = Config::load(&self.paths).map_err(SessionError::failed)?;
        if !config.auto_start() {
            return Err(SessionError::failed(
                "no host runs and auto_start is off; `kersh host` starts one",
            ));
        }
        crate::paths::private_dir(&self.paths.state).map_err(failed)?;
        let mut argv = crate::service::entry();
        argv.extend([
            "host".to_owned(),
            "--state".to_owned(),
            self.paths.state.display().to_string(),
        ]);
        let env: Vec<(String, String)> = crate::login_env::PASSED
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*key).to_owned(), value))
            })
            .chain(std::iter::once((
                "PATH".to_owned(),
                "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
            )))
            .collect();
        crate::service::spawn(
            config.service_manager(),
            &crate::host::label(&self.paths, "host"),
            &argv,
            &env,
            &self.paths.state.join("host.log"),
            &self.paths.state,
        )
        .map_err(SessionError::failed)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                return Ok(stream);
            }
            if Instant::now() > deadline {
                return Err(SessionError::failed(format!(
                    "the host did not start; see {}",
                    self.paths.state.join("host.log").display()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn call(&self, op: Op) -> Result<Value, SessionError> {
        let mut stream = self.connect_host(true)?;
        let mut line = serde_json::to_vec(&Request::new(op)).map_err(failed)?;
        line.push(b'\n');
        stream.write_all(&line).map_err(failed)?;
        let mut reader = std::io::BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).map_err(failed)?;
        if response.is_empty() {
            return Err(SessionError::failed("the host closed the connection"));
        }
        proto::parse_response(&response)
    }

    fn record(&self, id: &str) -> Result<Record, SessionError> {
        let path = self.paths.record(id);
        if !crate::runtime::safe_name(id) || !path.is_file() {
            return Err(SessionError::NotFound { id: id.to_owned() });
        }
        crate::session::read_record(&path)
    }

    /// Sessions, from the records on disk: those named by `ids` (all when
    /// empty), in `state` when given. Each headless record carries its
    /// last progress line. `kersh ps`.
    pub fn ps(&self, ids: &[String], state: Option<State>) -> Result<Vec<Record>, SessionError> {
        let mut records = if ids.is_empty() {
            crate::session::read_records(&self.paths.sessions())?
        } else {
            ids.iter()
                .map(|id| self.record(id))
                .collect::<Result<Vec<_>, _>>()?
        };
        records.retain(|record| state.is_none_or(|state| record.state == state));
        for record in &mut records {
            record.progress = self.progress(record);
        }
        Ok(records)
    }

    fn progress(&self, record: &Record) -> Option<String> {
        if record.mode != Mode::Headless {
            return None;
        }
        crate::events::read(&self.paths.events(&record.id), 0)
            .ok()?
            .pop()
            .map(|event| event.summary)
    }

    /// Every record, then each change, from the host. `kersh ps --follow`.
    pub fn ps_follow(&self) -> Result<RecordStream, SessionError> {
        let mut stream = self.connect_host(true)?;
        let mut line = serde_json::to_vec(&Request::new(Op::Follow)).map_err(failed)?;
        line.push(b'\n');
        stream.write_all(&line).map_err(failed)?;
        Ok(RecordStream {
            reader: std::io::BufReader::new(stream),
        })
    }

    /// Start a session and return its record. `kersh start`.
    pub fn start(&self, request: &StartRequest) -> Result<Record, SessionError> {
        let agent = request
            .agent
            .as_deref()
            .map(|name| self.stores.load(name))
            .transpose()
            .map_err(|error| SessionError::usage(error.to_string()))?;
        if agent.is_none() && request.harness.is_none() && request.runtime.is_none() {
            return Err(SessionError::usage(
                "name an agent, or give --harness or --runtime",
            ));
        }
        let runtimes = crate::runtime::load_runtimes(&self.paths).map_err(SessionError::usage)?;
        let resolved = crate::runtime::resolve(agent.as_ref(), &runtimes, request)
            .map_err(SessionError::usage)?;

        let mut system = String::new();
        if let Some(agent) = &agent {
            system.push_str(&agent.body);
        }
        let skill = request
            .skill
            .clone()
            .or_else(|| agent.as_ref().and_then(|agent| agent.meta.skill.clone()));
        if let Some(skill) = skill {
            let body = crate::skill::resolve(&skill).map_err(SessionError::failed)?;
            if !body.trim().is_empty() {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(body.trim());
            }
        }
        if let Some(file) = &request.instructions_file {
            let text = std::fs::read_to_string(file)
                .map_err(|error| SessionError::usage(format!("{}: {error}", file.display())))?;
            if !text.trim().is_empty() {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(text.trim());
            }
        }

        let cwd = match &request.cwd {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().map_err(failed)?,
        };
        let cwd = std::fs::canonicalize(&cwd)
            .map_err(|error| SessionError::usage(format!("{}: {error}", cwd.display())))?;
        if let Some(name) = &request.name
            && name.trim().is_empty()
        {
            return Err(SessionError::usage("--name is empty"));
        }
        if request.labels.keys().any(String::is_empty) {
            return Err(SessionError::usage(
                "a label needs a key: --label KEY=VALUE",
            ));
        }
        let launch = Launch {
            settings: resolved.settings,
            flags: resolved.flags,
            agent: agent.as_ref().map(|agent| AgentRef {
                name: agent.meta.name.clone(),
                path: self.stores.agent_path(&agent.meta.name).unwrap_or_default(),
                overrides: resolved.overrides.clone(),
            }),
            runtime_name: request
                .runtime
                .clone()
                .or_else(|| agent.as_ref().and_then(|agent| agent.meta.runtime.clone())),
            system_text: (!system.trim().is_empty()).then_some(system),
            cwd,
            labels: request.labels.clone(),
            group: request.group.clone(),
            max_running: request.max_running,
            name: request.name.clone(),
            prompt: request.prompt.clone(),
            resume_conversation: request.resume_conversation.clone(),
            size: request.size,
        };
        from_value(self.call(Op::Start(Box::new(launch)))?)
    }

    /// End a session and close its pane. `kersh stop`.
    pub fn stop(&self, id: &str) -> Result<Record, SessionError> {
        from_value(self.call(Op::Stop { id: id.to_owned() })?)
    }

    /// Continue a held or ended session's conversation in a new pane;
    /// a headless session needs a prompt. `kersh resume`.
    pub fn resume(&self, id: &str, prompt: Option<&str>) -> Result<Record, SessionError> {
        from_value(self.call(Op::Resume {
            id: id.to_owned(),
            prompt: prompt.map(str::to_owned),
        })?)
    }

    /// Restart an interactive session on its conversation. `kersh
    /// restart`.
    pub fn restart(&self, id: &str) -> Result<Record, SessionError> {
        from_value(self.call(Op::Restart { id: id.to_owned() })?)
    }

    /// Type `text` into a running interactive session, then Enter.
    /// `kersh send`.
    pub fn send(&self, id: &str, text: &str) -> Result<(), SessionError> {
        self.call(Op::Send {
            id: id.to_owned(),
            text: text.to_owned(),
        })
        .map(|_| ())
    }

    /// A headless session's events after `since`, from its events file.
    /// `kersh events`.
    pub fn events(&self, id: &str, since: u64) -> Result<Vec<Event>, SessionError> {
        self.record(id)?;
        crate::events::read(&self.paths.events(id), since).map_err(failed)
    }

    /// A session's screen, from the session server. `kersh screen`.
    pub fn screen(&self, id: &str) -> Result<Screen, SessionError> {
        let record = self.record(id)?;
        let (size, lines) =
            crate::mux::screen_direct(&self.paths, &record.rmux_session).map_err(|error| {
                SessionError::failed(format!(
                    "session {id} ({}) has no screen to read: {error}",
                    record.state
                ))
            })?;
        Ok(Screen {
            id: id.to_owned(),
            cols: size.cols,
            rows: size.rows,
            lines,
        })
    }

    /// Delete an ended session's record, events, and temp dir. `kersh
    /// rm`.
    pub fn rm(&self, id: &str) -> Result<(), SessionError> {
        self.call(Op::Rm { id: id.to_owned() }).map(|_| ())
    }

    /// Hand this terminal to the session until `C-]`. `kersh attach`.
    pub fn attach(&self, id: &str) -> Result<(), SessionError> {
        let record = self.record(id)?;
        crate::mux::attach(&self.paths, &record.rmux_session).map_err(SessionError::failed)
    }

    /// The build each harness would run: the host's answer when one runs,
    /// else this process captures the login environment itself. `kersh
    /// harness list`.
    pub fn harness_list(&self) -> Result<Vec<HarnessBuild>, SessionError> {
        if self.connect_host(false).is_ok() {
            return from_value(self.call(Op::HarnessList)?);
        }
        let env =
            crate::login_env::capture(&crate::service::entry()).map_err(SessionError::failed)?;
        Ok(crate::harness::list(&env))
    }

    /// Write an agent file into the user-level store and return its path.
    /// An existing, different file is kept unless `force`. `kersh agent
    /// add`.
    pub fn agent_add(&self, file: &Path, force: bool) -> Result<PathBuf, SessionError> {
        let text = std::fs::read_to_string(file)
            .map_err(|error| SessionError::usage(format!("{}: {error}", file.display())))?;
        let name = crate::agent::declared_name(&text).ok_or_else(|| {
            SessionError::usage(format!(
                "{}: the frontmatter names no agent",
                file.display()
            ))
        })?;
        if !crate::runtime::safe_name(&name) {
            return Err(SessionError::usage(format!(
                "`{name}` is not a valid agent name"
            )));
        }
        let dir = self.paths.user_agents().join(&name);
        let path = dir.join("AGENT.md");
        crate::agent::Agent::parse(&text, &path, &name)
            .map_err(|error| SessionError::usage(error.to_string()))?;
        if let Ok(existing) = std::fs::read_to_string(&path)
            && existing != text
            && !force
        {
            return Err(SessionError::failed(format!(
                "{} exists and differs; --force replaces it",
                path.display()
            )));
        }
        std::fs::create_dir_all(&dir).map_err(failed)?;
        crate::paths::write_private(&path, text.as_bytes()).map_err(failed)?;
        Ok(path)
    }

    /// Add or replace a runtime from a YAML file. `kersh runtime add`.
    pub fn runtime_add(&self, name: &str, file: &Path) -> Result<PathBuf, SessionError> {
        crate::runtime::add(&self.paths, name, file).map_err(SessionError::usage)
    }

    /// Every runtime. `kersh runtime list`.
    pub fn runtime_list(&self) -> Result<Vec<RuntimeEntry>, SessionError> {
        crate::runtime::list(&self.paths).map_err(SessionError::failed)
    }

    /// Run gaff's registration for `harness` and check each piece of its
    /// wiring. `kersh setup`.
    pub fn setup(&self, harness: crate::session::Harness) -> Result<SetupReport, SessionError> {
        let cwd = std::env::current_dir().map_err(failed)?;
        crate::setup::setup(harness, &cwd).map_err(SessionError::usage)
    }

    /// The agent-memory block, from `root` or the configured store.
    /// `kersh memories`.
    pub fn memories(&self, root: Option<&Path>) -> Result<String, SessionError> {
        let config = Config::load(&self.paths).map_err(SessionError::failed)?;
        let root = root.map(Path::to_path_buf).or(config.memory.root);
        crate::memories::generate(root.as_deref()).map_err(SessionError::failed)
    }
}
