//! The TUI's keys: sections of `~/.config/kersh/keys.yml` over the
//! shared presets, as mdstore-core's kit reads them.
//!
//! - `list`: the session list, one command per session command.
//! - `attach`: the live screen of one session, its scroll keys, and the
//!   key that leaves it. `C-]` also leaves a real attach, which the
//!   session server's client handles, so the help lists it here too.
//! - `form`: moving between the start form's fields.
//! - `editor`: the shared Editor, for a start, resume, or send prompt.
//!
//! The same tables drive the keys and the help screen, so no key works
//! without being listed.

use mdstore::tui::keymap::{Command, Keymap, KeymapError, KeysFile, Section};
use mdstore::tui::{EditAction, editor};

/// What a key does on the session list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListAction {
    Next,
    Previous,
    First,
    Last,
    Start,
    Stop,
    Resume,
    Restart,
    Attach,
    Send,
    Remove,
    View,
    Refresh,
    Help,
    Quit,
}

impl Command for ListAction {
    const ALL: &'static [Self] = &[
        Self::Next,
        Self::Previous,
        Self::First,
        Self::Last,
        Self::Start,
        Self::Stop,
        Self::Resume,
        Self::Restart,
        Self::Attach,
        Self::Send,
        Self::Remove,
        Self::View,
        Self::Refresh,
        Self::Help,
        Self::Quit,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Next => "next-session",
            Self::Previous => "previous-session",
            Self::First => "first-session",
            Self::Last => "last-session",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Resume => "resume",
            Self::Restart => "restart",
            Self::Attach => "attach",
            Self::Send => "send",
            Self::Remove => "remove",
            Self::View => "view-screen",
            Self::Refresh => "refresh",
            Self::Help => "help",
            Self::Quit => "quit",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Next => "next session",
            Self::Previous => "previous session",
            Self::First => "first session",
            Self::Last => "last session",
            Self::Start => "start a session",
            Self::Stop => "stop it and close its pane",
            Self::Resume => "resume it with a prompt",
            Self::Restart => "restart it on its conversation",
            Self::Attach => "attach; C-] comes back",
            Self::Send => "type text into it",
            Self::Remove => "remove an ended session",
            Self::View => "watch its screen here",
            Self::Refresh => "read the sessions again",
            Self::Help => "this help",
            Self::Quit => "quit",
        }
    }
}

type L = ListAction;

/// The `list` section.
pub const LIST: Section<ListAction> = Section {
    name: "list",
    default: &[
        ("j", L::Next),
        ("<down>", L::Next),
        ("k", L::Previous),
        ("<up>", L::Previous),
        ("<home>", L::First),
        ("<end>", L::Last),
        ("s", L::Start),
        ("x", L::Stop),
        ("r", L::Resume),
        ("R", L::Restart),
        ("a", L::Attach),
        ("RET", L::Attach),
        ("i", L::Send),
        ("d", L::Remove),
        ("v", L::View),
        ("g", L::Refresh),
        ("?", L::Help),
        ("q", L::Quit),
    ],
    emacs: &[
        ("n", L::Next),
        ("C-n", L::Next),
        ("<down>", L::Next),
        ("p", L::Previous),
        ("C-p", L::Previous),
        ("<up>", L::Previous),
        ("M-<", L::First),
        ("<home>", L::First),
        ("M->", L::Last),
        ("<end>", L::Last),
        ("s", L::Start),
        ("k", L::Stop),
        ("r", L::Resume),
        ("R", L::Restart),
        ("RET", L::Attach),
        ("a", L::Attach),
        ("i", L::Send),
        ("D", L::Remove),
        ("v", L::View),
        ("g", L::Refresh),
        ("?", L::Help),
        ("q", L::Quit),
        ("C-x C-c", L::Quit),
    ],
};

/// What a key does on a session's live screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachAction {
    Detach,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Attach,
}

impl Command for AttachAction {
    const ALL: &'static [Self] = &[
        Self::Detach,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::PageUp,
        Self::PageDown,
        Self::Top,
        Self::Bottom,
        Self::Attach,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::ScrollUp => "scroll-up",
            Self::ScrollDown => "scroll-down",
            Self::PageUp => "page-up",
            Self::PageDown => "page-down",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Attach => "attach",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Detach => "back to the list",
            Self::ScrollUp => "up a line",
            Self::ScrollDown => "down a line",
            Self::PageUp => "up a page",
            Self::PageDown => "down a page",
            Self::Top => "the top",
            Self::Bottom => "the bottom",
            Self::Attach => "attach; C-] comes back",
        }
    }
}

