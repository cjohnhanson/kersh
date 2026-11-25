//! `kersh run --output jsonl`: one JSON line on stdout per turn, tool
//! call, tool result, and assistant text, then the run's result with the
//! final text as the last line.
//!
//! The lines are [`RigLine`]s, so the wrapper of a rig session parses them
//! into the same events as a headless Claude Code or Codex run. Without
//! the flag no line is written and stdout stays the final text only.

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use rig::agent::hook::{
    AgentHook, CompletionCall, CompletionCallAction, HookContext, ModelTurnAction,
    ModelTurnFinished, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig::completion::message::AssistantContent;

use crate::events::{EventKind, RigLine};

/// Where the lines go: stdout, or a buffer in tests.
#[derive(Clone)]
pub struct Stream {
    out: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stream")
    }
}

impl Stream {
    /// Lines to standard output.
    #[must_use]
    pub fn stdout() -> Self {
        Self::to(Box::new(std::io::stdout()))
    }

    /// Lines to `out`.
    #[must_use]
    pub fn to(out: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            out: Arc::new(Mutex::new(out)),
        }
    }

    /// Write one line and flush it, so a reader sees each event as it
    /// happens. A closed stdout loses the line, not the run.
    pub fn emit(&self, line: &RigLine) {
        let Ok(mut text) = serde_json::to_vec(line) else {
            return;
        };
        text.push(b'\n');
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = out.write_all(&text);
        let _ = out.flush();
    }

    /// The first line: which agent runs on which model.
    pub fn init(&self, agent: &str, model: &str) {
        self.emit(&RigLine {
            agent: Some(agent.to_owned()),
            model: Some(model.to_owned()),
            ..RigLine::new(EventKind::Init)
        });
    }

    /// The last line: the final text, or the error that ended the run.
    pub fn result(&self, outcome: Result<&str, &str>) {
        let (status, is_error, text) = match outcome {
            Ok(text) => ("success", false, text),
            Err(error) => ("error", true, error),
        };
        self.emit(&RigLine {
            status: Some(status.to_owned()),
            is_error: Some(is_error),
            text: Some(text.to_owned()),
            ..RigLine::new(EventKind::Result)
        });
    }
}

/// The rig hook that writes the lines. It only observes: every action it
/// returns lets the run go on, so it is registered before the gaff hook
/// and sees each call gaff then refuses.
pub struct Observer {
    stream: Stream,
}

impl Observer {
    #[must_use]
    pub const fn new(stream: Stream) -> Self {
        Self { stream }
    }
}

/// A tool result's status, as the line names it.
fn result_status(result: &rig::tool::ToolResult) -> &'static str {
    if result.is_success() {
        "success"
    } else if result.is_skipped() || result.is_refused() {
        "refused"
    } else {
        "error"
    }
}

// The trait declares these methods async; the observer writes a line and
// answers at once, so no body awaits anything.
#[allow(clippy::unused_async_trait_impl)]
impl AgentHook for Observer {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        self.stream.emit(&RigLine {
            turn: Some(event.turn),
            ..RigLine::new(EventKind::Turn)
        });
        CompletionCallAction::Continue
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let text: Vec<&str> = event
            .content
            .iter()
            .filter_map(|item| match item {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .filter(|text| !text.trim().is_empty())
            .collect();
        if !text.is_empty() {
            self.stream.emit(&RigLine {
                text: Some(text.join("\n")),
                ..RigLine::new(EventKind::Text)
            });
        }
        ModelTurnAction::Continue
    }

    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        self.stream.emit(&RigLine {
            tool: Some(event.tool_name.to_owned()),
            input: Some(crate::events::tool_input_text(event.args)),
            ..RigLine::new(EventKind::Tool)
        });
        ToolCallAction::Run
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        let status = result_status(event.raw_result);
        self.stream.emit(&RigLine {
            tool: Some(event.tool_name.to_owned()),
            status: Some(status.to_owned()),
            is_error: Some(status != "success"),
            ..RigLine::new(EventKind::ToolResult)
        });
        ToolResultAction::Keep
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_line_is_one_json_object_the_events_parser_reads() {
        let buffer = Buffer::default();
        let stream = Stream::to(Box::new(buffer.clone()));
        stream.init("fixer", "fake/scripted");
        stream.result(Err("the run failed: boom"));
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let init = crate::events::parse_stdout(crate::session::Harness::Rig, lines[0]);
        assert_eq!(init.kind, EventKind::Init);
        assert_eq!(init.summary, "init: fake/scripted");
        let result = crate::events::parse_stdout(crate::session::Harness::Rig, lines[1]);
        assert_eq!(result.kind, EventKind::Result);
        assert_eq!(result.status.as_deref(), Some("error"));
        assert_eq!(result.is_error, Some(true));
        assert!(result.summary.contains("boom"), "{}", result.summary);
    }
}
