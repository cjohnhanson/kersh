//! Detached terminal sessions for a harness that needs a terminal.
//!
//! The host drives a harness through this trait and nothing else. rmux,
//! pinned at `=0.10.0`, sits only behind it: no other module names an
//! rmux type, so an rmux upgrade or a different multiplexer changes one
//! implementation and no caller. Attaching a person's terminal to a
//! session is a separate client path, not a method here, because it
//! hands the terminal over rather than exchanging data.
//!
//! This release declares the interface only; no implementation exists
//! yet.

use std::future::Future;
use std::path::PathBuf;

/// The terminal size of a session, in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

/// What to start in a new detached session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    /// The session name. It identifies the session in later calls.
    pub name: String,
    /// The program and its arguments. No shell parses them.
    pub argv: Vec<String>,
    /// The whole environment of the program. Nothing is inherited.
    pub env: Vec<(String, String)>,
    /// The working directory of the program.
    pub cwd: PathBuf,
    pub size: Size,
}

/// A session that exists, as [`Sessions::list`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub size: Size,
    /// Whether the program in the session still runs.
    pub alive: bool,
}

/// The narrow session interface the host uses.
pub trait Sessions {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Start `spec.argv` in a new detached session.
    fn create(&self, spec: &SessionSpec) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Every session that exists.
    fn list(&self) -> impl Future<Output = Result<Vec<SessionInfo>, Self::Error>> + Send;

    /// The visible lines of the session's screen, top to bottom.
    fn snapshot(&self, name: &str)
    -> impl Future<Output = Result<Vec<String>, Self::Error>> + Send;

    /// Type `text` into the session, as a person at its terminal would.
    fn send_text(
        &self,
        name: &str,
        text: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// End the session and its program.
    fn kill(&self, name: &str) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
