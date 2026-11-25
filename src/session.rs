//! The session record and the other types the session calls exchange.
//!
//! A record is one JSON file, `sessions/<id>/record.json`, and the host is
//! its only writer. Every record carries a schema number; a reader that
//! finds a newer schema than it knows refuses, naming both, instead of
//! acting on state it may not understand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The record schema this build writes and reads.
pub const RECORD_SCHEMA: u32 = 1;

/// Which program a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    /// kersh's built-in harness.
    Rig,
    ClaudeCode,
    Codex,
}

impl Harness {
    /// The name used on the command line and in files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rig => "rig",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// The program the harness runs, looked up on the login `PATH`.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Rig => "kersh",
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    /// The default mode: headless for rig, interactive for the others.
    #[must_use]
    pub const fn default_mode(self) -> Mode {
        match self {
            Self::Rig => Mode::Headless,
            Self::ClaudeCode | Self::Codex => Mode::Interactive,
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Harness {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "rig" => Ok(Self::Rig),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            other => Err(format!(
                "`{other}` is not a harness; use rig, claude-code, or codex"
            )),
        }
    }
}

/// Whether kersh reads structured events (headless) or hosts a TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Headless,
    Interactive,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Interactive => "interactive",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "headless" => Ok(Self::Headless),
            "interactive" => Ok(Self::Interactive),
            other => Err(format!(
                "`{other}` is not a mode; use headless or interactive"
            )),
        }
    }
}

/// A session's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The harness runs, or its wrapper left it for restore after a signal.
    Running,
    /// The pane stays open after a failure or an interrupt, waiting for a
    /// person or `kersh resume`.
    Held,
    /// The harness exited 0, or the session was stopped.
    Ended,
    /// The session vanished without an exit note.
    Lost,
}

impl State {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Held => "held",
            Self::Ended => "ended",
            Self::Lost => "lost",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for State {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "running" => Ok(Self::Running),
            "held" => Ok(Self::Held),
            "ended" => Ok(Self::Ended),
            "lost" => Ok(Self::Lost),
            other => Err(format!(
                "`{other}` is not a state; use running, held, ended, or lost"
            )),
        }
    }
}

/// Which build of the harness a session runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Build {
    /// How the build was found: `path`, the login `PATH`.
    pub installer: String,
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// The first line `<program> --version` printed.
    pub version: String,
}

/// What the environment prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnvironmentInfo {
    /// `local`.
    pub kind: String,
    #[schema(value_type = String)]
    pub cwd: PathBuf,
    /// The session's temp dir, kept while the record exists.
    #[schema(value_type = Option<String>)]
    pub tempdir: Option<PathBuf>,
}

/// The agent file a session started from, and the overrides given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentRef {
    pub name: String,
    /// The agent file that loaded.
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// The `kersh start` flags that overrode the files.
    pub overrides: BTreeMap<String, String>,
}

/// The resolved settings a session launched with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Settings {
    pub harness: Harness,
    pub mode: Mode,
    pub model: Option<String>,
    pub environment: String,
    /// The gaff profile, set in the pane as `GAFF_PROFILE`.
    pub profile: Option<String>,
    pub resume_prompt: Option<String>,
    pub tempdir: bool,
    /// Literal variables from the runtime.
    pub env: BTreeMap<String, String>,
}

/// The runtime a session used: its name, when one was named, and the
/// settings resolved at launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RuntimeInfo {
    pub name: Option<String>,
    pub settings: Settings,
}

/// Where a conversation id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSource {
    /// kersh set it at launch (Claude Code's `--session-id`).
    Launch,
    /// The harness printed it (Codex's `thread.started`).
    Stream,
    /// The rollout file the harness's own process holds open (Codex).
    Rollout,
    /// The caller named it (`--resume-conversation`).
    Caller,
}

/// The harness's own conversation, for resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Conversation {
    pub id: String,
    pub source: ConversationSource,
}

/// How a restore left a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RestoreOutcome {
    /// The harness resumed its conversation.
    Resumed,
    /// The pane holds its command until a person decides.
    Held,
}

/// One restore of a session, newest last on the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Restore {
    pub at: String,
    /// What took the session down: `a reboot`, `the session server
    /// stopped`, `kersh restart --mux`, or the wrapper's own reason.
    pub after: String,
    pub outcome: RestoreOutcome,
    /// The build change, when the harness build differs from the
    /// recorded one: `2.1.0 -> 2.2.0`.
    pub build_change: Option<String>,
    /// The line a front end shows: `restored at <time> after <cause>;
    /// tools that were running did not finish`.
    pub line: String,
}

