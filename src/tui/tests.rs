//! The TUI, driven by key events and drawn to a test backend.
//!
//! Each snapshot is the rendered screen at a fixed size over fixed
//! records, so a change to what the user sees shows up as a diff to
//! review. The calls go to a fake that records them, so each key's call
//! is checked without a host.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use mdstore::tui::keymap::{Keymap, KeysFile, Preset};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

use super::app::{App, Calls, Effect, Field, Message, Mode};
use super::keys::{ATTACH, FORM, Keys, LIST};
use super::ui;
use crate::session::{Harness, Record, SessionError, StartRequest, State};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

fn record(id: &str, state: &str, harness: &str, mode: &str, name: &str) -> Record {
    serde_json::from_value(json!({
        "schema": 1,
        "id": id,
        "name": name,
        "harness": harness,
        "mode": mode,
        "state": state,
        "build": {"installer": "path", "path": "/acme/bin/tool", "version": "1.0.0"},
        "environment": {"kind": "local", "cwd": "/acme/work", "tempdir": null},
        "agent": null,
        "runtime": {"name": null, "settings": {
            "harness": harness, "mode": mode, "model": null, "environment": "local",
            "profile": null, "resume_prompt": null, "tempdir": false, "env": {}
        }},
        "system_prompt": null,
        "conversation": null,
        "agent_flags": [],
        "first_prompt": null,
        "labels": {},
        "group": null,
        "pgid": null,
        "boot_id": null,
        "wrapper_pid": null,
        "harness_pid": null,
        "rmux_session": format!("kersh-{id}"),
        "pane_size": {"cols": 100, "rows": 30},
        "created_at": "2026-11-25T10:00:00Z",
        "ended_at": null,
        "exit_status": null,
        "reason": null,
        "restores": [],
        "held_run": null,
        "launches": 1
    }))
    .unwrap()
}

/// Three sessions: a running interactive one, a held one, and an ended
/// headless one.
fn records() -> Vec<Record> {
    let running = record("a1b2c3", "running", "claude-code", "interactive", "triage");
    let mut held = record("d4e5f6", "held", "codex", "interactive", "fixer");
    held.reason = Some("exit status 3".to_owned());
    held.exit_status = Some(3);
    held.group = Some("acme".to_owned());
    let mut ended = record("g7h8i9", "ended", "rig", "headless", "reviewer");
    ended.reason = Some("exit status 0".to_owned());
    ended.progress = Some("result success".to_owned());
    vec![running, held, ended]
}

/// The calls the TUI made, and canned answers.
struct Fake {
    records: Vec<Record>,
    log: RefCell<Vec<String>>,
    starts: RefCell<Vec<StartRequest>>,
    refuse_start: bool,
}

impl Fake {
    fn new() -> Self {
        Self {
            records: records(),
            log: RefCell::new(Vec::new()),
            starts: RefCell::new(Vec::new()),
            refuse_start: false,
        }
    }

