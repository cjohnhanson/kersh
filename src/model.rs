//! Running an agent through rig.
//!
//! The agent gets one tool, a shell, confined to the run's root. The gaff
//! profile decides what it may run.
//! The provider is chosen from the model's prefix: `claude-code/<model>`
//! runs the local `claude` CLI, and `anthropic/<model>` uses an API key.
//! The agent's system prompt is its preamble, and the composed first turn
//! is the prompt.

use std::sync::Arc;

use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};
use rig::prelude::*;
use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use crate::agent::Agent;
use crate::tools::{Root, ToolError};

/// Why a run could not start or finish.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("the model provider `{0}` is unknown; use `claude-code` or `anthropic`")]
    UnknownProvider(String),
    #[error("cannot start the {provider} provider: {message}")]
    Provider { provider: String, message: String },
    #[error("the agent's root is not usable: {0}")]
    Root(#[from] ToolError),
    #[error("the run did not finish within {0:?}")]
    TimedOut(std::time::Duration),
    #[error("gaff governance is on for this agent, but gaff could not start: {0}")]
    Gaff(String),
    #[error("the run failed: {0}")]
    Prompt(String),
}

/// Run `agent` with `system` as its preamble and `first_turn` as its
/// prompt, its tools confined to `root`.
///
/// When `gaff` is set, the run is governed: a rig hook calls `gaff hook`
/// at each tool call and at each stop, so the profile's guards and stop
/// rule apply. The caller has already run the session-start probe.
pub async fn run(
    agent: &Agent,
    root: Root,
    system: String,
    first_turn: String,
    gaff: Option<crate::gaff::Gaff>,
) -> Result<String, RunError> {
    let root = Arc::new(root);
    let per_call = agent.meta.timeout.0;
    // The whole run is bounded by the per-call deadline times the turn cap,
    // plus a margin that also covers the gaff calls each turn adds. rig
    // caps the turn count, not the wall clock, so a hung provider would
    // otherwise block forever.
    let turns = u32::try_from(agent.meta.max_turns).unwrap_or(u32::MAX);
    let gaff_margin =
        crate::gaff::HOOK_TIMEOUT.saturating_mul(turns.saturating_mul(3).saturating_add(1));
    let whole_run = per_call
        .saturating_mul(turns.saturating_add(1))
        .saturating_add(gaff_margin);

    // Claude Code attaches its working directory's files and every
    // CLAUDE.md up the tree once it has tools, which a large repo balloons
    // past the token limit. The agent's bash tool runs in the confined root,
    // so claude needs no project directory. Run it in a fresh directory
    // under the temp root, outside the home tree, so no ambient CLAUDE.md
    // loads. The directory is removed after the run.
    let claude_cwd = std::env::temp_dir().join(format!("kersh-claude-{}", crate::util::nonce()));
    let _ = std::fs::create_dir_all(&claude_cwd);
    let claude_cwd_str = claude_cwd.to_string_lossy().into_owned();

    let work = async {
        match agent.provider() {
            "claude-code" => {
                let client = rig_claude_code::ClaudeCodeClient::from_env()
                    .map_err(|error| RunError::Provider {
                        provider: "claude-code".to_owned(),
                        message: error.to_string(),
                    })?
                    .with_current_dir(claude_cwd_str.clone())
                    .with_timeout(per_call);
                let built = configure(
                    client.agent(agent.model_id()),
                    &system,
                    &root,
                    agent.meta.max_turns,
                    gaff,
                );
                drive(built, first_turn).await
            }
            "anthropic" => {
                let client = rig::providers::anthropic::Client::from_env().map_err(|error| {
                    RunError::Provider {
                        provider: "anthropic".to_owned(),
                        message: error.to_string(),
                    }
                })?;
                let built = configure(
                    client.agent(agent.model_id()),
                    &system,
                    &root,
                    agent.meta.max_turns,
                    gaff,
                );
                drive(built, first_turn).await
            }
            #[cfg(feature = "fake-model")]
            "fake" => {
                let model = crate::fake_model::FakeModel::from_env().map_err(|message| {
                    RunError::Provider {
                        provider: "fake".to_owned(),
                        message,
                    }
                })?;
                let built = configure(
                    AgentBuilder::new(model),
                    &system,
                    &root,
                    agent.meta.max_turns,
                    gaff,
                );
                drive(built, first_turn).await
            }
            other => Err(RunError::UnknownProvider(other.to_owned())),
        }
    };

    let outcome = match tokio::time::timeout(whole_run, work).await {
        Ok(result) => result,
        Err(_) => Err(RunError::TimedOut(whole_run)),
    };
    let _ = std::fs::remove_dir_all(&claude_cwd);
    outcome
}

