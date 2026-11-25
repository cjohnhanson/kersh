//! `kersh serve`: the MCP server, on stdio only.
//!
//! Each tool is one command an agent can use without a terminal, and it
//! calls the [`Client`] method that command calls. Its text is what the
//! command prints with `--json`: one JSON object per line. `attach` is
//! not a tool, because it needs a terminal.
//!
//! The server listens on nothing: the MCP client that started it holds
//! its stdin and stdout.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError,
    Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, Tool, ToolsCapability,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::session::{Harness, StartRequest, State};

/// The served client.
#[derive(Clone)]
pub struct KershServer {
    client: Arc<Client>,
}

/// What a tool call returns: its text, and whether it failed.
type Called = Result<String, String>;

/// One JSON object per line, as `--json` prints a list.
fn lines<T: serde::Serialize>(items: &[T]) -> Called {
    items
        .iter()
        .map(|item| serde_json::to_string(item).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map(|lines| lines.join("\n"))
}

fn line<T: serde::Serialize>(item: &T) -> Called {
    serde_json::to_string(item).map_err(|error| error.to_string())
}

fn args<T: DeserializeOwned>(arguments: Map<String, Value>) -> Result<T, String> {
    serde_json::from_value(Value::Object(arguments))
        .map_err(|error| format!("bad arguments: {error}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PsArgs {
    #[serde(default)]
    ids: Vec<String>,
    state: Option<State>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeArgs {
    id: String,
    prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsArgs {
    id: String,
    #[serde(default)]
    since: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupArgs {
    harness: Harness,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

/// An object schema from `(name, schema, required)` triples.
fn object(fields: &[(&str, Value, bool)]) -> Arc<Map<String, Value>> {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, schema, is_required) in fields {
        properties.insert((*name).to_owned(), schema.clone());
        if *is_required {
            required.push(json!(name));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    Arc::new(schema)
}

fn text(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn id() -> (&'static str, Value, bool) {
    ("id", text("The session id."), true)
}

fn harness_names() -> Value {
    json!({"type": "string", "enum": ["rig", "claude-code", "codex"]})
}

fn tool(name: &'static str, description: &'static str, schema: Arc<Map<String, Value>>) -> Tool {
    Tool::new(name, description, schema)
}

/// Every tool, in the order the commands are listed.
#[allow(clippy::too_many_lines)] // One entry per tool.
fn tools() -> Vec<Tool> {
    vec![
        tool(
            "kersh_ps",
            "List sessions from their records, as `kersh ps --json`: one record per line. \
             Needs no host.",
            object(&[
                (
                    "ids",
                    json!({"type": "array", "items": {"type": "string"}, "description": "Only these sessions."}),
                    false,
                ),
                (
                    "state",
                    json!({"type": "string", "enum": ["running", "held", "ended", "lost"]}),
                    false,
                ),
            ]),
        ),
        tool(
            "kersh_start",
            "Start a session, as `kersh start --json`, and return its record. Name an \
             agent, or give `harness` or `runtime`. A headless start needs `prompt`. \
             Without `cwd`, the session runs in this server's directory.",
            object(&[
                ("agent", text("An agent file name."), false),
                ("runtime", text("A runtime from runtimes.yml."), false),
                ("harness", harness_names(), false),
                (
                    "mode",
                    json!({"type": "string", "enum": ["headless", "interactive"]}),
                    false,
                ),
                ("model", text("The harness's model."), false),
                ("skill", text("Replace the agent's almanac skill."), false),
                ("profile", text("The gaff profile."), false),
                (
                    "instructions_file",
                    text("A file whose text is added to the agent's instructions."),
                    false,
                ),
                (
                    "cwd",
                    text("The working directory, an absolute path."),
                    false,
                ),
                (
                    "tempdir",
                    json!({"type": "boolean", "description": "Give the session its own temp dir."}),
                    false,
                ),
                (
                    "labels",
                    json!({"type": "object", "additionalProperties": {"type": "string"},
                           "description": "Labels kersh stores and echoes, never reads."}),
                    false,
                ),
                (
                    "group",
                    text("The session's group, for max_running."),
                    false,
                ),
                (
                    "max_running",
                    json!({"type": "integer", "minimum": 0,
                           "description": "Refuse the start when the group has this many running."}),
                    false,
                ),
                ("name", text("A name for the session."), false),
                ("prompt", text("The first prompt."), false),
                (
                    "resume_conversation",
                    text("Resume this harness conversation instead of starting a new one."),
                    false,
                ),
                (
                    "harness_flags",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Flags for the harness, checked against its allow-list."}),
                    false,
                ),
            ]),
        ),
        tool(
            "kersh_stop",
            "End a session and close its pane, as `kersh stop --json`.",
            object(&[id()]),
        ),
        tool(
            "kersh_resume",
            "Continue a held or ended session's conversation in a new pane, as `kersh \
             resume --json`. A headless session needs `prompt`.",
            object(&[id(), ("prompt", text("The prompt."), false)]),
        ),
        tool(
            "kersh_restart",
            "Restart an interactive session on its conversation, as `kersh restart --json`.",
            object(&[id()]),
        ),
        tool(
            "kersh_send",
            "Type text into a running interactive session, then Enter, as `kersh send`.",
            object(&[id(), ("text", text("The text to type."), true)]),
        ),
        tool(
            "kersh_events",
            "A headless session's events, as `kersh events`: one event per line.",
            object(&[
                id(),
                (
                    "since",
                    json!({"type": "integer", "minimum": 0,
                           "description": "Only events after this sequence number."}),
                    false,
                ),
            ]),
        ),
        tool(
            "kersh_screen",
            "A session's screen, as `kersh screen --json`.",
            object(&[id()]),
        ),
        tool(
            "kersh_agent_list",
            "Every agent file and its description, as `kersh list --json`.",
            object(&[]),
        ),
        tool(
            "kersh_agent_show",
            "One agent file's settings and system prompt, as `kersh show --json`.",
            object(&[("name", text("The agent."), true)]),
        ),
        tool(
            "kersh_runtime_list",
            "Every runtime, as `kersh runtime list --json`.",
            object(&[]),
        ),
        tool(
            "kersh_runtime_show",
            "One runtime, as `kersh runtime show --json`.",
            object(&[("name", text("The runtime."), true)]),
        ),
        tool(
            "kersh_harness_list",
            "The build each harness would run, as `kersh harness list --json`.",
            object(&[]),
        ),
        tool(
            "kersh_setup",
            "Run gaff's registration for a harness and check each piece of its wiring, as \
             `kersh setup --json`. Reports an error when a piece is missing.",
            object(&[(
                "harness",
                json!({"type": "string", "enum": ["claude-code", "codex"]}),
                true,
            )]),
        ),
    ]
}

impl KershServer {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// Run one tool. It blocks, so the caller runs it off the async
    /// workers.
    fn call(&self, name: &str, arguments: Map<String, Value>) -> Called {
        let client = &self.client;
        let fail = |error: crate::session::SessionError| error.to_string();
        match name {
            "kersh_ps" => {
                let PsArgs { ids, state } = args(arguments)?;
                lines(&client.ps(&ids, state).map_err(fail)?)
            }
            "kersh_start" => {
                let request: StartRequest = args(arguments)?;
                line(&client.start(&request).map_err(fail)?)
            }
            "kersh_stop" => {
                let IdArgs { id } = args(arguments)?;
                line(&client.stop(&id).map_err(fail)?)
            }
            "kersh_resume" => {
                let ResumeArgs { id, prompt } = args(arguments)?;
                line(&client.resume(&id, prompt.as_deref()).map_err(fail)?)
            }
            "kersh_restart" => {
                let IdArgs { id } = args(arguments)?;
                line(&client.restart(&id).map_err(fail)?)
            }
            "kersh_send" => {
                let SendArgs { id, text } = args(arguments)?;
                client
                    .send(&id, text.trim_end_matches('\n'))
                    .map_err(fail)?;
                Ok(String::new())
            }
            "kersh_events" => {
                let EventsArgs { id, since } = args(arguments)?;
                lines(&client.events(&id, since).map_err(fail)?)
            }
            "kersh_screen" => {
                let IdArgs { id } = args(arguments)?;
                line(&client.screen(&id).map_err(fail)?)
            }
            "kersh_agent_list" => {
                let NoArgs {} = args(arguments)?;
                lines(&client.list())
            }
            "kersh_agent_show" => {
                let NameArgs { name } = args(arguments)?;
                line(&client.show(&name).map_err(|error| error.to_string())?)
            }
            "kersh_runtime_list" => {
                let NoArgs {} = args(arguments)?;
                lines(&client.runtime_list().map_err(fail)?)
            }
            "kersh_runtime_show" => {
                let NameArgs { name } = args(arguments)?;
                line(&client.runtime_show(&name).map_err(fail)?)
            }
            "kersh_harness_list" => {
                let NoArgs {} = args(arguments)?;
                lines(&client.harness_list().map_err(fail)?)
            }
            "kersh_setup" => {
                let SetupArgs { harness } = args(arguments)?;
                let report = client.setup(harness).map_err(fail)?;
                let text = lines(&report.items)?;
                if report.ok() {
                    Ok(text)
                } else {
                    Err(format!(
                        "{text}\n{harness} is not fully wired; see the missing pieces"
                    ))
                }
            }
            other => Err(format!("no tool named `{other}`")),
        }
    }
}

impl ServerHandler for KershServer {
    fn get_info(&self) -> InitializeResult {
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability::default());
        InitializeResult::new(capabilities)
            .with_protocol_version(ProtocolVersion::default())
            .with_server_info({
                let mut info = Implementation::from_build_env();
                "kersh".clone_into(&mut info.name);
                env!("CARGO_PKG_VERSION").clone_into(&mut info.version);
                info
            })
            .with_instructions(
                "kersh runs agents in terminal sessions. Each tool is a kersh command and \
                 returns what the command prints with --json, one JSON object per line. \
                 An agent inside a session finds its own id in $KERSH_SESSION and ends the \
                 session with kersh_stop when its task is done.",
            )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tools())))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let name = request.name.to_string();
        let this = self.clone();
        // The calls read files and wait on the host, so they run off the
        // async workers.
        let called = tokio::task::spawn_blocking(move || this.call(&name, arguments))
            .await
            .map_err(|error| {
                McpError::internal_error(format!("the call did not finish: {error}"), None)
            })?;
        Ok(match called {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        }
        .into())
    }
}

/// Serve MCP on stdin and stdout until the client hangs up.
pub fn run() -> Result<(), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let server = KershServer::new(Client::discover(&cwd));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        use rmcp::ServiceExt as _;
        let service = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|error| error.to_string())?;
        service
            .waiting()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn attach_is_not_a_tool_and_every_schema_is_an_object() {
        let tools = tools();
        assert!(tools.iter().all(|tool| tool.name != "kersh_attach"));
        for tool in &tools {
            assert_eq!(tool.input_schema.get("type"), Some(&json!("object")));
        }
    }

    #[test]
    fn start_arguments_are_a_start_request() {
        let request: StartRequest = args(
            json!({"harness": "codex", "labels": {"card": "acme-3"}})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(request.harness, Some(Harness::Codex));
        assert_eq!(
            request.labels.get("card").map(String::as_str),
            Some("acme-3")
        );
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        let server = KershServer::new(Client::from_root(PathBuf::from("/nonexistent")));
        let error = server
            .call(
                "kersh_stop",
                json!({"id": "a", "force": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap_err();
        assert!(error.contains("force"), "{error}");
    }
}