    fn find(&self, id: &str) -> Result<Record, SessionError> {
        self.records
            .iter()
            .find(|record| record.id == id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound { id: id.to_owned() })
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl Calls for Fake {
    fn ps(&self) -> Result<Vec<Record>, SessionError> {
        Ok(self.records.clone())
    }

    fn screen(&self, id: &str) -> Result<Vec<String>, SessionError> {
        match id {
            "a1b2c3" => Ok(vec![
                "fake claude waiting".to_owned(),
                "> review the open diff".to_owned(),
                String::new(),
            ]),
            "d4e5f6" => Ok(vec![
                "fake codex fails".to_owned(),
                "[kersh] codex exited with status 3. RET runs it again.".to_owned(),
            ]),
            _ => Err(SessionError::failed(format!(
                "session {id} (ended) has no screen to read: the session's pane is closed"
            ))),
        }
    }

    fn start(&self, request: &StartRequest) -> Result<Record, SessionError> {
        self.starts.borrow_mut().push(request.clone());
        if self.refuse_start {
            return Err(SessionError::usage(
                "a headless start needs a prompt; give --prompt-file",
            ));
        }
        Ok(record("n3w000", "running", "codex", "headless", "new"))
    }

    fn stop(&self, id: &str) -> Result<Record, SessionError> {
        self.log.borrow_mut().push(format!("stop {id}"));
        let mut record = self.find(id)?;
        record.state = State::Ended;
        Ok(record)
    }

    fn resume(&self, id: &str, prompt: Option<&str>) -> Result<Record, SessionError> {
        self.log
            .borrow_mut()
            .push(format!("resume {id} {prompt:?}"));
        let mut record = self.find(id)?;
        record.state = State::Running;
        Ok(record)
    }

    fn restart(&self, id: &str) -> Result<Record, SessionError> {
        self.log.borrow_mut().push(format!("restart {id}"));
        self.find(id)
    }

    fn send(&self, id: &str, text: &str) -> Result<(), SessionError> {
        self.log.borrow_mut().push(format!("send {id} {text:?}"));
        Ok(())
    }

    fn rm(&self, id: &str) -> Result<(), SessionError> {
        self.log.borrow_mut().push(format!("rm {id}"));
        Ok(())
    }
}

fn keys(preset: &str) -> Keys {
    let file = KeysFile::parse(
        &format!("format: 1\npreset: {preset}\n"),
        Path::new("/acme/.config/kersh/keys.yml"),
    )
    .unwrap();
    Keys::from_file(&file).unwrap()
}

fn app_with(fake: Fake, preset: &str) -> App<Fake> {
    App::new(fake, keys(preset), PathBuf::from("/acme/work"))
}

fn app() -> App<Fake> {
    app_with(Fake::new(), "default")
}

fn press(app: &mut App<Fake>, code: KeyCode) -> Option<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(app: &mut App<Fake>, c: char) -> Option<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn typed(app: &mut App<Fake>, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn draw(app: &App<Fake>) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    terminal.draw(|frame| ui::render(app, frame)).unwrap();
    terminal
}

#[test]
fn every_preset_table_is_valid() {
    for preset in [Preset::Default, Preset::Emacs] {
        let _ = Keymap::preset(&LIST, preset);
        let _ = Keymap::preset(&ATTACH, preset);
        let _ = Keymap::preset(&FORM, preset);
    }
}

#[test]
fn a_keys_file_section_kersh_does_not_read_is_refused() {
    let file = KeysFile::parse("format: 1\nlsit:\n  q: quit\n", Path::new("/k.yml")).unwrap();
    let error = Keys::from_file(&file).unwrap_err().to_string();
    assert!(
        error.contains("lsit") && error.contains("attach"),
        "{error}"
    );
}

#[test]
fn the_session_list() {
    let app = app();
    insta::assert_snapshot!(draw(&app).backend());
}

#[test]
fn a_held_session() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.current().unwrap().state, State::Held);
    insta::assert_snapshot!(draw(&app).backend());
}

#[test]
fn the_start_form_with_its_prompt_editor() {
    let mut app = app();
    press(&mut app, KeyCode::Char('s'));
    assert!(matches!(app.mode, Mode::Start(_)));
    typed(&mut app, "reviewer");
    press(&mut app, KeyCode::Tab);
    typed(&mut app, "codex");
    press(&mut app, KeyCode::Enter);
    typed(&mut app, "headless");
    // On past the runtime, dir, name, and group to the prompt.
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab);
    }
    typed(&mut app, "review the open diff");
    press(&mut app, KeyCode::Enter);
    typed(&mut app, "and report.");
    insta::assert_snapshot!(draw(&app).backend());
}

#[test]
fn the_keymap_help() {
    let mut app = app();
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.mode, Mode::Help));
    insta::assert_snapshot!(draw(&app).backend());
    press(&mut app, KeyCode::Char('x'));
    assert!(matches!(app.mode, Mode::List), "any key closes the help");
    assert!(
        app.calls.log().is_empty(),
        "the closing key did nothing else"
    );
}

#[test]
fn the_emacs_keymap_help() {
    let mut app = app_with(Fake::new(), "emacs");
    press(&mut app, KeyCode::Char('?'));
    insta::assert_snapshot!(draw(&app).backend());
}

#[test]
fn stop_calls_stop_on_the_selected_session() {
    let mut app = app();
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(app.calls.log(), vec!["stop d4e5f6"]);
    assert_eq!(app.message, Some(Message::Info("d4e5f6 ended".to_owned())));
}

#[test]
fn resume_opens_the_editor_and_sends_its_prompt() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    assert!(matches!(app.mode, Mode::Prompt(_)));
    typed(&mut app, "try again");
    assert!(
        app.calls.log().is_empty(),
        "nothing is sent before the finish"
    );
    ctrl(&mut app, 's');
    assert_eq!(app.calls.log(), vec![r#"resume d4e5f6 Some("try again")"#]);
    assert!(matches!(app.mode, Mode::List));
}

#[test]
fn an_empty_resume_prompt_resumes_without_one() {
    let mut app = app();
    press(&mut app, KeyCode::Char('r'));
    ctrl(&mut app, 's');
    assert_eq!(app.calls.log(), vec!["resume a1b2c3 None"]);
}

#[test]
fn a_cancelled_prompt_calls_nothing() {
    let mut app = app();
    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::List));
    assert!(app.calls.log().is_empty());
}

