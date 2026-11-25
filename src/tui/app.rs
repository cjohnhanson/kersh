//! The TUI's state and what each key does to it.
//!
//! Every session command goes through [`Calls`], which [`Client`]
//! implements with the same methods the commands call, so a stop here is
//! `kersh stop` and a start is `kersh start`. Tests implement it with a
//! recording fake.

use std::path::PathBuf;

use mdstore::tui::keymap::Lookup;
use mdstore::tui::{Editor, Outcome};
use ratatui::crossterm::event::KeyEvent;

use super::keys::{AttachAction, FormAction, Keys, ListAction};
use crate::client::Client;
use crate::session::{Harness, Mode as SessionMode, PaneSize, Record, SessionError, StartRequest};

/// The session calls the TUI makes.
pub trait Calls {
    fn ps(&self) -> Result<Vec<Record>, SessionError>;
    fn screen(&self, id: &str) -> Result<Vec<String>, SessionError>;
    fn start(&self, request: &StartRequest) -> Result<Record, SessionError>;
    fn stop(&self, id: &str) -> Result<Record, SessionError>;
    fn resume(&self, id: &str, prompt: Option<&str>) -> Result<Record, SessionError>;
    fn restart(&self, id: &str) -> Result<Record, SessionError>;
    fn send(&self, id: &str, text: &str) -> Result<(), SessionError>;
    fn rm(&self, id: &str) -> Result<(), SessionError>;
}

impl Calls for Client {
    fn ps(&self) -> Result<Vec<Record>, SessionError> {
        Self::ps(self, &[], None)
    }

    fn screen(&self, id: &str) -> Result<Vec<String>, SessionError> {
        Self::screen(self, id).map(|screen| screen.lines)
    }

    fn start(&self, request: &StartRequest) -> Result<Record, SessionError> {
        Self::start(self, request)
    }

    fn stop(&self, id: &str) -> Result<Record, SessionError> {
        Self::stop(self, id)
    }

    fn resume(&self, id: &str, prompt: Option<&str>) -> Result<Record, SessionError> {
        Self::resume(self, id, prompt)
    }

    fn restart(&self, id: &str) -> Result<Record, SessionError> {
        Self::restart(self, id)
    }

    fn send(&self, id: &str, text: &str) -> Result<(), SessionError> {
        Self::send(self, id, text)
    }

    fn rm(&self, id: &str) -> Result<(), SessionError> {
        Self::rm(self, id)
    }
}

/// What the event loop does after a key: hand the terminal to an attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Attach(String),
}

/// The footer line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Info(String),
    Error(String),
}

/// One field of the start form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Agent,
    Harness,
    Mode,
    Runtime,
    Cwd,
    Name,
    Group,
    Prompt,
}

impl Field {
    pub const ALL: [Self; 8] = [
        Self::Agent,
        Self::Harness,
        Self::Mode,
        Self::Runtime,
        Self::Cwd,
        Self::Name,
        Self::Group,
        Self::Prompt,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Harness => "harness",
            Self::Mode => "mode",
            Self::Runtime => "runtime",
            Self::Cwd => "dir",
            Self::Name => "name",
            Self::Group => "group",
            Self::Prompt => "prompt",
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Self::Agent => "an agent file; or leave empty and give a harness",
            Self::Harness => "rig, claude-code, or codex",
            Self::Mode => "headless or interactive",
            Self::Runtime => "a runtime from runtimes.yml",
            Self::Cwd => "the working directory",
            Self::Name => "a name for the session",
            Self::Group => "a group, for its running limit",
            Self::Prompt => "the first prompt; a headless start needs one",
        }
    }
}

/// The start form: one single-line editor per field, and the prompt.
pub struct StartForm {
    pub editors: Vec<Editor>,
    pub focus: usize,
}

/// What a prompt box sends when it finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Resume(String),
    Send(String),
}

/// A one-editor box: a resume prompt or text to send.
pub struct PromptBox {
    pub kind: PromptKind,
    pub editor: Editor,
}

