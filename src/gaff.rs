//! The gaff integration: a run's guards, context, and stop rule.
//!
//! kersh does not link gaff. It calls the `gaff hook` binary with
//! `GAFF_HOST=generic` and a payload in gaff's normalized vocabulary, and
//! reads the exit code and the streams. gaff refuses a tool call or a stop
//! with exit 2 and the reason on stderr, injects context on stdout at a
//! flush point, and degrades to a no-op with no config.
//!
//! Governance is opt-in: it is active only when the agent file declares a
//! `profile`. The profile is selected with `GAFF_PROFILE`, which gaff
//! honors from the environment only when `transitions.agent_may_set`
//! names the profile, so an operator opts a bundle into kersh use.
//!
//! # What does not apply
//!
//! A gaff base guard written against another host's tool names, such as
//! Claude Code's `Read`, does not match kersh's `read_file` and does not
//! protect a kersh agent. Only a profile guard written against kersh's
//! own tool names applies. gaff injects context only at a flush point,
//! and kersh flushes only at session start, so a bundle cannot inject
//! context mid-run in this release.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

/// The deadline for one `gaff hook` call. A hook answers in well under a
/// second, so this bounds a wedged gaff, not real work.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// The normalized event names gaff's generic host recognizes. A name that
/// gaff does not recognize is a no-op there, so a guard on it would not
/// fire. These strings are the enforcement contract; gaff's own `generic`
/// tests pin the same names on its side. Verified against gaff: a
/// `pre_tool_call` on a guarded tool exits 2.
pub const SESSION_START: &str = "session_start";
pub const PRE_TOOL_CALL: &str = "pre_tool_call";
pub const TOOL_CALL: &str = "tool_call";
pub const STOP: &str = "stop";

/// The run-scoped gaff context: the minted session id and the profile.
#[derive(Debug, Clone)]
pub struct Gaff {
    pub binary: String,
    pub session_id: String,
    pub profile: String,
}

/// The context injected at session start, if any.
#[derive(Debug, Deserialize)]
struct ContextOutput {
    #[serde(default)]
    context: String,
}

/// The outcome of one `gaff hook` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The call was allowed. The string is any injected context (empty
    /// for a non-flush event).
    Allow(String),
    /// A guard or a hold refused, with the reason.
    Refuse(String),
    /// gaff could not answer: absent, timed out, or an unexpected exit.
    /// Every tool call fails closed on this, and the up-front probe
    /// aborts the run on it.
    Failed(String),
}

/// Build the gaff context for an agent, or `None` when the agent
/// declares no profile. The binary is `KERSH_GAFF` or `gaff` on `PATH`.
#[must_use]
pub fn for_agent(agent: &crate::agent::Agent) -> Option<Gaff> {
    let profile = agent.meta.profile.clone()?;
    let binary = std::env::var("KERSH_GAFF").unwrap_or_else(|_| "gaff".to_owned());
    Some(Gaff {
        binary,
        session_id: crate::util::nonce(),
        profile,
    })
}

impl Gaff {
    /// Call `gaff hook` for `event`, with an optional tool call.
    ///
    /// `tool` is `(name, arguments-json)` for a `pre_tool_call`, so a
    /// guard can read the tool and its fields.
    pub async fn hook(&self, event: &str, tool: Option<(&str, &str)>) -> Outcome {
        let mut map = serde_json::Map::new();
        map.insert("gaff_event".to_owned(), event.into());
        map.insert("session_id".to_owned(), self.session_id.clone().into());
        if let Some((name, args)) = tool {
            map.insert("tool_name".to_owned(), name.into());
            // The arguments are a JSON object; a guard reads a field of it.
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(args) {
                map.insert("tool_input".to_owned(), input);
            }
        }
        let payload = serde_json::Value::Object(map);

        let mut command = tokio::process::Command::new(&self.binary);
        command
            .arg("hook")
            .env("GAFF_HOST", "generic")
            .env("GAFF_PROFILE", &self.profile)
            .env("GAFF_SESSION_ID", &self.session_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let run = async {
            use tokio::io::AsyncWriteExt as _;
            let mut child = command.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.to_string().as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
            child.wait_with_output().await
        };

        match tokio::time::timeout(HOOK_TIMEOUT, run).await {
            Err(_) => Outcome::Failed(format!(
                "`{} hook` did not answer within {HOOK_TIMEOUT:?}",
                self.binary
            )),
            Ok(Err(error)) => {
                Outcome::Failed(format!("cannot run `{} hook`: {error}", self.binary))
            }
            Ok(Ok(output)) => classify(&output),
        }
    }
}

/// Map a finished `gaff hook` process to an outcome.
///
/// Exit 0 is allowed, with any stdout context. Exit 2 is a refusal, with
/// the reason from stderr. Any other exit is a failure, because gaff's own
/// contract is that only a guard or a hold exits 2.
fn classify(output: &std::process::Output) -> Outcome {
    match output.status.code() {
        Some(0) => Outcome::Allow(parse_context(&output.stdout)),
        Some(2) => {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Outcome::Refuse(if reason.is_empty() {
                "refused by a gaff guard".to_owned()
            } else {
                reason
            })
        }
        other => {
            let code = other.map_or_else(|| "a signal".to_owned(), |c| c.to_string());
            Outcome::Failed(format!(
                "`gaff hook` exited with {code}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// The `context` field of gaff's `{event, context}` output, or empty.
fn parse_context(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    serde_json::from_str::<ContextOutput>(trimmed)
        .map(|c| c.context)
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{ExitStatus, Output};

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn exit_zero_is_allowed_with_the_parsed_context() {
        let out = output(
            0,
            r#"{"event":"session_start","context":"HOUSE RULES"}"#,
            "",
        );
        assert_eq!(classify(&out), Outcome::Allow("HOUSE RULES".to_owned()));
    }

    #[test]
    fn empty_stdout_is_allowed_with_no_context() {
        assert_eq!(classify(&output(0, "", "")), Outcome::Allow(String::new()));
    }

    #[test]
    fn exit_two_is_a_refusal_with_the_stderr_reason() {
        let out = output(
            2,
            "",
            "gaff: refused by the guard `no-secrets`.\n\nThat path holds secrets.",
        );
        match classify(&out) {
            Outcome::Refuse(reason) => assert!(reason.contains("secrets"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn any_other_exit_is_a_failure() {
        assert!(matches!(
            classify(&output(1, "", "boom")),
            Outcome::Failed(_)
        ));
    }
}
