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
    #[error("the run failed: {0}")]
    Prompt(String),
}

/// Run `agent` with `system` as its preamble and `first_turn` as its
/// prompt, its tools confined to `root`.
pub async fn run(
    agent: &Agent,
    root: Root,
    system: String,
    first_turn: String,
) -> Result<String, RunError> {
    let root = Arc::new(root);
    match agent.provider() {
        "claude-code" => {
            let client = rig_claude_code::ClaudeCodeClient::from_env().map_err(|error| {
                RunError::Provider {
                    provider: "claude-code".to_owned(),
                    message: error.to_string(),
                }
            })?;
            let built = configure(client.agent(agent.model_id()), &system, &root, agent.meta.max_turns);
            drive(built, first_turn).await
        }
        "anthropic" => {
            let client =
                rig::providers::anthropic::Client::from_env().map_err(|error| RunError::Provider {
                    provider: "anthropic".to_owned(),
                    message: error.to_string(),
                })?;
            let built =
                configure(client.agent(agent.model_id()), &system, &root, agent.meta.max_turns);
            drive(built, first_turn).await
        }
        other => Err(RunError::UnknownProvider(other.to_owned())),
    }
}

/// Add the read tools, the preamble, and the turn cap to any provider's
/// agent builder, then build it.
fn configure<M: CompletionModel + 'static>(
    builder: AgentBuilder<M>,
    system: &str,
    root: &Arc<Root>,
    max_turns: usize,
) -> rig::agent::Agent<M> {
    builder
        .preamble(system)
        .tool(ReadFile(Arc::clone(root)))
        .tool(Grep(Arc::clone(root)))
        .tool(ListFiles(Arc::clone(root)))
        .default_max_turns(max_turns)
        .build()
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

    async fn call(&self, _: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
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

    async fn call(&self, _: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
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

    async fn call(&self, _: &mut ToolContext, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(self.0.list(&args.glob)?.join("\n"))
    }
}