/// Add the bash tool, the preamble, the turn cap, and the gaff hook to
/// any provider's agent builder, then build it.
fn configure<M: CompletionModel + 'static>(
    builder: AgentBuilder<M>,
    system: &str,
    root: &Arc<Root>,
    max_turns: usize,
    gaff: Option<crate::gaff::Gaff>,
) -> rig::agent::Agent<M> {
    let builder = builder
        .preamble(system)
        .tool(Bash(Arc::clone(root)))
        .default_max_turns(max_turns);
    let builder = match gaff {
        Some(gaff) => builder.add_hook(crate::hook::GaffHook::new(gaff)),
        None => builder,
    };
    builder.build()
}

/// Prompt the agent and return its final text.
async fn drive<M: CompletionModel + 'static>(
    agent: rig::agent::Agent<M>,
    prompt: String,
) -> Result<String, RunError> {
    agent
        .prompt(prompt)
        .await
        .map_err(|error| RunError::Prompt(error.to_string()))
}

// The one tool an agent gets: a shell. kersh does not decide what it may
// run; the gaff profile does, through the pre-tool-call hook. The command
// runs in the confined root. A single command tool carries none of the
// working directory into the model's request, unlike a per-file tool
// bridged over the root.

/// The per-command deadline. A read command answers well within it.
const BASH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The most output bytes one command returns to the model.
const BASH_OUTPUT_CAP: usize = 64 * 1024;

struct Bash(Arc<Root>);

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

impl Tool for Bash {
    const NAME: &'static str = "bash";
    type Args = BashArgs;
    type Output = String;
    type Error = ToolError;

    fn description(&self) -> String {
        "Run a shell command in the working directory and return its output. \
         Read with it: cat, ls, rg, git diff, and so on."
            .to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(self.0.path())
            .stdin(std::process::Stdio::null())
            .output();
        match tokio::time::timeout(BASH_TIMEOUT, run).await {
            Err(_) => Ok(format!(
                "(the command did not finish within {BASH_TIMEOUT:?})"
            )),
            Ok(Err(source)) => Err(ToolError::Io {
                path: "sh".to_owned(),
                source,
            }),
            Ok(Ok(output)) => Ok(cap_output(&output)),
        }
    }
}

/// Join a command's stdout and stderr, note a non-zero exit, and cap the
/// whole to [`BASH_OUTPUT_CAP`] on a character boundary, so the model
/// reads a failure's message and never an unbounded dump.
fn cap_output(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if !output.status.success() {
        use std::fmt::Write as _;
        let code = output
            .status
            .code()
            .map_or_else(|| "a signal".to_owned(), |c| c.to_string());
        let _ = write!(text, "\n(the command exited with {code})");
    }
    if text.len() > BASH_OUTPUT_CAP {
        let mut end = BASH_OUTPUT_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n(output truncated)");
    }
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::Path;

    fn agent(model: &str) -> Agent {
        Agent::parse(
            &format!("---\nname: a\nmodel: {model}\n---\nbody"),
            Path::new("x"),
            "a",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_unknown_provider_is_named_not_panicked() {
        // `model_is_safe` allows a slashed value with any lowercase halves,
        // so an unknown provider reaches the run loop and is named.
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path()).unwrap();
        let error = run(
            &agent("frobnicator/x"),
            root,
            "sys".into(),
            "hi".into(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, RunError::UnknownProvider(p) if p == "frobnicator"),
            "{error:?}"
        );
    }

    // Without the `fake-model` feature, the shipped binary has no fake
    // provider: `fake/scripted` is just an unknown provider. The feature
    // build carries the provider, and the governed missouri suites drive
    // it end to end.
    #[cfg(not(feature = "fake-model"))]
    #[tokio::test]
    async fn the_fake_provider_is_absent_without_its_feature() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path()).unwrap();
        let error = run(
            &agent("fake/scripted"),
            root,
            "sys".into(),
            "hi".into(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, RunError::UnknownProvider(p) if p == "fake"),
            "{error:?}"
        );
    }
}