/// What RET runs in a held pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeldRun {
    /// The first launch again: a new conversation with the first prompt.
    Fresh,
    /// The harness's resume of the recorded conversation.
    Resume,
}

/// A pane size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PaneSize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for PaneSize {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 36,
        }
    }
}

/// One session, as the host records it and `kersh ps --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Record {
    pub schema: u32,
    /// Also `KERSH_SESSION` in the pane.
    pub id: String,
    pub name: Option<String>,
    pub harness: Harness,
    pub mode: Mode,
    pub state: State,
    pub build: Build,
    pub environment: EnvironmentInfo,
    pub agent: Option<AgentRef>,
    pub runtime: RuntimeInfo,
    /// The composed body and skill, passed again on every resume.
    #[schema(value_type = Option<String>)]
    pub system_prompt: Option<PathBuf>,
    pub conversation: Option<Conversation>,
    /// Persistent harness flags, checked against the harness's allow-list.
    pub agent_flags: Vec<String>,
    /// The first prompt; never replayed without a person's say.
    #[schema(value_type = Option<String>)]
    pub first_prompt: Option<PathBuf>,
    /// Opaque to kersh: stored and echoed, never interpreted.
    pub labels: BTreeMap<String, String>,
    pub group: Option<String>,
    pub pgid: Option<i32>,
    /// The launch whose wrapper reported `pgid`. Absent, an earlier
    /// launch's.
    #[serde(default)]
    pub pgid_launch: Option<u32>,
    pub boot_id: Option<String>,
    pub wrapper_pid: Option<i32>,
    pub harness_pid: Option<i32>,
    pub rmux_session: String,
    pub pane_size: PaneSize,
    pub created_at: String,
    pub ended_at: Option<String>,
    pub exit_status: Option<i32>,
    /// Why the session is held or ended: `exit status 3`, `interrupted`,
    /// `stopped`.
    pub reason: Option<String>,
    /// Every restore, oldest first. An older record's count of restores
    /// reads as none.
    #[serde(default, deserialize_with = "restores_or_count")]
    pub restores: Vec<Restore>,
    /// What RET runs while the session is held, when RET runs anything.
    #[serde(default)]
    pub held_run: Option<HeldRun>,
    /// How many times the session was resumed or restarted.
    pub launches: u32,
    /// The last progress line, filled when a record is read for a list.
    /// Never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
}

/// What `kersh start` and [`crate::Client::start`] take. Everything but
/// the harness is optional; an agent file and runtimes fill the rest.
/// Every field may be left out of its JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct StartRequest {
    /// An agent file name. Without one, `harness` is required.
    pub agent: Option<String>,
    /// A runtime from `runtimes.yml`, applied over the agent's own.
    pub runtime: Option<String>,
    pub harness: Option<Harness>,
    pub mode: Option<Mode>,
    pub model: Option<String>,
    /// Replaces the agent's almanac skill.
    pub skill: Option<String>,
    pub profile: Option<String>,
    /// Added to the agent's body for this session.
    #[schema(value_type = Option<String>)]
    pub instructions_file: Option<PathBuf>,
    /// The working directory; the caller's when absent.
    #[schema(value_type = Option<String>)]
    pub cwd: Option<PathBuf>,
    pub tempdir: bool,
    pub labels: BTreeMap<String, String>,
    pub group: Option<String>,
    pub max_running: Option<u32>,
    pub name: Option<String>,
    /// The first prompt. A headless start requires one.
    pub prompt: Option<String>,
    /// Resume this harness conversation instead of starting a new one.
    pub resume_conversation: Option<String>,
    /// Flags passed to the harness, checked against its allow-list.
    pub harness_flags: Vec<String>,
    /// The requesting terminal's size; the configured default when absent.
    pub size: Option<PaneSize>,
}

/// A session's screen, as `kersh screen --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Screen {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<String>,
}

/// One harness build, as `kersh harness list --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HarnessBuild {
    pub harness: Harness,
    /// `path`.
    pub installer: String,
    #[schema(value_type = Option<String>)]
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    /// Why no build was found, when none was.
    pub error: Option<String>,
}