#[test]
fn send_types_the_text() {
    let mut app = app();
    press(&mut app, KeyCode::Char('i'));
    typed(&mut app, "status?");
    ctrl(&mut app, 's');
    assert_eq!(app.calls.log(), vec![r#"send a1b2c3 "status?""#]);
}

#[test]
fn restart_and_remove_call_their_commands() {
    let mut app = app();
    press(&mut app, KeyCode::Char('R'));
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(app.calls.log(), vec!["restart a1b2c3", "rm g7h8i9"]);
}

#[test]
fn attach_hands_the_selected_session_to_the_event_loop() {
    let mut app = app();
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Attach("a1b2c3".to_owned()))
    );
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Attach("d4e5f6".to_owned()))
    );
    assert!(app.calls.log().is_empty(), "attach makes no host call");
}

#[test]
fn the_screen_view_scrolls_detaches_and_attaches() {
    let mut app = app();
    press(&mut app, KeyCode::Char('v'));
    assert!(matches!(app.mode, Mode::View));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.scroll, 1);
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Attach("a1b2c3".to_owned()))
    );
    ctrl(&mut app, ']');
    assert!(matches!(app.mode, Mode::List), "C-] leaves the screen view");
}

#[test]
fn start_sends_the_form_as_a_start_request() {
    let mut app = app();
    press(&mut app, KeyCode::Char('s'));
    press(&mut app, KeyCode::Tab);
    typed(&mut app, "codex");
    press(&mut app, KeyCode::Tab);
    typed(&mut app, "headless");
    // On past the runtime and dir to the name.
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab);
    }
    typed(&mut app, "new");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    assert!(
        matches!(&app.mode, Mode::Start(form) if Field::ALL[form.focus] == Field::Prompt),
        "the focus is on the prompt"
    );
    typed(&mut app, "say hi");
    ctrl(&mut app, 's');
    let starts = app.calls.starts.borrow().clone();
    assert_eq!(starts.len(), 1);
    let request = &starts[0];
    assert_eq!(request.harness, Some(Harness::Codex));
    assert_eq!(request.mode, Some(crate::session::Mode::Headless));
    assert_eq!(request.name.as_deref(), Some("new"));
    assert_eq!(request.prompt.as_deref(), Some("say hi"));
    assert_eq!(request.cwd.as_deref(), Some(Path::new("/acme/work")));
    assert_eq!(request.agent, None);
    assert!(matches!(app.mode, Mode::List));
    assert_eq!(
        app.message,
        Some(Message::Info("started n3w000".to_owned()))
    );
}

#[test]
fn a_refused_start_keeps_the_form_and_its_text() {
    let mut fake = Fake::new();
    fake.refuse_start = true;
    let mut app = app_with(fake, "default");
    press(&mut app, KeyCode::Char('s'));
    press(&mut app, KeyCode::BackTab);
    typed(&mut app, "draft");
    ctrl(&mut app, 's');
    let Mode::Start(form) = &app.mode else {
        panic!("the form closed");
    };
    assert_eq!(form.editors[form.focus].text().trim_end(), "draft");
    assert!(
        matches!(&app.message, Some(Message::Error(text)) if text.contains("needs a prompt")),
        "{:?}",
        app.message
    );
}

#[test]
fn a_bad_harness_is_refused_before_any_call() {
    let mut app = app();
    press(&mut app, KeyCode::Char('s'));
    press(&mut app, KeyCode::Tab);
    typed(&mut app, "vim");
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::BackTab);
    ctrl(&mut app, 's');
    assert!(app.calls.starts.borrow().is_empty());
    assert!(
        matches!(&app.message, Some(Message::Error(text)) if text.contains("not a harness")),
        "{:?}",
        app.message
    );
}

#[test]
fn the_emacs_preset_drives_the_same_commands() {
    let mut app = app_with(Fake::new(), "emacs");
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('r'));
    typed(&mut app, "go");
    ctrl(&mut app, 'c');
    ctrl(&mut app, 'c');
    assert_eq!(
        app.calls.log(),
        vec!["stop d4e5f6", r#"resume d4e5f6 Some("go")"#]
    );
    ctrl(&mut app, 'x');
    ctrl(&mut app, 'c');
    assert!(app.quit, "C-x C-c quits");
}
