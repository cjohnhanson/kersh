//! The session commands. Each calls one [`Client`] method and prints its
//! result: a table or text by default, JSON lines with `--json`. Exit 0
//! on success, 1 on a failure, 2 on a usage error, 3 at a group's limit.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde::Serialize;

use crate::client::Client;
use crate::paths::Paths;
use crate::session::{Harness, Mode, PaneSize, Record, SessionError, StartRequest, State};

/// `kersh start`'s arguments, before the prompt file is read.
#[derive(Debug)]
pub struct StartInput {
    pub agent: Option<String>,
    pub runtime: Option<String>,
    pub harness: Option<Harness>,
    pub mode: Option<Mode>,
    pub model: Option<String>,
    pub skill: Option<String>,
    pub profile: Option<String>,
    pub instructions_file: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub tempdir: bool,
    pub labels: Vec<String>,
    pub group: Option<String>,
    pub max_running: Option<u32>,
    pub name: Option<String>,
    pub prompt_file: Option<PathBuf>,
    pub resume_conversation: Option<String>,
    pub harness_flags: Vec<String>,
}

/// A session command, parsed.
#[derive(Debug)]
pub enum SessionCmd {
    Ps {
        ids: Vec<String>,
        state: Option<State>,
        follow: bool,
        json: bool,
    },
    Start {
        request: Box<StartInput>,
        json: bool,
    },
    Attach {
        id: String,
    },
    Stop {
        id: String,
        json: bool,
    },
    Resume {
        id: String,
        prompt_file: Option<PathBuf>,
        json: bool,
    },
    Restart {
        id: String,
        json: bool,
    },
    RestartMux {
        json: bool,
    },
    Send {
        id: String,
        prompt_file: PathBuf,
    },
    Events {
        id: String,
        since: u64,
        follow: bool,
        text: bool,
    },
    Screen {
        id: String,
        follow: bool,
        json: bool,
    },
    Rm {
        id: String,
        json: bool,
    },
    HarnessList {
        json: bool,
    },
    AgentAdd {
        file: PathBuf,
        force: bool,
    },
    RuntimeAdd {
        name: String,
        file: PathBuf,
    },
    RuntimeList {
        json: bool,
    },
    RuntimeShow {
        name: String,
        json: bool,
    },
    Serve,
    #[cfg(feature = "tui")]
    Tui,
    Setup {
        harness: Harness,
        json: bool,
    },
    Memories {
        root: Option<PathBuf>,
    },
    Doctor {
        json: bool,
    },
    ServiceInstall {
        json: bool,
    },
    ServiceStop {
        json: bool,
    },
    Host {
        state: Option<PathBuf>,
    },
    Mux {
        state: Option<PathBuf>,
    },
    WrapperRun {
        state: Option<PathBuf>,
        id: String,
    },
    SetsidExec {
        argv: Vec<String>,
    },
    EnvDump,
}

fn paths(state: Option<PathBuf>) -> Paths {
    let mut paths = Paths::discover();
    if let Some(state) = state {
        paths.state = state;
    }
    paths
}

fn client() -> Client {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Client::discover(&cwd)
}

fn report(error: &SessionError) -> ExitCode {
    eprintln!("kersh: {error}");
    ExitCode::from(error.exit_code())
}

fn fail(message: &str) -> ExitCode {
    eprintln!("kersh: {message}");
    ExitCode::FAILURE
}

/// Print a `kersh service` result: its detail line, or its JSON.
fn service_result(
    result: Result<crate::service::ServiceStatus, SessionError>,
    json: bool,
) -> ExitCode {
    match result {
        Ok(status) => {
            if json {
                json_line(&status);
            } else {
                println!("{}", status.detail);
            }
            ExitCode::SUCCESS
        }
        Err(error) => report(&error),
    }
}

/// Print `value` as one JSON line; false when stdout is gone.
fn json_line(value: &impl Serialize) -> bool {
    let Ok(line) = serde_json::to_string(value) else {
        return false;
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
}

/// Read a prompt file, `-` meaning standard input.
fn read_prompt(path: &Path) -> Result<String, SessionError> {
    if path == Path::new("-") {
        use std::io::Read as _;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| SessionError::usage(format!("cannot read standard input: {error}")))?;
        return Ok(text);
    }
    std::fs::read_to_string(path)
        .map_err(|error| SessionError::usage(format!("cannot read `{}`: {error}", path.display())))
}