/// What the screen shows.
pub enum Mode {
    List,
    Help,
    View,
    Start(Box<StartForm>),
    Prompt(Box<PromptBox>),
}

/// The TUI's state.
pub struct App<C: Calls> {
    pub calls: C,
    pub keys: Keys,
    pub records: Vec<Record>,
    pub selected: usize,
    /// The selected session's screen, read on each refresh.
    pub preview: Result<Vec<String>, String>,
    /// How far the live screen is scrolled from its top.
    pub scroll: usize,
    pub mode: Mode,
    pub message: Option<Message>,
    pub quit: bool,
    /// The directory a start runs in when the form leaves it empty.
    pub cwd: PathBuf,
    /// The terminal's size, passed with a start as the CLI does.
    pub size: Option<PaneSize>,
}

fn error_text(error: &SessionError) -> String {
    error.to_string()
}

impl<C: Calls> App<C> {
    pub fn new(calls: C, keys: Keys, cwd: PathBuf) -> Self {
        let mut app = Self {
            calls,
            keys,
            records: Vec::new(),
            selected: 0,
            preview: Ok(Vec::new()),
            scroll: 0,
            mode: Mode::List,
            message: None,
            quit: false,
            cwd,
            size: None,
        };
        app.refresh();
        app
    }

    /// The selected session.
    pub fn current(&self) -> Option<&Record> {
        self.records.get(self.selected)
    }

    fn current_id(&self) -> Option<String> {
        self.current().map(|record| record.id.clone())
    }

