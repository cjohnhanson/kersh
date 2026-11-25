//! `kersh tui`: browse and drive sessions in the terminal.
//!
//! The UI is one more front end on [`crate::Client`]: each key that acts
//! makes the call the matching command makes. An attach releases the
//! terminal and runs this binary's `attach <id>` as a foreground child,
//! so the session takes the terminal exactly as `kersh attach` gives it;
//! `C-]` ends the child and the list comes back.

mod app;
mod keys;
mod ui;

#[cfg(test)]
mod tests;

use std::io::IsTerminal as _;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

pub use app::{App, Calls, Effect};
pub use keys::Keys;

/// The name the keys file is found under: `~/.config/kersh/keys.yml`.
pub const TOOL: Option<mdstore::ToolName<'static>> = mdstore::ToolName::new("kersh");

/// How often the list and the selected screen are read again.
const REFRESH: Duration = Duration::from_secs(1);

/// Run the terminal UI until the user quits.
pub fn run() -> Result<(), String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("kersh tui needs an interactive terminal; use `kersh ps` in a script".into());
    }
    // Read the keys before taking the terminal, so a bad keys file is
    // reported on an ordinary screen.
    let keys = Keys::load().map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let client = crate::Client::discover(&cwd);
    let mut app = App::new(client, keys, cwd);
    let mut terminal = ratatui::try_init().map_err(|error| error.to_string())?;
    let outcome = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    outcome
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App<crate::Client>) -> Result<(), String> {
    while !app.quit {
        app.size = crate::mux::terminal_size().map(|size| crate::session::PaneSize {
            cols: size.cols,
            rows: size.rows,
        });
        terminal
            .draw(|frame| ui::render(app, frame))
            .map_err(|error| error.to_string())?;
        if !event::poll(REFRESH).map_err(|error| error.to_string())? {
            app.refresh();
            continue;
        }
        let effect = match event::read().map_err(|error| error.to_string())? {
            // Only a press acts: some terminals report a release too.
            Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
            _ => None,
        };
        if let Some(Effect::Attach(id)) = effect {
            ratatui::restore();
            let status = attach(&id);
            *terminal = ratatui::try_init().map_err(|error| error.to_string())?;
            terminal.clear().map_err(|error| error.to_string())?;
            app.message = Some(match status {
                Ok(()) => app::Message::Info(format!("detached from {id}")),
                Err(error) => app::Message::Error(error),
            });
            app.refresh();
        }
    }
    Ok(())
}

/// Run `kersh attach <id>` in the foreground, with this terminal.
fn attach(id: &str) -> Result<(), String> {
    let mut argv = crate::service::entry();
    argv.extend(["attach".to_owned(), id.to_owned()]);
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| "no program to run".to_owned())?;
    let status = std::process::Command::new(program)
        .args(rest)
        .status()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("attach {id} exited with {status}"))
    }
}