fn parse_labels(labels: &[String]) -> Result<BTreeMap<String, String>, SessionError> {
    let mut map = BTreeMap::new();
    for label in labels {
        let (key, value) = label.split_once('=').ok_or_else(|| {
            SessionError::usage(format!("`{label}` is not a label; use --label KEY=VALUE"))
        })?;
        if key.is_empty() {
            return Err(SessionError::usage(format!(
                "`{label}` has no key; use --label KEY=VALUE"
            )));
        }
        map.insert(key.to_owned(), value.to_owned());
    }
    Ok(map)
}

/// Indent each line by two spaces, as a runtime sits in runtimes.yml.
fn indent(text: &str) -> String {
    text.lines().fold(String::new(), |mut out, line| {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
        out
    })
}

fn print_record(record: &Record, json: bool) {
    if json {
        json_line(record);
    } else {
        println!("{}", record.id);
    }
}

/// One `ps` table row.
fn row(record: &Record) -> String {
    let who = record
        .name
        .clone()
        .or_else(|| record.agent.as_ref().map(|agent| agent.name.clone()))
        .unwrap_or_else(|| "-".to_owned());
    let detail = record
        .progress
        .clone()
        .or_else(|| record.reason.clone())
        .unwrap_or_default();
    format!(
        "{:<12}  {:<7}  {:<11}  {:<11}  {:<10}  {:<12}  {}  {}",
        record.id,
        record.state,
        record.harness,
        record.mode,
        record.group.as_deref().unwrap_or("-"),
        who,
        record.environment.cwd.display(),
        detail
    )
}

const HEADER: &str =
    "ID            STATE    HARNESS      MODE         GROUP       NAME          DIR  PROGRESS";

