//! The client protocol on `host.sock`: one JSON request per line, one
//! JSON response per line (a follow keeps sending lines).
//!
//! Every message carries the protocol schema and the sender's kersh
//! version. The host refuses a request of another schema, and a client
//! refuses a response of a newer one, each naming both versions, so two
//! builds never act on messages they may not understand.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::{AgentRef, PaneSize, SessionError, Settings};

/// The protocol schema this build speaks.
pub const PROTOCOL_SCHEMA: u32 = 1;

/// The version every message names.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A request line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub schema: u32,
    pub version: String,
    #[serde(flatten)]
    pub op: Op,
}

impl Request {
    #[must_use]
    pub fn new(op: Op) -> Self {
        Self {
            schema: PROTOCOL_SCHEMA,
            version: VERSION.to_owned(),
            op,
        }
    }
}

/// What a client asks the host to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    /// Stream every record, then each change.
    Follow,
    Start(Box<Launch>),
    Stop {
        id: String,
    },
    Resume {
        id: String,
        prompt: Option<String>,
    },
    Restart {
        id: String,
    },
    Send {
        id: String,
        text: String,
    },
    Rm {
        id: String,
    },
    HarnessList,
    /// RET in a held pane: run the held command, gated like a resume.
    HeldReturn {
        id: String,
    },
    /// Capture a session's screen now, as after a detach.
    Capture {
        id: String,
    },
    /// Stop the session server, start a new one, and restore every
    /// session.
    RestartMux,
}

/// A start, resolved by the client from the agent file and runtimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub settings: Settings,
    pub flags: Vec<String>,
    pub agent: Option<AgentRef>,
    pub runtime_name: Option<String>,
    /// The composed body, skill, and instructions.
    pub system_text: Option<String>,
    pub cwd: PathBuf,
    pub labels: BTreeMap<String, String>,
    pub group: Option<String>,
    pub max_running: Option<u32>,
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub resume_conversation: Option<String>,
    pub size: Option<PaneSize>,
}

/// A response line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub schema: u32,
    pub version: String,
    #[serde(flatten)]
    pub body: Body,
}

/// A result or an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    Ok(Value),
    Err(SessionError),
}

impl Response {
    #[must_use]
    pub fn ok(value: Value) -> Self {
        Self {
            schema: PROTOCOL_SCHEMA,
            version: VERSION.to_owned(),
            body: Body::Ok(value),
        }
    }

    #[must_use]
    pub fn err(error: SessionError) -> Self {
        Self {
            schema: PROTOCOL_SCHEMA,
            version: VERSION.to_owned(),
            body: Body::Err(error),
        }
    }
}

/// Check a message's schema against this build's.
pub fn check_schema(what: &str, schema: u32, version: &str) -> Result<(), SessionError> {
    if schema == PROTOCOL_SCHEMA {
        return Ok(());
    }
    Err(SessionError::Schema {
        what: format!("{what} from kersh {version}"),
        found: schema,
        supported: PROTOCOL_SCHEMA,
        version: VERSION.to_owned(),
    })
}

/// Parse a request line, refusing another schema before the operation is
/// read.
pub fn parse_request(line: &str) -> Result<Request, SessionError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| SessionError::usage(format!("not a request: {error}")))?;
    let schema = value
        .get("schema")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    check_schema("the request", schema, &version)?;
    serde_json::from_value(value)
        .map_err(|error| SessionError::usage(format!("not a request: {error}")))
}

/// Parse a response line, refusing another schema.
pub fn parse_response(line: &str) -> Result<Value, SessionError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| SessionError::failed(format!("the host sent a bad line: {error}")))?;
    let schema = value
        .get("schema")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    check_schema("the host's response", schema, &version)?;
    let response: Response = serde_json::from_value(value)
        .map_err(|error| SessionError::failed(format!("the host sent a bad line: {error}")))?;
    match response.body {
        Body::Ok(value) => Ok(value),
        Body::Err(error) => Err(error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_with_its_operation_inline() {
        let line = serde_json::to_string(&Request::new(Op::Stop { id: "a".to_owned() })).unwrap();
        assert!(line.contains(r#""op":"stop""#), "{line}");
        let parsed = parse_request(&line).unwrap();
        assert_eq!(parsed.op, Op::Stop { id: "a".to_owned() });
    }

    #[test]
    fn another_schema_is_refused_naming_both_versions() {
        let error = parse_request(r#"{"schema":2,"version":"9.0.0","op":"ping"}"#).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("9.0.0") && text.contains(VERSION), "{text}");
        let error = parse_response(r#"{"schema":7,"version":"9.0.0","ok":null}"#).unwrap_err();
        assert!(
            matches!(error, SessionError::Schema { found: 7, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_error_response_comes_back_as_the_error() {
        let line = serde_json::to_string(&Response::err(SessionError::NotFound {
            id: "x".to_owned(),
        }))
        .unwrap();
        assert_eq!(
            parse_response(&line).unwrap_err(),
            SessionError::NotFound { id: "x".to_owned() }
        );
    }
}
