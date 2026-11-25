//! The protocol on `host.sock`: HTTP/1.1 with JSON bodies, described by
//! the `OpenAPI` document the host serves at `/openapi.json`.
//!
//! One protocol serves both kinds of caller. [`crate::Client`] speaks it,
//! and so does anything else of this user that can reach the socket
//! (`curl --unix-socket`). A body is the JSON the matching command prints
//! with `--json`; a list is an array of the objects the command prints
//! one per line; an error is a [`SessionError`] with its `code`.
//!
//! Every request and response names the protocol schema and the
//! sender's kersh version in the `kersh-schema` and `kersh-version`
//! headers. The host refuses a request of another schema with a 400 that
//! names both versions. A request without the headers, from a shell, is
//! taken as this build's. The client refuses a response without this
//! build's schema header.
//!
//! Before it sends anything, the client reads the schema in `host.lock`.
//! A running host of another schema is refused there, with both
//! versions and the host's pid to stop. This catches a schema 1 host,
//! which reads JSON lines and cannot answer HTTP. A schema 1 client gets
//! only the 400 of an HTTP host, a reply it cannot read.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::session::{AgentRef, PaneSize, SessionError, Settings};

/// The protocol schema this build speaks. Schema 1 was one JSON line per
/// request; schema 2 is HTTP.
pub const PROTOCOL_SCHEMA: u32 = 2;

/// The version every message names.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The header that carries the protocol schema.
pub const SCHEMA_HEADER: &str = "kersh-schema";

/// The header that carries the sender's kersh version.
pub const VERSION_HEADER: &str = "kersh-version";

/// The authority in request URLs. A request reaches only the socket the
/// client was given, so the name resolves nowhere.
pub const AUTHORITY: &str = "kersh";

/// The media type of a stream of JSON objects, one per line.
pub const NDJSON: &str = "application/x-ndjson";

/// A start, resolved by the client from the agent file and runtimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Launch {
    pub settings: Settings,
    pub flags: Vec<String>,
    pub agent: Option<AgentRef>,
    pub runtime_name: Option<String>,
    /// The composed body, skill, and instructions.
    pub system_text: Option<String>,
    #[schema(value_type = String)]
    pub cwd: PathBuf,
    pub labels: BTreeMap<String, String>,
    pub group: Option<String>,
    pub max_running: Option<u32>,
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub resume_conversation: Option<String>,
    pub size: Option<PaneSize>,
}

/// The body of a resume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResumeBody {
    /// The prompt. A headless session needs one.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// The body of a send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SendBody {
    /// The text typed into the session, followed by Enter.
    pub text: String,
}

/// What `GET /v1/version` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VersionInfo {
    pub version: String,
    pub schema: u32,
}

/// The HTTP status of an error: 400 for usage and schema, 404 for a
/// missing session, 409 at a group's limit, 500 otherwise.
#[must_use]
pub const fn status_of(error: &SessionError) -> u16 {
    match error {
        SessionError::Usage { .. } | SessionError::Schema { .. } => 400,
        SessionError::NotFound { .. } => 404,
        SessionError::AtLimit { .. } => 409,
        SessionError::Failed { .. } => 500,
    }
}

/// Check a message's schema header against this build's. A missing
/// header is this build's schema.
pub fn check_schema(
    what: &str,
    schema: Option<&str>,
    version: Option<&str>,
) -> Result<(), SessionError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let found = schema.trim().parse::<u32>().unwrap_or(0);
    if found == PROTOCOL_SCHEMA {
        return Ok(());
    }
    Err(SessionError::Schema {
        what: format!("{what} from kersh {}", version.unwrap_or("unknown")),
        found,
        supported: PROTOCOL_SCHEMA,
        version: VERSION.to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn another_schema_is_refused_naming_both_versions() {
        let error = check_schema("the request", Some("1"), Some("9.0.0")).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("9.0.0") && text.contains(VERSION), "{text}");
        assert!(
            matches!(error, SessionError::Schema { found: 1, .. }),
            "{error:?}"
        );
        assert!(check_schema("x", Some(&PROTOCOL_SCHEMA.to_string()), None).is_ok());
        assert!(check_schema("x", None, None).is_ok());
    }

    #[test]
    fn each_error_has_its_status() {
        assert_eq!(status_of(&SessionError::usage("u")), 400);
        assert_eq!(
            status_of(&SessionError::NotFound { id: "x".to_owned() }),
            404
        );
        assert_eq!(
            status_of(&SessionError::AtLimit {
                group: "g".to_owned(),
                running: 1,
                max: 1
            }),
            409
        );
        assert_eq!(status_of(&SessionError::failed("f")), 500);
    }
}
