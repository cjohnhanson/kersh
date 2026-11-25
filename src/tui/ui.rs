//! Drawing the TUI: the session list with the selected session's screen,
//! the live screen, the start form, a prompt box, and the help.

use mdstore::tui::Command as _;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};

use super::app::{App, Calls, Field, Message, Mode, PromptKind};
use super::keys::{AttachAction, ListAction};
use crate::session::{Record, State};

fn state_style(state: State) -> Style {
    match state {
        State::Running => Style::new().fg(Color::Green),
        State::Held => Style::new().fg(Color::Yellow),
        State::Ended => Style::new().add_modifier(Modifier::DIM),
        State::Lost => Style::new().fg(Color::Red),
    }
}

fn who(record: &Record) -> String {
    record
        .name
        .clone()
        .or_else(|| record.agent.as_ref().map(|agent| agent.name.clone()))
        .unwrap_or_else(|| "-".to_owned())
}

/// Draw the whole screen.
pub fn render<C: Calls>(app: &App<C>, frame: &mut Frame<'_>) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    match &app.mode {
        Mode::View => view(app, frame, main),
        _ => list(app, frame, main),
    }
    frame.render_widget(Paragraph::new(footer_line(app)), footer);
    match &app.mode {
        Mode::Help => help(app, frame, main),
        Mode::Start(_) => start_form(app, frame, main),
        Mode::Prompt(_) => prompt_box(app, frame, main),
        Mode::List | Mode::View => {}
    }
}

fn footer_line<C: Calls>(app: &App<C>) -> Line<'static> {
    if let Some(message) = &app.message {
        return match message {
            Message::Info(text) => Line::raw(text.clone()),
            Message::Error(text) => {
                Line::styled(format!("kersh: {text}"), Style::new().fg(Color::Red))
            }
        };
    }
    let keys = &app.keys;
    let hint = |action: ListAction, label: &str| {
        let bound = keys.list_keys(action);
        (!bound.is_empty()).then(|| format!("{bound} {label}"))
    };
    let text = match &app.mode {
        Mode::View => {
            let detach = keys.attach.keys_for(AttachAction::Detach).join(", ");
            let attach = keys.attach.keys_for(AttachAction::Attach).join(", ");
            format!("{detach} back  {attach} attach")
        }
        _ => [
            hint(ListAction::Start, "start"),
            hint(ListAction::Stop, "stop"),
            hint(ListAction::Resume, "resume"),
            hint(ListAction::Attach, "attach"),
            hint(ListAction::View, "screen"),
            hint(ListAction::Help, "help"),
            hint(ListAction::Quit, "quit"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  "),
    };
    Line::styled(text, Style::new().add_modifier(Modifier::DIM))
}

fn list<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Min(4)]).areas(area);
    let title = format!(" kersh · {} sessions ", app.records.len());
    if app.records.is_empty() {
        let start = app.keys.list_keys(ListAction::Start);
        let text = format!("no sessions. {start} starts one.");
        frame.render_widget(
            Paragraph::new(text).block(Block::new().borders(Borders::ALL).title(title)),
            table_area,
        );
    } else {
        let rows = app.records.iter().map(|record| {
            Row::new(vec![
                Cell::from(record.id.clone()),
                Cell::from(record.state.to_string()).style(state_style(record.state)),
                Cell::from(record.harness.to_string()),
                Cell::from(record.mode.to_string()),
                Cell::from(record.group.clone().unwrap_or_else(|| "-".to_owned())),
                Cell::from(who(record)),
                Cell::from(
                    record
                        .progress
                        .clone()
                        .or_else(|| record.reason.clone())
                        .unwrap_or_default(),
                ),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(7),
                Constraint::Length(11),
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new([
                "ID", "STATE", "HARNESS", "MODE", "GROUP", "NAME", "PROGRESS",
            ])
            .style(Style::new().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ")
        .block(Block::new().borders(Borders::ALL).title(title));
        let mut state = TableState::default().with_selected(Some(app.selected));
        frame.render_stateful_widget(table, table_area, &mut state);
    }
    detail(app, frame, detail_area);
}

/// What the selected session is doing: its state line, then its screen.
fn detail<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let Some(record) = app.current() else {
        frame.render_widget(Block::new().borders(Borders::ALL), area);
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(record.state.to_string(), state_style(record.state)),
        Span::raw(format!(
            "  {} {}  {}",
            record.harness,
            record.mode,
            record.environment.cwd.display()
        )),
    ])];
    if let Some(reason) = &record.reason {
        lines.push(Line::raw(format!("reason: {reason}")));
    }
    if let Some(restore) = record.restores.last() {
        lines.push(Line::raw(restore.line.clone()));
    }
    if record.state == State::Held {
        let resume = app.keys.list_keys(ListAction::Resume);
        let attach = app.keys.list_keys(ListAction::Attach);
        lines.push(Line::styled(
            format!(
                "held: {resume} resumes it with a prompt; {attach} attaches, where RET runs it again"
            ),
            Style::new().fg(Color::Yellow),
        ));
    }
    let header = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    match &app.preview {
        Ok(screen) => {
            // The bottom of the screen, as much as fits.
            let room = usize::from(area.height.saturating_sub(header + 2));
            let text: Vec<&String> = screen.iter().collect();
            let end = text
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(0, |last| last + 1);
            let start = end.saturating_sub(room);
            lines.extend(
                text.get(start..end)
                    .unwrap_or_default()
                    .iter()
                    .map(|line| Line::raw((*line).clone())),
            );
        }
        Err(error) => lines.push(Line::styled(
            error.clone(),
            Style::new().add_modifier(Modifier::DIM),
        )),
    }
    let title = format!(" {} · {} ", record.id, who(record));
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(title)),
        area,
    );
}

