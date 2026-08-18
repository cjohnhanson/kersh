//! A scripted fake model, for hermetic end-to-end tests only.
//!
//! This module exists behind the `fake-model` feature, off by default, so
//! the shipped binary carries no fake provider. A test build turns it on,
//! and `model: fake/scripted` selects it. The script is a JSON array in
//! `KERSH_FAKE_SCRIPT`, one entry per model turn. It lets a missouri test
//! drive a whole governed run with no network: the fake model emits a
//! scripted tool call, the real gaff hook runs, and the run ends on a
//! scripted text turn.
//!
//! The fake reads no real API and holds no state between calls. It picks
//! its turn by counting the assistant messages already in the history, so
//! turn 0 emits the first script entry, and each answered tool call
//! advances the index by one.

use rig::completion::message::{AssistantContent, Message, ToolResultContent, UserContent};
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
};
use rig::one_or_many::OneOrMany;
use rig::streaming::StreamingCompletionResponse;
use serde::{Deserialize, Serialize};

/// One scripted model turn. Tagged by `kind`, so a fixture reads
/// naturally: `{"kind":"tool","name":"read_file","args":{"path":"x"}}`,
/// `{"kind":"text","text":"done"}`, or `{"kind":"echo_prompt"}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Turn {
    /// Call a tool with the given name and arguments.
    Tool {
        name: String,
        args: serde_json::Value,
    },
    /// Emit final text and end the run.
    Text { text: String },
    /// Emit the last user turn's text back as the answer, so a test can
    /// prove the injected session-start context reached the model.
    EchoPrompt,
    /// Emit the most recent tool result back as the answer, so a test can
    /// prove what a tool returned: the file content when a guard allowed
    /// the read, or the refusal reason when a guard skipped it.
    EchoToolResult,
}

/// A model that plays a fixed script. Cloneable, as the trait requires;
/// the script is shared by clone of the vector.
#[derive(Debug, Clone)]
pub struct FakeModel {
    script: Vec<Turn>,
}

impl FakeModel {
    /// Build a fake model from the JSON script in `KERSH_FAKE_SCRIPT`.
    ///
    /// # Errors
    /// Returns the reason when the variable is unset or is not a JSON array
    /// of turns.
    pub fn from_env() -> Result<Self, String> {
        let raw = std::env::var("KERSH_FAKE_SCRIPT")
            .map_err(|_| "the fake model needs a script in KERSH_FAKE_SCRIPT".to_owned())?;
        Self::from_json(&raw)
    }

    /// Build a fake model from a JSON script.
    ///
    /// # Errors
    /// Returns the reason when the text is not a JSON array of turns.
    fn from_json(raw: &str) -> Result<Self, String> {
        let script: Vec<Turn> = serde_json::from_str(raw)
            .map_err(|error| format!("KERSH_FAKE_SCRIPT is not a valid script: {error}"))?;
        Ok(Self { script })
    }

    /// The turn to play, chosen by how many assistant turns already ran.
    fn turn_for(&self, history: &OneOrMany<Message>) -> AssistantContent {
        let index = history
            .iter()
            .filter(|message| matches!(message, Message::Assistant { .. }))
            .count();
        match self.script.get(index) {
            Some(Turn::Tool { name, args }) => {
                AssistantContent::tool_call(format!("fake-{index}"), name, args.clone())
            }
            Some(Turn::Text { text }) => AssistantContent::text(text.clone()),
            Some(Turn::EchoPrompt) => AssistantContent::text(last_user_text(history)),
            Some(Turn::EchoToolResult) => AssistantContent::text(last_tool_result_text(history)),
            // A run past the script ends, rather than looping forever.
            None => AssistantContent::text("fake: the script is spent".to_owned()),
        }
    }
}

/// The text of the last user message, joined, or empty. A test scripts
/// `echo_prompt` to read back the first turn, which carries the injected
/// context.
fn last_user_text(history: &OneOrMany<Message>) -> String {
    let mut found = String::new();
    for message in history.iter() {
        if let Message::User { content } = message {
            let mut parts = Vec::new();
            for item in content.iter() {
                if let UserContent::Text(text) = item {
                    parts.push(text.text.clone());
                }
            }
            if !parts.is_empty() {
                found = parts.join("\n");
            }
        }
    }
    found
}

/// The text of the most recent tool result in the history, joined, or
/// empty. rig records a skipped call's reason as its result too, so this
/// reads back the file content on an allow and the refusal on a skip.
fn last_tool_result_text(history: &OneOrMany<Message>) -> String {
    let mut found = String::new();
    for message in history.iter() {
        if let Message::User { content } = message {
            for item in content.iter() {
                if let UserContent::ToolResult(result) = item {
                    let mut parts = Vec::new();
                    for piece in result.content.iter() {
                        if let ToolResultContent::Text(text) = piece {
                            parts.push(text.text.clone());
                        }
                    }
                    if !parts.is_empty() {
                        found = parts.join("\n");
                    }
                }
            }
        }
    }
    found
}

/// The fake's raw response. It carries nothing; the choice holds the turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeRaw;

impl rig::completion::GetTokenUsage for FakeRaw {
    fn token_usage(&self) -> Usage {
        Usage::new()
    }
}

impl CompletionModel for FakeModel {
    type Response = FakeRaw;
    type StreamingResponse = FakeRaw;
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self { script: Vec::new() }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        Ok(CompletionResponse {
            choice: OneOrMany::one(self.turn_for(&request.chat_history)),
            usage: Usage::new(),
            raw_response: FakeRaw,
            message_id: None,
        })
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        // kersh drives the agent with `prompt`, which uses `completion`.
        // The streaming path is never taken, so it is a plain refusal.
        Err(CompletionError::ProviderError(
            "the fake model does not stream".to_owned(),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A history with `assistant_turns` assistant messages after the first
    /// user prompt. The fake counts these to pick its turn.
    fn history(assistant_turns: usize) -> OneOrMany<Message> {
        let mut messages = vec![Message::User {
            content: OneOrMany::one(UserContent::Text("go".to_owned().into())),
        }];
        for _ in 0..assistant_turns {
            messages.push(Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("ok")),
            });
        }
        OneOrMany::many(messages).unwrap()
    }

    fn model(script_json: &str) -> FakeModel {
        FakeModel::from_json(script_json).unwrap()
    }

    #[test]
    fn the_turn_index_follows_the_assistant_count() {
        let fake = model(
            r#"[{"kind":"tool","name":"read_file","args":{"path":"x"}},{"kind":"text","text":"done"}]"#,
        );
        // Turn 0: no assistant messages yet, so the first entry, a tool.
        assert!(matches!(
            fake.turn_for(&history(0)),
            AssistantContent::ToolCall(_)
        ));
        // Turn 1: one assistant message answered, so the second entry, text.
        assert!(matches!(
            fake.turn_for(&history(1)),
            AssistantContent::Text(_)
        ));
    }

    #[test]
    fn a_run_past_the_script_ends_with_text() {
        let fake = model(r#"[{"kind":"text","text":"only"}]"#);
        // A history longer than the script gets a terminal text turn, so a
        // run never loops forever.
        assert!(matches!(
            fake.turn_for(&history(5)),
            AssistantContent::Text(_)
        ));
    }

    #[test]
    fn a_malformed_script_is_a_named_error() {
        let error = FakeModel::from_json("not json").unwrap_err();
        assert!(error.contains("not a valid script"), "{error}");
    }
}
