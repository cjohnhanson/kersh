//! A headless session's progress: `sessions/<id>/events.jsonl`.
//!
//! The wrapper is the file's only writer and holds an exclusive `flock` on
//! it for its whole life. Each line the harness prints becomes one event,
//! numbered from 1 and appended with its newline in one write. A wrapper
//! that starts on an existing file first drops a trailing line with no
//! newline (a write a crash cut off), then continues from the last number.
//! Readers skip a trailing line with no newline, so they never parse half
//! an event.

use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::Harness;

/// What an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The harness started its session.
    Init,
    /// A turn began.
    Turn,
    /// A tool call.
    Tool,
    /// A tool's result.
    ToolResult,
    /// Assistant text.
    Text,
    /// The run's result, with its status and, where the harness reports
    /// one, its cost.
    Result,
    /// An error item from the harness.
    Error,
    /// A line on the harness's standard error.
    Stderr,
    /// Anything else, kept with its raw line.
    Other,
}

/// One event, as `kersh events` prints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The sequence number, from 1, unique within the session.
    pub seq: u64,
    /// When the wrapper read the line, RFC 3339 in UTC.
    pub ts: String,
    pub kind: EventKind,
    /// One line for a person: `tool Bash: cargo test`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The tool's input, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A result's status: `success`, `completed`, an error subtype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// A conversation id the line carries (Codex's `thread.started`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// The harness's line: its JSON, or the text of a line that is not
    /// JSON.
    pub raw: Value,
}

/// The longest tool input an event keeps, in characters.
const INPUT_CHARS: usize = 200;

/// Shorten `text` to `max` characters, marking the cut.
fn shorten(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max).collect();
    format!("{cut}…")
}

/// The parsed parts of one line, before it is numbered.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub kind: EventKind,
    pub summary: String,
    pub tool: Option<String>,
    pub input: Option<String>,
    pub status: Option<String>,
    pub is_error: Option<bool>,
    pub cost_usd: Option<f64>,
    pub conversation: Option<String>,
    pub raw: Value,
}

impl Parsed {
    fn new(kind: EventKind, summary: impl Into<String>, raw: Value) -> Self {
        Self {
            kind,
            summary: summary.into(),
            tool: None,
            input: None,
            status: None,
            is_error: None,
            cost_usd: None,
            conversation: None,
            raw,
        }
    }

    /// Number the parsed line.
    #[must_use]
    pub fn into_event(self, seq: u64, ts: String) -> Event {
        Event {
            seq,
            ts,
            kind: self.kind,
            summary: self.summary,
            tool: self.tool,
            input: self.input,
            status: self.status,
            is_error: self.is_error,
            cost_usd: self.cost_usd,
            conversation: self.conversation,
            raw: self.raw,
        }
    }
}

/// A line the harness wrote to standard error.
#[must_use]
pub fn parse_stderr(line: &str) -> Parsed {
    Parsed::new(
        EventKind::Stderr,
        format!("stderr: {}", shorten(line, INPUT_CHARS)),
        Value::String(line.to_owned()),
    )
}