/// Run a session command.
#[allow(clippy::too_many_lines)] // One arm per command.
pub fn dispatch(command: SessionCmd) -> ExitCode {
    match command {
        SessionCmd::Ps {
            ids,
            state,
            follow,
            json,
        } => ps(&ids, state, follow, json),
        SessionCmd::Start { request, json } => start(*request, json),
        SessionCmd::Attach { id } => client()
            .attach(&id)
            .map_or_else(|error| report(&error), |()| ExitCode::SUCCESS),
        SessionCmd::Stop { id, json } => match client().stop(&id) {
            Ok(record) => {
                print_record(&record, json);
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::Resume {
            id,
            prompt_file,
            json,
        } => {
            let prompt = match prompt_file.as_deref().map(read_prompt).transpose() {
                Ok(prompt) => prompt,
                Err(error) => return report(&error),
            };
            match client().resume(&id, prompt.as_deref()) {
                Ok(record) => {
                    print_record(&record, json);
                    ExitCode::SUCCESS
                }
                Err(error) => report(&error),
            }
        }
        SessionCmd::Restart { id, json } => match client().restart(&id) {
            Ok(record) => {
                print_record(&record, json);
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::RestartMux { json } => match client().restart_mux() {
            Ok(records) => {
                for record in &records {
                    print_record(record, json);
                }
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::Send { id, prompt_file } => {
            let text = match read_prompt(&prompt_file) {
                Ok(text) => text,
                Err(error) => return report(&error),
            };
            match client().send(&id, text.trim_end_matches('\n')) {
                Ok(()) => {
                    println!("sent to {id}");
                    ExitCode::SUCCESS
                }
                Err(error) => report(&error),
            }
        }
        SessionCmd::Events {
            id,
            since,
            follow,
            text,
        } => events(&id, since, follow, text),
        SessionCmd::Screen { id, follow, json } => screen(&id, follow, json),
        SessionCmd::Rm { id, json } => match client().rm(&id) {
            Ok(()) => {
                if json {
                    json_line(&serde_json::json!({ "id": id }));
                } else {
                    println!("{id}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::HarnessList { json } => match client().harness_list() {
            Ok(builds) => {
                for build in &builds {
                    if json {
                        json_line(build);
                    } else {
                        match (&build.path, &build.version, &build.error) {
                            (Some(path), Some(version), _) => {
                                println!("{}  {}  {version}", build.harness, path.display());
                            }
                            (_, _, Some(error)) => println!("{}  missing: {error}", build.harness),
                            _ => println!("{}  missing", build.harness),
                        }
                    }
                }
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::AgentAdd { file, force } => match client().agent_add(&file, force) {
            Ok(path) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::RuntimeAdd { name, file } => match client().runtime_add(&name, &file) {
            Ok(path) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::RuntimeList { json } => match client().runtime_list() {
            Ok(entries) => {
                for entry in &entries {
                    if json {
                        json_line(entry);
                    } else {
                        let harness = entry
                            .runtime
                            .harness
                            .map_or_else(|| "rig".to_owned(), |h| h.to_string());
                        let mode = entry
                            .runtime
                            .mode
                            .map_or_else(|| "-".to_owned(), |m| m.to_string());
                        println!("{}  {harness}  {mode}", entry.name);
                    }
                }
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::RuntimeShow { name, json } => match client().runtime_show(&name) {
            Ok(entry) => {
                if json {
                    json_line(&entry);
                } else {
                    match serde_yaml_ng::to_string(&entry.runtime) {
                        Ok(yaml) => print!("{}:\n{}", entry.name, indent(&yaml)),
                        Err(error) => return fail(&error.to_string()),
                    }
                }
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::Serve => {
            crate::serve::run().map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS)
        }
        #[cfg(feature = "tui")]
        SessionCmd::Tui => {
            crate::tui::run().map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS)
        }
        SessionCmd::Setup { harness, json } => match client().setup(harness) {
            Ok(report) => {
                for item in &report.items {
                    if json {
                        json_line(item);
                    } else {
                        let mark = if item.present { "present" } else { "missing" };
                        println!("{mark:<8} {}: {}", item.piece, item.detail);
                    }
                }
                if report.ok() {
                    ExitCode::SUCCESS
                } else {
                    if !json {
                        eprintln!("kersh: {harness} is not fully wired; see the missing pieces");
                    }
                    ExitCode::FAILURE
                }
            }
            Err(error) => report(&error),
        },
        SessionCmd::Doctor { json } => match client().doctor() {
            Ok(report) => {
                for item in &report.items {
                    if json {
                        json_line(item);
                    } else {
                        let flag = if item.required && !item.state.passes() {
                            "  [required]"
                        } else {
                            ""
                        };
                        println!(
                            "{:<11} {:<8} {}: {}{flag}",
                            item.state.as_str(),
                            item.area,
                            item.check,
                            item.detail
                        );
                    }
                }
                if report.ok() {
                    ExitCode::SUCCESS
                } else {
                    if !json {
                        eprintln!(
                            "kersh: a required piece is missing; see the lines marked required"
                        );
                    }
                    ExitCode::FAILURE
                }
            }
            Err(error) => report(&error),
        },
        SessionCmd::ServiceInstall { json } => service_result(client().service_install(), json),
        SessionCmd::ServiceStop { json } => service_result(client().service_stop(), json),
        SessionCmd::Memories { root } => match client().memories(root.as_deref()) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => report(&error),
        },
        SessionCmd::Host { state } => {
            crate::host::run(paths(state)).map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS)
        }
        SessionCmd::Mux { state } => crate::mux::serve(&paths(state), crate::service::entry())
            .map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS),
        SessionCmd::WrapperRun { state, id } => crate::wrapper::run(&paths(state), &id),
        SessionCmd::SetsidExec { argv } => crate::wrapper::setsid_exec(&argv),
        SessionCmd::EnvDump => crate::login_env::dump(),
    }
}

fn ps(ids: &[String], state: Option<State>, follow: bool, json: bool) -> ExitCode {
    let client = client();
    if follow {
        let stream = match client.ps_follow() {
            Ok(stream) => stream,
            Err(error) => return report(&error),
        };
        if !json {
            println!("{HEADER}");
        }
        for record in stream {
            let record = match record {
                Ok(record) => record,
                Err(error) => return report(&error),
            };
            if !ids.is_empty() && !ids.contains(&record.id) {
                continue;
            }
            if state.is_some_and(|state| record.state != state) {
                continue;
            }
            let shown = if json {
                json_line(&record)
            } else {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", row(&record))
                    .and_then(|()| out.flush())
                    .is_ok()
            };
            if !shown {
                return ExitCode::SUCCESS;
            }
        }
        return ExitCode::SUCCESS;
    }
    let records = match client.ps(ids, state) {
        Ok(records) => records,
        Err(error) => return report(&error),
    };
    if json {
        for record in &records {
            json_line(record);
        }
        return ExitCode::SUCCESS;
    }
    if records.is_empty() {
        match state {
            Some(state) => println!("no sessions in state {state}."),
            None => println!("no sessions. `kersh start` starts one."),
        }
        return ExitCode::SUCCESS;
    }
    println!("{HEADER}");
    for record in &records {
        println!("{}", row(record));
    }
    ExitCode::SUCCESS
}

fn start(input: StartInput, json: bool) -> ExitCode {
    let labels = match parse_labels(&input.labels) {
        Ok(labels) => labels,
        Err(error) => return report(&error),
    };
    let prompt = match input.prompt_file.as_deref().map(read_prompt).transpose() {
        Ok(prompt) => prompt,
        Err(error) => return report(&error),
    };
    let size = crate::mux::terminal_size().map(|size| PaneSize {
        cols: size.cols,
        rows: size.rows,
    });
    let request = StartRequest {
        agent: input.agent,
        runtime: input.runtime,
        harness: input.harness,
        mode: input.mode,
        model: input.model,
        skill: input.skill,
        profile: input.profile,
        instructions_file: input.instructions_file,
        cwd: input.cwd,
        tempdir: input.tempdir,
        labels,
        group: input.group,
        max_running: input.max_running,
        name: input.name,
        prompt,
        resume_conversation: input.resume_conversation,
        harness_flags: input.harness_flags,
        size,
    };
    match client().start(&request) {
        Ok(record) => {
            print_record(&record, json);
            ExitCode::SUCCESS
        }
        Err(error) => report(&error),
    }
}

fn events(id: &str, since: u64, follow: bool, text: bool) -> ExitCode {
    let client = client();
    let mut since = since;
    loop {
        let events = match client.events(id, since) {
            Ok(events) => events,
            Err(error) => return report(&error),
        };
        for event in &events {
            since = since.max(event.seq);
            let shown = if text {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", event.summary)
                    .and_then(|()| out.flush())
                    .is_ok()
            } else {
                json_line(event)
            };
            if !shown {
                return ExitCode::SUCCESS;
            }
        }
        if !follow {
            return ExitCode::SUCCESS;
        }
        let running = client
            .ps(&[id.to_owned()], None)
            .ok()
            .and_then(|records| records.first().map(|record| record.state == State::Running))
            .unwrap_or(false);
        if !running && events.is_empty() {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn screen(id: &str, follow: bool, json: bool) -> ExitCode {
    let client = client();
    let mut last: Option<Vec<String>> = None;
    loop {
        let screen = match client.screen(id) {
            Ok(screen) => screen,
            Err(error) if follow && last.is_some() => {
                let _ = error;
                return ExitCode::SUCCESS;
            }
            Err(error) => return report(&error),
        };
        if last.as_ref() != Some(&screen.lines) {
            let shown = if json {
                json_line(&screen)
            } else {
                let mut out = std::io::stdout().lock();
                let text = screen.lines.join("\n");
                let separator = if last.is_some() { "\u{c}\n" } else { "" };
                writeln!(out, "{separator}{text}")
                    .and_then(|()| out.flush())
                    .is_ok()
            };
            if !shown {
                return ExitCode::SUCCESS;
            }
            last = Some(screen.lines);
        }
        if !follow {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn labels_keep_their_values_untouched() {
        let labels = parse_labels(&[
            "card=a b=c".to_owned(),
            "note= spaced ".to_owned(),
            "u=ünï".to_owned(),
        ])
        .unwrap();
        assert_eq!(labels.get("card").map(String::as_str), Some("a b=c"));
        assert_eq!(labels.get("note").map(String::as_str), Some(" spaced "));
        assert_eq!(labels.get("u").map(String::as_str), Some("ünï"));
        assert!(parse_labels(&["=x".to_owned()]).is_err());
        assert!(parse_labels(&["nokey".to_owned()]).is_err());
    }
}