/// Why a session call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, ToSchema)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SessionError {
    /// A start or resume met its group's limit. Exit 3.
    #[error("group `{group}` has {running} of {max} sessions running")]
    AtLimit {
        group: String,
        running: u32,
        max: u32,
    },
    /// The request was malformed. Exit 2.
    #[error("{message}")]
    Usage { message: String },
    #[error("no session `{id}`")]
    NotFound { id: String },
    /// A file or message carries a schema this build does not know.
    #[error(
        "{what} has schema {found}, but this kersh {version} knows schema {supported}; \
         use a kersh that matches the one that wrote it"
    )]
    Schema {
        what: String,
        found: u32,
        supported: u32,
        version: String,
    },
    #[error("{message}")]
    Failed { message: String },
}

impl SessionError {
    /// A plain failure.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    /// A usage error.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage {
            message: message.into(),
        }
    }

    /// The command's exit code: 3 at a limit, 2 for usage, 1 otherwise.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::AtLimit { .. } => 3,
            Self::Usage { .. } => 2,
            Self::NotFound { .. } | Self::Schema { .. } | Self::Failed { .. } => 1,
        }
    }
}

/// The restores of a record, or none for an older record's integer
/// count of them.
fn restores_or_count<'de, D>(deserializer: D) -> Result<Vec<Restore>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Restores {
        List(Vec<Restore>),
        #[allow(dead_code)] // The count is read only to accept its form.
        Count(u64),
    }
    Ok(match Restores::deserialize(deserializer)? {
        Restores::List(list) => list,
        Restores::Count(_) => Vec::new(),
    })
}

/// Read one record, refusing a schema newer than this build's.
pub fn read_record(path: &Path) -> Result<Record, SessionError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| SessionError::failed(format!("{}: {error}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| SessionError::failed(format!("{}: {error}", path.display())))?;
    let found = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    if found != RECORD_SCHEMA {
        return Err(SessionError::Schema {
            what: format!("the record {}", path.display()),
            found,
            supported: RECORD_SCHEMA,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        });
    }
    serde_json::from_value(value)
        .map_err(|error| SessionError::failed(format!("{}: {error}", path.display())))
}

/// Every record under `sessions`, sorted by creation time then id.
/// One record that does not read is an error.
pub fn read_records(sessions: &Path) -> Result<Vec<Record>, SessionError> {
    let (records, mut unread) = read_records_skipping(sessions);
    match unread.drain(..).next() {
        Some((_, error)) => Err(error),
        None => Ok(records),
    }
}

/// Every record under `sessions` that reads, sorted by creation time
/// then id, and the path and error of each that does not.
#[must_use]
pub fn read_records_skipping(sessions: &Path) -> (Vec<Record>, Vec<(PathBuf, SessionError)>) {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return (Vec::new(), Vec::new());
    };
    let mut records = Vec::new();
    let mut unread = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join("record.json");
        if path.is_file() {
            match read_record(&path) {
                Ok(record) => records.push(record),
                Err(error) => unread.push((path, error)),
            }
        }
    }
    records.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    (records, unread)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_record_that_counts_its_restores_loads_with_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a");
        std::fs::create_dir_all(&dir).unwrap();
        let record = serde_json::json!({
            "schema": RECORD_SCHEMA,
            "id": "a", "name": null, "harness": "claude-code", "mode": "interactive",
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
            "rmux_session": "kersh-a",
            "pane_size": {"cols": 80, "rows": 24},
            "created_at": "2026-11-01T00:00:00.000Z", "ended_at": null,
            "exit_status": null, "reason": null, "restores": 0, "launches": 1
        });
        std::fs::write(dir.join("record.json"), record.to_string()).unwrap();
        let records = read_records(tmp.path()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records.first().unwrap().restores.is_empty());
    }

    #[test]
    fn a_newer_record_schema_is_refused_naming_both() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("record.json");
        std::fs::write(&path, r#"{"schema": 99, "id": "x"}"#).unwrap();
        let error = read_record(&path).unwrap_err();
        let SessionError::Schema {
            found, supported, ..
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!((*found, *supported), (99, RECORD_SCHEMA));
        assert!(error.to_string().contains("schema 99"), "{error}");
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn names_round_trip() {
        for harness in [Harness::Rig, Harness::ClaudeCode, Harness::Codex] {
            assert_eq!(harness.as_str().parse::<Harness>().unwrap(), harness);
        }
        assert_eq!(
            serde_json::to_string(&Harness::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert_eq!("held".parse::<State>().unwrap(), State::Held);
    }

    #[test]
    fn an_error_serializes_with_its_code() {
        let error = SessionError::AtLimit {
            group: "g".to_owned(),
            running: 2,
            max: 2,
        };
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"code":"at_limit","group":"g","running":2,"max":2}"#
        );
        assert_eq!(error.exit_code(), 3);
    }
}