/// The selected session's screen, scrolled.
fn view<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let title = app.current().map_or_else(String::new, |record| {
        format!(" {} · {} · {} ", record.id, who(record), record.state)
    });
    let block = Block::new().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    let lines: Vec<Line<'static>> = match &app.preview {
        Ok(screen) => screen.iter().map(|line| Line::raw(line.clone())).collect(),
        Err(error) => vec![Line::raw(error.clone())],
    };
    let most = lines.len().saturating_sub(usize::from(inner.height));
    let scroll = u16::try_from(app.scroll.min(most)).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// A rectangle `width` by `height` in the middle of `area`, clipped.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// A section's title, then one row per key and what it does, the keys
/// padded to one width.
fn section(title: &str, rows: Vec<(String, &str)>) -> Vec<Line<'static>> {
    let width = rows.iter().map(|(keys, _)| keys.len()).max().unwrap_or(0);
    let mut lines = vec![Line::styled(
        title.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        rows.into_iter()
            .map(|(keys, what)| Line::raw(format!(" {keys:<width$}  {what}"))),
    );
    lines
}

fn help<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let keys = &app.keys;
    let left = section(
        "sessions [list]",
        keys.list
            .help_rows()
            .into_iter()
            .map(|(action, bound)| (bound, action.description()))
            .collect(),
    );
    let mut right = section(
        "a session's screen [attach]",
        keys.attach
            .help_rows()
            .into_iter()
            .map(|(action, bound)| (bound, action.description()))
            .collect(),
    );
    let (finish, cancel) = keys.prompt_keys();
    let field = |action| keys.form.keys_for(action).join(", ");
    right.push(Line::raw(""));
    right.extend(section(
        "prompts and the start form [editor, form]",
        vec![
            (finish, "start, resume, send"),
            (cancel, "cancel"),
            (field(super::keys::FormAction::NextField), "next field"),
            (
                field(super::keys::FormAction::PreviousField),
                "previous field",
            ),
        ],
    ));
    right.push(Line::raw(""));
    right.push(Line::styled(
        "~/.config/kersh/keys.yml rebinds any key.",
        Style::new().add_modifier(Modifier::DIM),
    ));
    right.push(Line::styled(
        "Any key closes this help.",
        Style::new().add_modifier(Modifier::DIM),
    ));
    let height = u16::try_from(left.len().max(right.len()) + 2).unwrap_or(u16::MAX);
    let box_area = centered(area, 96, height);
    frame.render_widget(Clear, box_area);
    let block = Block::new().borders(Borders::ALL).title(" keys ");
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
}

fn start_form<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let Mode::Start(form) = &app.mode else {
        return;
    };
    let box_area = centered(area, 80, 22);
    frame.render_widget(Clear, box_area);
    let (finish, cancel) = app.keys.prompt_keys();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(" start a session ")
        .title_bottom(format!(" {finish} start  {cancel} cancel "));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);
    let fields = Field::ALL.len() - 1;
    let mut constraints = vec![Constraint::Length(1); fields];
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Min(3));
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(inner);
    for (index, field) in Field::ALL.iter().enumerate() {
        let Some(editor) = form.editors.get(index) else {
            continue;
        };
        let focused = index == form.focus;
        let marker = if focused { ">" } else { " " };
        let label = Style::new().add_modifier(if focused {
            Modifier::BOLD
        } else {
            Modifier::DIM
        });
        if *field == Field::Prompt {
            let Some(&row) = rows.get(fields + 1) else {
                continue;
            };
            let Some(&label_row) = rows.get(fields) else {
                continue;
            };
            frame.render_widget(
                Paragraph::new(Line::styled(format!("{marker} {}", field.label()), label)),
                label_row,
            );
            frame.render_widget(editor, row);
        } else {
            let Some(&row) = rows.get(index) else {
                continue;
            };
            let [name, value] =
                Layout::horizontal([Constraint::Length(10), Constraint::Min(1)]).areas(row);
            frame.render_widget(
                Paragraph::new(Line::styled(format!("{marker} {}", field.label()), label)),
                name,
            );
            frame.render_widget(editor, value);
        }
    }
    // A single-line field draws no echo line of its own.
    if let (Some(editor), Some(&echo)) = (form.editors.get(form.focus), rows.last())
        && Field::ALL.get(form.focus) != Some(&Field::Prompt)
        && let Some(text) = editor.echo()
    {
        frame.render_widget(Paragraph::new(text), echo);
    }
}

fn prompt_box<C: Calls>(app: &App<C>, frame: &mut Frame<'_>, area: Rect) {
    let Mode::Prompt(prompt) = &app.mode else {
        return;
    };
    let title = match &prompt.kind {
        PromptKind::Resume(id) => format!(" resume {id} "),
        PromptKind::Send(id) => format!(" send to {id} "),
    };
    let (finish, cancel) = app.keys.prompt_keys();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(format!(" {finish} send  {cancel} cancel "));
    let box_area = centered(area, 72, 12);
    frame.render_widget(Clear, box_area);
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);
    frame.render_widget(&prompt.editor, inner);
}