    /// Read the sessions again, keeping the selection on the same id.
    pub fn refresh(&mut self) {
        let keep = self.current_id();
        match self.calls.ps() {
            Ok(records) => self.records = records,
            Err(error) => self.message = Some(Message::Error(error_text(&error))),
        }
        if let Some(id) = keep
            && let Some(index) = self.records.iter().position(|record| record.id == id)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.records.len().saturating_sub(1));
        self.preview = match self.current_id() {
            Some(id) => self.calls.screen(&id).map_err(|error| error_text(&error)),
            None => Ok(Vec::new()),
        };
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.records.len().saturating_sub(1));
        self.scroll = 0;
        self.preview = match self.current_id() {
            Some(id) => self.calls.screen(&id).map_err(|error| error_text(&error)),
            None => Ok(Vec::new()),
        };
    }

    fn info(&mut self, text: String) {
        self.message = Some(Message::Info(text));
    }

    fn fail(&mut self, error: &SessionError) {
        self.message = Some(Message::Error(error_text(error)));
    }

    /// Handle one key press.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Effect> {
        match &mut self.mode {
            Mode::List => self.list_key(&key),
            Mode::Help => {
                // Any key leaves the help.
                self.mode = Mode::List;
                None
            }
            Mode::View => self.view_key(&key),
            Mode::Start(_) => {
                self.form_key(&key);
                None
            }
            Mode::Prompt(_) => {
                self.prompt_key(&key);
                None
            }
        }
    }

    fn list_key(&mut self, key: &KeyEvent) -> Option<Effect> {
        let action = match self.keys.list.feed(key) {
            Lookup::Action(action) => action,
            Lookup::Pending | Lookup::Ignored => return None,
            Lookup::Cancelled => {
                self.info("Quit".to_owned());
                return None;
            }
            Lookup::Undefined(text) => {
                self.info(text);
                return None;
            }
            Lookup::Unbound(key) => {
                let help = self.keys.list_keys(ListAction::Help);
                self.info(format!("{key} is undefined; {help} lists the keys"));
                return None;
            }
        };
        self.message = None;
        self.list_action(action)
    }

    fn list_action(&mut self, action: ListAction) -> Option<Effect> {
        match action {
            ListAction::Next => self.select(self.selected.saturating_add(1)),
            ListAction::Previous => self.select(self.selected.saturating_sub(1)),
            ListAction::First => self.select(0),
            ListAction::Last => self.select(self.records.len().saturating_sub(1)),
            ListAction::Refresh => self.refresh(),
            ListAction::Help => self.mode = Mode::Help,
            ListAction::Quit => self.quit = true,
            ListAction::Start => self.open_start(),
            ListAction::Attach => return self.current_id().map(Effect::Attach),
            ListAction::View => {
                if self.current().is_some() {
                    self.scroll = 0;
                    self.mode = Mode::View;
                }
            }
            ListAction::Stop => self.on_current(|calls, id| {
                calls
                    .stop(id)
                    .map(|record| format!("{id} {}", record.state))
            }),
            ListAction::Restart => self.on_current(|calls, id| {
                calls
                    .restart(id)
                    .map(|record| format!("{id} restarted, {}", record.state))
            }),
            ListAction::Remove => {
                self.on_current(|calls, id| calls.rm(id).map(|()| format!("{id} removed")));
            }
            ListAction::Resume => self.open_prompt(PromptKind::Resume),
            ListAction::Send => self.open_prompt(PromptKind::Send),
        }
        None
    }

    /// Run a call on the selected session, then read the sessions again.
    fn on_current(&mut self, call: impl FnOnce(&C, &str) -> Result<String, SessionError>) {
        let Some(id) = self.current_id() else {
            self.info("no session is selected".to_owned());
            return;
        };
        match call(&self.calls, &id) {
            Ok(text) => self.info(text),
            Err(error) => self.fail(&error),
        }
        self.refresh();
    }

    fn open_prompt(&mut self, kind: fn(String) -> PromptKind) {
        let Some(id) = self.current_id() else {
            self.info("no session is selected".to_owned());
            return;
        };
        let kind = kind(id);
        let mut editor = Editor::new("", self.keys.editor.clone());
        editor.set_placeholder_text(match &kind {
            PromptKind::Resume(_) => "the prompt; a headless session needs one",
            PromptKind::Send(_) => "the text to type, then Enter",
        });
        self.mode = Mode::Prompt(Box::new(PromptBox { kind, editor }));
    }

    fn open_start(&mut self) {
        let editors = Field::ALL
            .iter()
            .map(|field| {
                let mut editor = if *field == Field::Prompt {
                    Editor::new("", self.keys.editor.clone())
                } else {
                    Editor::single_line("", self.keys.editor.clone())
                };
                editor.set_placeholder_text(field.hint());
                editor
            })
            .collect();
        self.keys.form.clear();
        self.mode = Mode::Start(Box::new(StartForm { editors, focus: 0 }));
    }

    fn view_key(&mut self, key: &KeyEvent) -> Option<Effect> {
        let action = match self.keys.attach.feed(key) {
            Lookup::Action(action) => action,
            Lookup::Undefined(text) => {
                self.info(text);
                return None;
            }
            _ => return None,
        };
        let page = 10;
        match action {
            AttachAction::Detach => self.mode = Mode::List,
            AttachAction::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            AttachAction::ScrollDown => self.scroll = self.scroll.saturating_add(1),
            AttachAction::PageUp => self.scroll = self.scroll.saturating_sub(page),
            AttachAction::PageDown => self.scroll = self.scroll.saturating_add(page),
            AttachAction::Top => self.scroll = 0,
            AttachAction::Bottom => self.scroll = usize::MAX,
            AttachAction::Attach => return self.current_id().map(Effect::Attach),
        }
        None
    }

    fn form_key(&mut self, key: &KeyEvent) {
        let Mode::Start(form) = &mut self.mode else {
            return;
        };
        let Some(editor) = form.editors.get_mut(form.focus) else {
            return;
        };
        // The form's keys are single keys; while the editor waits on a
        // prefix, the key is the editor's.
        if editor.keymap().pending().is_none() {
            match self.keys.form.feed(key) {
                Lookup::Action(FormAction::NextField) => {
                    form.focus = (form.focus + 1) % form.editors.len();
                    return;
                }
                Lookup::Action(FormAction::PreviousField) => {
                    form.focus = (form.focus + form.editors.len() - 1) % form.editors.len();
                    return;
                }
                _ => self.keys.form.clear(),
            }
        }
        let prompt = Field::ALL.get(form.focus) == Some(&Field::Prompt);
        match editor.handle(key) {
            Outcome::Continue => {}
            Outcome::Cancel => {
                self.mode = Mode::List;
                self.info("start cancelled".to_owned());
            }
            Outcome::Save(_) | Outcome::Finish(_) if !prompt => {
                form.focus = (form.focus + 1) % form.editors.len();
            }
            Outcome::Save(_) | Outcome::Finish(_) => self.submit_start(),
        }
    }

    /// The start the form describes.
    fn start_request(&self, form: &StartForm) -> Result<StartRequest, String> {
        let value = |field: Field| -> Option<String> {
            let index = Field::ALL.iter().position(|f| *f == field)?;
            // The editor ends its text with a newline, as a file body.
            let text = form.editors.get(index)?.text();
            let text = if field == Field::Prompt {
                text.trim_end_matches('\n').to_owned()
            } else {
                text.trim().to_owned()
            };
            (!text.trim().is_empty()).then_some(text)
        };
        let harness = value(Field::Harness)
            .map(|text| text.parse::<Harness>())
            .transpose()?;
        let mode = value(Field::Mode)
            .map(|text| text.parse::<SessionMode>())
            .transpose()?;
        let cwd = value(Field::Cwd).map_or_else(|| self.cwd.clone(), PathBuf::from);
        Ok(StartRequest {
            agent: value(Field::Agent),
            runtime: value(Field::Runtime),
            harness,
            mode,
            cwd: Some(cwd),
            name: value(Field::Name),
            group: value(Field::Group),
            prompt: value(Field::Prompt),
            size: self.size,
            ..StartRequest::default()
        })
    }

    fn submit_start(&mut self) {
        let Mode::Start(form) = &self.mode else {
            return;
        };
        let request = match self.start_request(form) {
            Ok(request) => request,
            Err(error) => {
                self.set_form_message(&error);
                return;
            }
        };
        match self.calls.start(&request) {
            Ok(record) => {
                self.mode = Mode::List;
                self.info(format!("started {}", record.id));
                self.refresh();
                if let Some(index) = self.records.iter().position(|r| r.id == record.id) {
                    self.select(index);
                }
            }
            // The form stays open with the refusal, so nothing typed is
            // lost.
            Err(error) => self.set_form_message(&error_text(&error)),
        }
    }

    fn set_form_message(&mut self, text: &str) {
        if let Mode::Start(form) = &mut self.mode
            && let Some(editor) = form.editors.get_mut(form.focus)
        {
            editor.set_message(text);
        }
        self.message = Some(Message::Error(text.to_owned()));
    }

    fn prompt_key(&mut self, key: &KeyEvent) {
        let Mode::Prompt(prompt) = &mut self.mode else {
            return;
        };
        let text = match prompt.editor.handle(key) {
            Outcome::Continue => return,
            Outcome::Cancel => {
                self.mode = Mode::List;
                self.info("cancelled".to_owned());
                return;
            }
            Outcome::Save(text) | Outcome::Finish(text) => text.trim_end_matches('\n').to_owned(),
        };
        let kind = prompt.kind.clone();
        let result = match &kind {
            PromptKind::Resume(id) => {
                let prompt = (!text.trim().is_empty()).then_some(text.as_str());
                self.calls
                    .resume(id, prompt)
                    .map(|record| format!("{id} resumed, {}", record.state))
            }
            PromptKind::Send(id) => self.calls.send(id, &text).map(|()| format!("sent to {id}")),
        };
        match result {
            Ok(done) => {
                self.mode = Mode::List;
                self.info(done);
                self.refresh();
            }
            // The box stays open with the refusal, so the text is kept.
            Err(error) => {
                let text = error_text(&error);
                if let Mode::Prompt(prompt) = &mut self.mode {
                    prompt.editor.set_message(text.clone());
                }
                self.message = Some(Message::Error(text));
            }
        }
    }
}
