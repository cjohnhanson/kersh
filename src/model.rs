//! Running an agent through rig.
//!
//! The three read tools become rig tools over the run's confined root.
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
use crate::tools::{DEFAULT_MAX_BYTES, Root, ToolError};

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
    // past the token limit. The agent's read tools use the confined root,
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

/// Add the read tools, the preamble, the turn cap, and the gaff hook to
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
        .tool(ReadFile(Arc::clone(root)))
        .tool(Grep(Arc::clone(root)))
        .tool(ListFiles(Arc::clone(root)))
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

// The tools. Each wraps the confined root and reports its error as text
// the model reads.

struct ReadFile(Arc<Root>);
struct Grep(Arc<Root>);
struct ListFiles(Arc<Root>);

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "all_files")]
    glob: String,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default = "all_files")]
    glob: String,
}

fn all_files() -> String {
    "**/*".to_owned()
}

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Args = ReadArgs;
    type Output = String;
    type Error = ToolError;

    fn description(&self) -> String {
        "Read a text file under the working root, by relative path.".to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.read_file(&args.path, DEFAULT_MAX_BYTES)
    }
}

impl Tool for Grep {
    const NAME: &'static str = "grep";
    type Args = GrepArgs;
    type Output = String;
    type Error = ToolError;

    fn description(&self) -> String {
        "Search files under the root for a regular expression. Returns path:line:text.".to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "glob": { "type": "string", "description": "A path glob, default **/*" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.0.grep(&args.pattern, &args.glob)?.join("\n"))
    }
}

impl Tool for ListFiles {
    const NAME: &'static str = "list";
    type Args = ListArgs;
    type Output = String;
    type Error = ToolError;

    fn description(&self) -> String {
        "List the files under the root that match a path glob.".to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "glob": { "type": "string", "description": "A path glob, default **/*" }
            }
        })
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok(self.0.list(&args.glob)?.join("\n"))
    }
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