/// A line the harness wrote to standard output.
#[must_use]
pub fn parse_stdout(harness: Harness, line: &str) -> Parsed {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Parsed::new(
            EventKind::Other,
            shorten(line, INPUT_CHARS),
            Value::String(line.to_owned()),
        );
    };
    match harness {
        Harness::ClaudeCode => parse_claude(value),
        Harness::Codex => parse_codex(value),
        Harness::Rig => parse_other(value),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn parse_other(value: Value) -> Parsed {
    let kind = str_field(&value, "type").unwrap_or("event").to_owned();
    Parsed::new(EventKind::Other, kind, value)
}

/// A tool input in one line: a shell command as written, anything else as
/// compact JSON.
fn tool_input(input: &Value) -> String {
    let text = input
        .get("command")
        .and_then(Value::as_str)
        .map_or_else(|| input.to_string(), str::to_owned);
    shorten(&text, INPUT_CHARS)
}

/// Claude Code's `stream-json`. Lines are found by `type` and `subtype`,
/// never by position: hook events come before `system/init`.
fn parse_claude(value: Value) -> Parsed {
    let kind = str_field(&value, "type").unwrap_or_default().to_owned();
    let subtype = str_field(&value, "subtype").unwrap_or_default().to_owned();
    match (kind.as_str(), subtype.as_str()) {
        ("system", "init") => {
            let model = str_field(&value, "model").unwrap_or("unknown model");
            let summary = format!("init: {model}");
            let mut parsed = Parsed::new(EventKind::Init, summary, value);
            parsed.conversation = str_field(&parsed.raw, "session_id").map(str::to_owned);
            parsed
        }
        ("system", sub) => {
            let hook = str_field(&value, "hook_event").unwrap_or_default();
            let summary = if hook.is_empty() {
                format!("system {sub}")
            } else {
                format!("system {sub} {hook}")
            };
            Parsed::new(EventKind::Other, summary, value)
        }
        ("assistant", _) => parse_claude_assistant(value),
        ("user", _) => parse_claude_user(value),
        ("result", sub) => {
            let status = sub.to_owned();
            let is_error = value.get("is_error").and_then(Value::as_bool);
            let cost = value.get("total_cost_usd").and_then(Value::as_f64);
            let summary = match cost {
                Some(cost) => format!("result: {status}, ${cost:.2}"),
                None => format!("result: {status}"),
            };
            let mut parsed = Parsed::new(EventKind::Result, summary, value);
            parsed.status = Some(status);
            parsed.is_error = is_error;
            parsed.cost_usd = cost;
            parsed
        }
        (other, _) => Parsed::new(EventKind::Other, other.to_owned(), value),
    }
}

fn content_blocks(value: &Value) -> Vec<Value> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn parse_claude_assistant(value: Value) -> Parsed {
    let blocks = content_blocks(&value);
    let tools: Vec<&Value> = blocks
        .iter()
        .filter(|block| str_field(block, "type") == Some("tool_use"))
        .collect();
    if let Some(first) = tools.first() {
        let name = str_field(first, "name").unwrap_or("tool").to_owned();
        let input = first.get("input").map(tool_input).unwrap_or_default();
        let mut summary = format!("tool {name}: {input}");
        if tools.len() > 1 {
            use std::fmt::Write as _;
            let _ = write!(summary, " (+{} more)", tools.len() - 1);
        }
        let mut parsed = Parsed::new(EventKind::Tool, summary, value);
        parsed.tool = Some(name);
        parsed.input = Some(input);
        return parsed;
    }
    let text: Vec<&str> = blocks
        .iter()
        .filter(|block| str_field(block, "type") == Some("text"))
        .filter_map(|block| str_field(block, "text"))
        .collect();
    if text.is_empty() {
        return Parsed::new(EventKind::Other, "assistant", value);
    }
    let summary = shorten(&text.join(" "), INPUT_CHARS);
    Parsed::new(EventKind::Text, summary, value)
}

fn parse_claude_user(value: Value) -> Parsed {
    let blocks = content_blocks(&value);
    let Some(result) = blocks
        .iter()
        .find(|block| str_field(block, "type") == Some("tool_result"))
    else {
        return Parsed::new(EventKind::Other, "user", value);
    };
    let is_error = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = if is_error {
        "tool result: error"
    } else {
        "tool result: ok"
    };
    let mut parsed = Parsed::new(EventKind::ToolResult, summary, value);
    parsed.is_error = Some(is_error);
    parsed
}

/// Codex's `exec --json` lines.
fn parse_codex(value: Value) -> Parsed {
    let kind = str_field(&value, "type").unwrap_or_default().to_owned();
    match kind.as_str() {
        "thread.started" => {
            let thread = str_field(&value, "thread_id").map(str::to_owned);
            let summary = format!("thread {}", thread.as_deref().unwrap_or("?"));
            let mut parsed = Parsed::new(EventKind::Init, summary, value);
            parsed.conversation = thread;
            parsed
        }
        "turn.started" => Parsed::new(EventKind::Turn, "turn started", value),
        "turn.completed" => {
            let mut parsed = Parsed::new(EventKind::Result, "result: completed", value);
            parsed.status = Some("completed".to_owned());
            parsed.is_error = Some(false);
            parsed
        }
        "turn.failed" => {
            let mut parsed = Parsed::new(EventKind::Result, "result: failed", value);
            parsed.status = Some("failed".to_owned());
            parsed.is_error = Some(true);
            parsed
        }
        "item.started" | "item.completed" => parse_codex_item(&kind, value),
        other => Parsed::new(EventKind::Other, other.to_owned(), value),
    }
}

fn parse_codex_item(kind: &str, value: Value) -> Parsed {
    let item = value.get("item").cloned().unwrap_or(Value::Null);
    let item_type = str_field(&item, "type").unwrap_or_default();
    let completed = kind == "item.completed";
    match item_type {
        "agent_message" if completed => {
            let text = str_field(&item, "text").unwrap_or_default();
            Parsed::new(EventKind::Text, shorten(text, INPUT_CHARS), value)
        }
        "command_execution" => {
            let command = shorten(str_field(&item, "command").unwrap_or_default(), INPUT_CHARS);
            if completed {
                let code = item.get("exit_code").and_then(Value::as_i64);
                let summary = match code {
                    Some(code) => format!("tool result: exit {code}"),
                    None => "tool result".to_owned(),
                };
                let mut parsed = Parsed::new(EventKind::ToolResult, summary, value);
                parsed.tool = Some("shell".to_owned());
                parsed.is_error = code.map(|code| code != 0);
                parsed
            } else {
                let mut parsed =
                    Parsed::new(EventKind::Tool, format!("tool shell: {command}"), value);
                parsed.tool = Some("shell".to_owned());
                parsed.input = Some(command);
                parsed
            }
        }
        "error" => {
            let message = str_field(&item, "message").unwrap_or_default();
            let mut parsed = Parsed::new(
                EventKind::Error,
                format!("error: {}", shorten(message, INPUT_CHARS)),
                value,
            );
            parsed.is_error = Some(true);
            parsed
        }
        other => Parsed::new(EventKind::Other, format!("{kind} {other}"), value),
    }
}

/// The writer side of an events file, held by one wrapper.
#[derive(Debug)]
pub struct Writer {
    file: std::fs::File,
    next: u64,
}

impl Writer {
    /// Open `path` for appending, take its exclusive lock, drop a trailing
    /// partial line, and continue numbering after the last event.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                std::io::Error::other(format!(
                    "{} is held by another writer: {error}",
                    path.display()
                ))
            },
        )?;
        let mut text = Vec::new();
        file.rewind()?;
        file.read_to_end(&mut text)?;
        let complete = text
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1);
        if complete < text.len() {
            file.set_len(u64::try_from(complete).unwrap_or(0))?;
        }
        let last = text
            .get(..complete)
            .unwrap_or_default()
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter_map(|value| value.get("seq").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        Ok(Self {
            file,
            next: last + 1,
        })
    }

    /// The number the next event gets.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next
    }

    /// Number `parsed`, append it with its newline in one write, and
    /// return the event.
    pub fn append(&mut self, parsed: Parsed) -> std::io::Result<Event> {
        let event = parsed.into_event(self.next, crate::util::now_rfc3339());
        let mut line = serde_json::to_vec(&event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.next += 1;
        Ok(event)
    }
}