type A = AttachAction;

/// The `attach` section.
pub const ATTACH: Section<AttachAction> = Section {
    name: "attach",
    default: &[
        ("C-]", A::Detach),
        ("q", A::Detach),
        ("k", A::ScrollUp),
        ("<up>", A::ScrollUp),
        ("j", A::ScrollDown),
        ("<down>", A::ScrollDown),
        ("<prior>", A::PageUp),
        ("<next>", A::PageDown),
        ("SPC", A::PageDown),
        ("<home>", A::Top),
        ("<end>", A::Bottom),
        ("a", A::Attach),
        ("RET", A::Attach),
    ],
    emacs: &[
        ("C-]", A::Detach),
        ("q", A::Detach),
        ("p", A::ScrollUp),
        ("C-p", A::ScrollUp),
        ("<up>", A::ScrollUp),
        ("n", A::ScrollDown),
        ("C-n", A::ScrollDown),
        ("<down>", A::ScrollDown),
        ("M-v", A::PageUp),
        ("<prior>", A::PageUp),
        ("C-v", A::PageDown),
        ("SPC", A::PageDown),
        ("<next>", A::PageDown),
        ("M-<", A::Top),
        ("<home>", A::Top),
        ("M->", A::Bottom),
        ("<end>", A::Bottom),
        ("RET", A::Attach),
        ("a", A::Attach),
    ],
};

/// What a key does between the start form's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormAction {
    NextField,
    PreviousField,
}

impl Command for FormAction {
    const ALL: &'static [Self] = &[Self::NextField, Self::PreviousField];

    fn name(self) -> &'static str {
        match self {
            Self::NextField => "next-field",
            Self::PreviousField => "previous-field",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::NextField => "next field",
            Self::PreviousField => "previous field",
        }
    }
}

/// The `form` section. Single keys only, so it never holds a prefix the
/// editor's keys would have to wait behind.
pub const FORM: Section<FormAction> = Section {
    name: "form",
    default: &[
        ("TAB", FormAction::NextField),
        ("<backtab>", FormAction::PreviousField),
    ],
    emacs: &[
        ("TAB", FormAction::NextField),
        ("<backtab>", FormAction::PreviousField),
    ],
};

/// Every section the keys file may hold.
pub const SECTIONS: &[&str] = &["list", "attach", "form", "editor"];

/// The keymaps, read from one keys file.
#[derive(Debug, Clone)]
pub struct Keys {
    pub list: Keymap<ListAction>,
    pub attach: Keymap<AttachAction>,
    pub form: Keymap<FormAction>,
    pub editor: Keymap<EditAction>,
}

impl Keys {
    /// The keymaps of a keys file, refusing a section kersh does not read.
    pub fn from_file(file: &KeysFile) -> Result<Self, KeymapError> {
        file.check_sections(SECTIONS)?;
        Ok(Self {
            list: file.keymap(&LIST)?,
            attach: file.keymap(&ATTACH)?,
            form: file.keymap(&FORM)?,
            editor: file.keymap(&editor::SECTION)?,
        })
    }

    /// The user's keys: `~/.config/kersh/keys.yml` over its preset.
    pub fn load() -> Result<Self, KeymapError> {
        let file = match crate::tui::TOOL.and_then(mdstore::tui::keymap::keys_path) {
            Some(path) => KeysFile::load(&path)?,
            None => KeysFile::default(),
        };
        Self::from_file(&file)
    }

    /// The keys for `action` on the list, joined for a hint.
    #[must_use]
    pub fn list_keys(&self, action: ListAction) -> String {
        self.list.keys_for(action).join(", ")
    }

    /// The editor keys that finish or cancel a prompt. Save also
    /// finishes one; it is named only when no finish key is bound.
    #[must_use]
    pub fn prompt_keys(&self) -> (String, String) {
        let mut finish = self.editor.keys_for(EditAction::Finish);
        if finish.is_empty() {
            finish = self.editor.keys_for(EditAction::Save);
        }
        (
            finish.join(", "),
            self.editor.keys_for(EditAction::Cancel).join(", "),
        )
    }
}