/// Read the events after `since` (exclusive), skipping a trailing line
/// with no newline. A missing file has no events.
pub fn read(path: &Path, since: u64) -> std::io::Result<Vec<Event>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        if let Ok(event) = serde_json::from_slice::<Event>(&line)
            && event.seq > since
        {
            events.push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Open a writer on a file the test's own writer just released. A
    /// process another test forks at that moment holds a copy of the old
    /// descriptor, and so the lock, until its exec closes it.
    fn reopen(path: &Path) -> Writer {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match Writer::open(path) {
                Ok(writer) => return writer,
                Err(error) if std::time::Instant::now() > deadline => panic!("{error}"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn events_are_numbered_from_one_and_read_after_since() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut writer = Writer::open(&path).unwrap();
        for line in ["a", "b", "c"] {
            writer.append(parse_stderr(line)).unwrap();
        }
        let all = read(&path, 0).unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
        let later = read(&path, 2).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].raw, Value::String("c".to_owned()));
    }

    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let _first = Writer::open(&path).unwrap();
        let error = Writer::open(&path).unwrap_err();
        assert!(error.to_string().contains("another writer"), "{error}");
    }

    #[test]
    fn a_trailing_partial_line_is_skipped_by_readers_and_dropped_by_the_next_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        {
            let mut writer = Writer::open(&path).unwrap();
            writer.append(parse_stderr("one")).unwrap();
            writer.append(parse_stderr("two")).unwrap();
        }
        // A write cut off by a crash: no newline at the end.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"seq":3,"ts":"x","kind":"stde"#)
            .unwrap();
        drop(file);

        assert_eq!(read(&path, 0).unwrap().len(), 2, "readers skip it");

        let mut writer = reopen(&path);
        assert_eq!(writer.next_seq(), 3, "numbering continues after the last");
        writer.append(parse_stderr("three")).unwrap();
        let all = read(&path, 0).unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(!text(&path).contains("\"stde{"), "the cut line is gone");
        assert!(text(&path).ends_with('\n'));
    }

    #[test]
    fn a_whole_event_cut_before_its_newline_is_skipped_and_its_number_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        {
            let mut writer = Writer::open(&path).unwrap();
            writer.append(parse_stderr("one")).unwrap();
        }
        // The crash cut only the newline: the line parses, but it was
        // never a complete write.
        let cut = parse_stderr("lost").into_event(2, "x".to_owned());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&serde_json::to_vec(&cut).unwrap()).unwrap();
        drop(file);

        assert_eq!(
            read(&path, 0).unwrap().len(),
            1,
            "a reader never sees an event the next writer drops"
        );
        let mut writer = reopen(&path);
        assert_eq!(writer.next_seq(), 2, "the cut event's number is reused");
        writer.append(parse_stderr("two")).unwrap();
        let all = read(&path, 0).unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(all[1].raw, Value::String("two".to_owned()));
    }

    #[test]
    fn claude_init_is_found_by_type_after_hook_events() {
        let hook = parse_stdout(
            Harness::ClaudeCode,
            r#"{"type":"system","subtype":"hook_started","hook_event":"SessionStart","session_id":"u"}"#,
        );
        assert_eq!(hook.kind, EventKind::Other);
        let init = parse_stdout(
            Harness::ClaudeCode,
            r#"{"type":"system","subtype":"init","model":"opus","session_id":"u"}"#,
        );
        assert_eq!(init.kind, EventKind::Init);
        assert_eq!(init.conversation.as_deref(), Some("u"));
    }

    #[test]
    fn a_claude_tool_call_and_result_are_summarized() {
        let tool = parse_stdout(
            Harness::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"cargo test"}}]},"session_id":"u"}"#,
        );
        assert_eq!(tool.kind, EventKind::Tool);
        assert_eq!(tool.summary, "tool Bash: cargo test");
        let result = parse_stdout(
            Harness::ClaudeCode,
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.41,"session_id":"u"}"#,
        );
        assert_eq!(result.summary, "result: success, $0.41");
        assert_eq!(result.cost_usd, Some(0.41));
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn codex_lines_carry_the_thread_and_the_items() {
        let started = parse_stdout(
            Harness::Codex,
            r#"{"type":"thread.started","thread_id":"01a2"}"#,
        );
        assert_eq!(started.kind, EventKind::Init);
        assert_eq!(started.conversation.as_deref(), Some("01a2"));
        let command = parse_stdout(
            Harness::Codex,
            r#"{"type":"item.started","item":{"id":"i","type":"command_execution","command":"ls","status":"in_progress"}}"#,
        );
        assert_eq!(command.summary, "tool shell: ls");
        let done = parse_stdout(
            Harness::Codex,
            r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#,
        );
        assert_eq!(done.kind, EventKind::Result);
        assert_eq!(done.cost_usd, None, "Codex reports no cost");
    }

    #[test]
    fn a_line_that_is_not_json_is_kept_as_text() {
        let parsed = parse_stdout(Harness::Codex, "plain words");
        assert_eq!(parsed.kind, EventKind::Other);
        assert_eq!(parsed.raw, Value::String("plain words".to_owned()));
    }
}
