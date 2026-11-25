//! The command surface. `main` in the binary is a thin shim, so the whole
//! surface is testable in process.
//!
//! Results go to standard output and errors to standard error. The exit
//! code is 0 on success, 1 on a failure, and 2 on a usage error.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::client::{Client, ListEntry};
use crate::session::{Harness, Mode, State};

const PRIME: &str = "\
# kersh
Run a declarative agent. An agent is `agents/<name>/AGENT.md`: frontmatter
names a model, a skill, and a gaff profile; the body is the system prompt.
The profile carries the agent's context, guards, and stop rule.
Commands:
  kersh init [name]
  kersh list [--json]
  kersh show <name> [--json]
  kersh check [--json]
  kersh render <name> [--context-file <path|->] [--json] [prompt]
  kersh run <name> [--context-file <path|->] [--root <dir>] [--output jsonl] [prompt]
  kersh docs
  kersh --version
Agent-file commands take --root <dir>. --json prints one JSON object per line.
Sessions (rig, Claude Code, and Codex, hosted in a terminal session):
  kersh start [agent] [--runtime R] [--harness H] [--mode M] [--prompt-file <path|->]
  kersh ps [--follow] [--json]
  kersh attach <id>          (C-] leaves it running)
  kersh events <id> [--since N] [--follow]
  kersh screen <id>
  kersh send <id> --prompt-file <path|->
  kersh stop|resume|restart|rm <id>
  kersh stop $KERSH_SESSION  (an agent ends its own session when its task is done)
";

/// The primer's last lines: the front ends this build has.
#[cfg(feature = "tui")]
const FRONT_ENDS: &str = "\
Front ends over the same calls: kersh serve (MCP on stdio), kersh tui, and REST
on the host's socket, host.sock (GET /openapi.json describes it).
Machine:
  kersh doctor [--json]        (what is installed, wired, and guarded)
  kersh service install|stop   (the kersh-host unit that starts the host at login)
";

#[cfg(not(feature = "tui"))]
const FRONT_ENDS: &str = "\
Front ends over the same calls: kersh serve (MCP on stdio), and REST on the
host's socket, host.sock (GET /openapi.json describes it).
";

const DOCS: &str = include_str!("../docs/kersh.md");

/// The program name a multi-call binary passes as the first word.
const NAME: &str = "kersh";

#[derive(Debug, Parser)]
#[command(
    name = "kersh",
    version,
    about = "Run declarative agents. An agent file names a model, a skill, and a gaff profile."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a kersh root and a starter agent. Existing files are kept.
    Init {
        /// The agent to create.
        #[arg(default_value = "reviewer")]
        name: String,
        /// Create the root in this directory instead of the current one.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
    /// List every agent and its description.
    List {
        #[command(flatten)]
        root: RootArg,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Print an agent's settings and system prompt.
    Show {
        /// The agent to show.
        name: String,
        #[command(flatten)]
        root: RootArg,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Check that every agent file loads. Exits 1 when one does not.
    Check {
        #[command(flatten)]
        root: RootArg,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Print the prompt a run would send, without calling the model.
    Render {
        /// The agent to render.
        name: String,
        /// Read context from this file, or from standard input for `-`.
        #[arg(long, value_name = "PATH|-")]
        context_file: Option<PathBuf>,
        #[command(flatten)]
        root: RootArg,
        #[command(flatten)]
        json: JsonArg,
        /// The prompt. Put `--` before a prompt that starts with a dash.
        prompt: Option<String>,
    },
    /// Run an agent and print its final answer.
    Run(Box<RunArgs>),
    /// Print the kersh manual.
    Docs,
    /// Print a short primer for an agent's context.
    Prime,
    /// List sessions. --follow streams each record change.
    Ps {
        /// Only these sessions.
        ids: Vec<String>,
        /// Only sessions in this state: running, held, ended, or lost.
        #[arg(long, value_name = "STATE")]
        state: Option<State>,
        /// Print every record, then each change, until interrupted.
        #[arg(long)]
        follow: bool,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Start a session. Prints its id; exits 3 at the group's limit.
    Start(Box<StartArgs>),
    /// Drop into a session's terminal. C-] leaves it running.
    Attach {
        /// The session.
        id: String,
    },
    /// End a session and close its pane.
    Stop {
        /// The session.
        id: String,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Continue a held or ended session's conversation in a new pane.
    Resume {
        /// The session.
        id: String,
        /// The prompt, from this file or standard input for `-`. A
        /// headless session needs one.
        #[arg(long, value_name = "PATH|-")]
        prompt_file: Option<PathBuf>,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Restart an interactive session on its conversation, or the session
    /// server with --mux.
    Restart {
        /// The session.
        #[arg(required_unless_present = "mux", conflicts_with = "mux")]
        id: Option<String>,
        /// Stop the session server, start a new one, and restore every
        /// session, as after an upgrade that changes its wire version.
        #[arg(long)]
        mux: bool,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Type text into a running interactive session, then Enter.
    Send {
        /// The session.
        id: String,
        /// The text, from this file or standard input for `-`.
        #[arg(long, value_name = "PATH|-", default_value = "-")]
        prompt_file: PathBuf,
    },
    /// Print a headless session's events as JSON lines.
    Events {
        /// The session.
        id: String,
        /// Only events after this sequence number.
        #[arg(long, value_name = "N", default_value_t = 0)]
        since: u64,
        /// Keep printing new events until the session stops running.
        #[arg(long)]
        follow: bool,
        /// Print each event's summary line instead of JSON.
        #[arg(long)]
        text: bool,
    },
    /// Print a session's screen as plain text.
    Screen {
        /// The session.
        id: String,
        /// Print the screen again each time it changes.
        #[arg(long)]
        follow: bool,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Delete an ended session's record, events, and temp dir.
    Rm {
        /// The session.
        id: String,
        #[command(flatten)]
        json: JsonArg,
    },
    /// The harness builds kersh would run.
    Harness {
        #[command(subcommand)]
        command: HarnessCommand,
    },
    /// Write agent files into the user-level store.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Write, list, and show runtimes in runtimes.yml.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Run gaff's registration for a harness and check its wiring.
    /// Exits 1 when a piece is missing.
    Setup {
        /// claude-code or codex.
        harness: Harness,
        #[command(flatten)]
        json: JsonArg,
    },
    /// Check this machine for kersh. Exits 1 when a required piece is
    /// missing.
    ///
    /// Reports the harnesses, the host and session server, the kersh-host
    /// unit, gaff and its hooks, the guard for each harness and mode, and
    /// the eng.rip tools, against the login environment. Required: kersh
    /// and gaff on the login PATH, gaff's hooks for each harness found,
    /// and a running host or session server of this kersh's version.
    Doctor {
        #[command(flatten)]
        json: JsonArg,
    },
    /// Install or stop the kersh-host unit, which starts the host at
    /// login.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Print the agent-memory block for a gaff section.
    Memories {
        /// The notes store; the configured memory.root by default.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
    /// Serve the session and agent commands over MCP on stdio.
    Serve,
    /// Browse and drive sessions in a terminal UI.
    #[cfg(feature = "tui")]
    Tui,
    /// Run the session host in the foreground.
    Host {
        #[arg(long, value_name = "DIR", hide = true)]
        state: Option<PathBuf>,
    },
    /// The session server role.
    #[command(hide = true)]
    Mux {
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
    },
    /// The wrapper roles inside a session pane.
    #[command(hide = true)]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Print the environment, NUL-framed, for the login capture.
    #[command(hide = true)]
    EnvDump,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The agent to run.
    name: String,
    /// Read context from this file, or from standard input for `-`.
    #[arg(long, value_name = "PATH|-")]
    context_file: Option<PathBuf>,
    /// Read the prompt from this file, or from standard input for `-`.
    #[arg(long, value_name = "PATH|-", conflicts_with = "prompt")]
    prompt_file: Option<PathBuf>,
    /// Use this kersh root, and run the agent's commands in it.
    /// The default is the current directory.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
    /// `text` prints the final answer only. `jsonl` prints one JSON event
    /// per line: `init` with the agent and model, then `turn` for each
    /// model turn, `text` for its text, `tool` for each tool call,
    /// `tool_result` with its status (`success`, `error`, or `refused`),
    /// and `result` with the final text. `result` is always last; a run
    /// refused before it starts writes only `result`.
    #[arg(long, value_name = "FORMAT", value_enum, default_value_t = RunOutput::Text)]
    output: RunOutput,
    /// The agent file a session runs, instead of a store lookup.
    #[arg(long, value_name = "PATH", hide = true)]
    agent_file: Option<PathBuf>,
    /// The system prompt a session composed, instead of the agent's
    /// body and skill.
    #[arg(long, value_name = "PATH", hide = true)]
    system_file: Option<PathBuf>,
    /// The gaff profile a session resolved, over the agent's own.
    #[arg(long, value_name = "PROFILE", hide = true)]
    profile: Option<String>,
    /// The prompt. Put `--` before a prompt that starts with a dash.
    prompt: Option<String>,
}

/// What `kersh run` prints on standard output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RunOutput {
    Text,
    Jsonl,
}

#[derive(Debug, Args)]
struct StartArgs {
    /// The agent file to start. Without one, --harness or --runtime is
    /// required.
    agent: Option<String>,
    /// A runtime from runtimes.yml, over the agent's own.
    #[arg(long, value_name = "NAME")]
    runtime: Option<String>,
    /// rig, claude-code, or codex.
    #[arg(long, value_name = "HARNESS")]
    harness: Option<Harness>,
    /// headless or interactive.
    #[arg(long, value_name = "MODE")]
    mode: Option<Mode>,
    /// The harness's model.
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
    /// Replace the agent's almanac skill.
    #[arg(long, value_name = "SKILL")]
    skill: Option<String>,
    /// The gaff profile.
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,
    /// Add this file's text to the agent's instructions.
    #[arg(long, value_name = "PATH")]
    instructions_file: Option<PathBuf>,
    /// The working directory; the current one by default.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Give the session its own temp dir, kept with its record.
    #[arg(long)]
    tempdir: bool,
    /// A label kersh stores and echoes, never reads. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
    /// The session's group, for --max-running.
    #[arg(long, value_name = "GROUP")]
    group: Option<String>,
    /// Refuse the start, exit 3, when the group already has this many
    /// sessions running.
    #[arg(long, value_name = "N", requires = "group")]
    max_running: Option<u32>,
    /// A name for the session.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    /// The first prompt, from this file or standard input for `-`. A
    /// headless start needs one.
    #[arg(long, value_name = "PATH|-")]
    prompt_file: Option<PathBuf>,
    /// Resume this harness conversation instead of starting a new one.
    #[arg(long, value_name = "ID")]
    resume_conversation: Option<String>,
    #[command(flatten)]
    json: JsonArg,
    /// Flags for the harness, after `--`, checked against its allow-list.
    #[arg(last = true, value_name = "HARNESS FLAGS")]
    harness_flags: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum HarnessCommand {
    /// The path and version of each harness on the login PATH.
    List {
        #[command(flatten)]
        json: JsonArg,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Write the kersh-host unit and load it, so the host runs at login.
    ///
    /// The unit is a launchd agent with `service_manager: launchd` and a
    /// systemd user unit with `service_manager: systemd`. The host runs
    /// now and at every login, and restores lost sessions when it starts.
    /// Running it again with the same unit changes nothing.
    Install {
        #[command(flatten)]
        json: JsonArg,
    },
    /// Stop the kersh-host unit and remove it. Sessions keep running.
    ///
    /// The unit is unloaded and its file removed, so the host no longer
    /// starts at login. The session server and its agents keep running.
    Stop {
        #[command(flatten)]
        json: JsonArg,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Copy an agent file into ~/.config/kersh/agents/<name>/AGENT.md.
    Add {
        /// The agent file.
        file: PathBuf,
        /// Replace an existing file that differs.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Add or replace a runtime from a YAML file.
    Add {
        /// The runtime's name.
        name: String,
        /// A YAML file with the runtime's fields.
        file: PathBuf,
    },
    /// List every runtime.
    List {
        #[command(flatten)]
        json: JsonArg,
    },
    /// Print one runtime's fields.
    Show {
        /// The runtime's name.
        name: String,
        #[command(flatten)]
        json: JsonArg,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Run a session's harness inside its pane.
    Run {
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
        id: String,
    },
    /// Run a program in a new session with no controlling terminal.
    SetsidExec {
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
}

#[derive(Debug, Args)]
struct RootArg {
    /// Use this kersh root instead of the nearest one above the current
    /// directory.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct JsonArg {
    /// Print JSON, one object per line.
    #[arg(long)]
    json: bool,
}

/// Run the CLI on `args`, which exclude the program name. One leading
/// `kersh` word is dropped, so a multi-call binary or a link named
/// `kersh` dispatches the same commands. Returns the exit code.
#[must_use]
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|first| first == NAME) {
        args.next();
    }
    let cli = match Cli::try_parse_from(std::iter::once(OsString::from(NAME)).chain(args)) {
        Ok(cli) => cli,
        Err(error) => {
            // clap sends help and the version to stdout with exit 0, and a
            // usage error to stderr with exit 2.
            let _ = error.print();
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };
    dispatch(cli.command)
}

fn dispatch(command: Command) -> ExitCode {
    match command {
        Command::Init { name, root } => init(&name, root),
        Command::List { root, json } => with_client(root.root.as_deref(), |c| list(c, json.json)),
        Command::Show { name, root, json } => {
            with_client(root.root.as_deref(), |c| show(c, &name, json.json))
        }
        Command::Check { root, json } => with_client(root.root.as_deref(), |c| check(c, json.json)),
        Command::Render {
            name,
            context_file,
            root,
            json,
            prompt,
        } => with_client(root.root.as_deref(), |c| {
            render(
                c,
                &name,
                context_file.as_deref(),
                prompt.as_deref().unwrap_or_default(),
                json.json,
            )
        }),
        Command::Run(args) => run_agent(&args),
        Command::Docs => {
            print!("{DOCS}");
            ExitCode::SUCCESS
        }
        Command::Prime => {
            print!("{PRIME}{FRONT_ENDS}");
            ExitCode::SUCCESS
        }
        command => command_into(command).map_or(ExitCode::FAILURE, crate::cli_sessions::dispatch),
    }
}

/// A session command, for its own module; `None` for the others.
#[allow(clippy::too_many_lines)] // One arm per command.
fn command_into(command: Command) -> Option<crate::cli_sessions::SessionCmd> {
    use crate::cli_sessions::SessionCmd as S;
    Some(match command {
        Command::Ps {
            ids,
            state,
            follow,
            json,
        } => S::Ps {
            ids,
            state,
            follow,
            json: json.json,
        },
        Command::Start(args) => {
            let args = *args;
            S::Start {
                request: Box::new(crate::cli_sessions::StartInput {
                    agent: args.agent,
                    runtime: args.runtime,
                    harness: args.harness,
                    mode: args.mode,
                    model: args.model,
                    skill: args.skill,
                    profile: args.profile,
                    instructions_file: args.instructions_file,
                    cwd: args.cwd,
                    tempdir: args.tempdir,
                    labels: args.labels,
                    group: args.group,
                    max_running: args.max_running,
                    name: args.name,
                    prompt_file: args.prompt_file,
                    resume_conversation: args.resume_conversation,
                    harness_flags: args.harness_flags,
                }),
                json: args.json.json,
            }
        }
        Command::Attach { id } => S::Attach { id },
        Command::Stop { id, json } => S::Stop {
            id,
            json: json.json,
        },
        Command::Resume {
            id,
            prompt_file,
            json,
        } => S::Resume {
            id,
            prompt_file,
            json: json.json,
        },
        Command::Restart { id, mux, json } => match (id, mux) {
            (Some(id), false) => S::Restart {
                id,
                json: json.json,
            },
            _ => S::RestartMux { json: json.json },
        },
        Command::Send { id, prompt_file } => S::Send { id, prompt_file },
        Command::Events {
            id,
            since,
            follow,
            text,
        } => S::Events {
            id,
            since,
            follow,
            text,
        },
        Command::Screen { id, follow, json } => S::Screen {
            id,
            follow,
            json: json.json,
        },
        Command::Rm { id, json } => S::Rm {
            id,
            json: json.json,
        },
        Command::Harness {
            command: HarnessCommand::List { json },
        } => S::HarnessList { json: json.json },
        Command::Agent {
            command: AgentCommand::Add { file, force },
        } => S::AgentAdd { file, force },
        Command::Runtime { command } => match command {
            RuntimeCommand::Add { name, file } => S::RuntimeAdd { name, file },
            RuntimeCommand::List { json } => S::RuntimeList { json: json.json },
            RuntimeCommand::Show { name, json } => S::RuntimeShow {
                name,
                json: json.json,
            },
        },
        Command::Serve => S::Serve,
        #[cfg(feature = "tui")]
        Command::Tui => S::Tui,
        Command::Setup { harness, json } => S::Setup {
            harness,
            json: json.json,
        },
        Command::Doctor { json } => S::Doctor { json: json.json },
        Command::Service { command } => match command {
            ServiceCommand::Install { json } => S::ServiceInstall { json: json.json },
            ServiceCommand::Stop { json } => S::ServiceStop { json: json.json },
        },
        Command::Memories { root } => S::Memories { root },
        Command::Host { state } => S::Host { state },
        Command::Mux { state } => S::Mux { state },
        Command::Session {
            command: SessionCommand::Run { state, id },
        } => S::WrapperRun { state, id },
        Command::Session {
            command: SessionCommand::SetsidExec { argv },
        } => S::SetsidExec { argv },
        Command::EnvDump => S::EnvDump,
        Command::Init { .. }
        | Command::List { .. }
        | Command::Show { .. }
        | Command::Check { .. }
        | Command::Render { .. }
        | Command::Run(_)
        | Command::Docs
        | Command::Prime => return None,
    })
}

/// `kersh init [name]` — scaffold a kersh root.
///
/// Write `kersh.yml` when it is absent, then `agents/<name>/AGENT.md` from
/// a template when it is absent. Never overwrite a file, so a re-run is
/// safe.
fn init(name: &str, root: Option<PathBuf>) -> ExitCode {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return fail(&format!(
            "`{name}` is not a valid agent name; use a single directory name"
        ));
    }
    let root = root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut created = Vec::new();
    let mut present = Vec::new();

    let marker = root.join("kersh.yml");
    if marker.exists() {
        present.push("kersh.yml".to_owned());
    } else if let Err(error) = std::fs::write(&marker, "") {
        return fail(&format!("cannot write kersh.yml: {error}"));
    } else {
        created.push("kersh.yml".to_owned());
    }

    let agent_dir = root.join("agents").join(name);
    let agent_file = agent_dir.join("AGENT.md");
    let rel = format!("agents/{name}/AGENT.md");
    if agent_file.exists() {
        present.push(rel);
    } else if let Err(error) = std::fs::create_dir_all(&agent_dir) {
        return fail(&format!("cannot create {}: {error}", agent_dir.display()));
    } else if let Err(error) = std::fs::write(&agent_file, agent_template(name)) {
        return fail(&format!("cannot write {rel}: {error}"));
    } else {
        created.push(rel);
    }

    for path in &created {
        println!("created {path}");
    }
    for path in &present {
        println!("present {path}");
    }
    if created.is_empty() {
        println!("nothing to do; the root and the agent are already here.");
    } else {
        println!("\nedit agents/{name}/AGENT.md, then run `kersh check`.");
    }
    ExitCode::SUCCESS
}

/// The starter agent file `init` writes. It is valid on its own, so
/// `kersh check` passes right after `kersh init`.
fn agent_template(name: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: One line, shown by `kersh list`.\n\
         model: claude-code/haiku\n\
         max_turns: 4\n\
         timeout: 120s\n\
         ---\n\
         State the agent's job here: what it does, what it reports, and\n\
         what it must not do. This body is the system prompt.\n\
         \n\
         The caller supplies the situation with `--context-file` or a pipe.\n\
         Use the bash tool to read files: cat, ls, rg, git diff, and so on.\n"
    )
}

/// The client over `--root`, or over the stores found from the current
/// directory. A `--root` without `kersh.yml` is an error.
fn client(root: Option<&Path>) -> Result<Client, String> {
    if let Some(root) = root {
        return Client::from_root(root.to_path_buf()).map_err(|error| error.to_string());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Ok(Client::discover(&cwd))
}

/// Run `command` over the client for `root`, or fail with why the root
/// does not open.
fn with_client(root: Option<&Path>, command: impl FnOnce(&Client) -> ExitCode) -> ExitCode {
    client(root).map_or_else(|error| fail(&error), |client| command(&client))
}

/// Print `value` as one JSON line.
fn print_json(value: &impl Serialize) -> Result<(), String> {
    let line =
        serde_json::to_string(value).map_err(|error| format!("cannot write the JSON: {error}"))?;
    println!("{line}");
    Ok(())
}

fn list(client: &Client, json: bool) -> ExitCode {
    let entries = client.list();
    if json {
        for entry in &entries {
            if let Err(error) = print_json(entry) {
                return fail(&error);
            }
        }
        return ExitCode::SUCCESS;
    }
    if entries.is_empty() {
        println!("no agents. Add agents/<name>/AGENT.md under a kersh.yml root.");
        return ExitCode::SUCCESS;
    }
    for entry in entries {
        match entry {
            ListEntry::Agent(agent) if !agent.description.is_empty() => {
                println!("{}  {}", agent.name, agent.description);
            }
            ListEntry::Agent(agent) => println!("{}", agent.name),
            ListEntry::Broken(broken) => println!("{}", broken.name),
        }
    }
    ExitCode::SUCCESS
}

fn show(client: &Client, name: &str, json: bool) -> ExitCode {
    let agent = match client.show(name) {
        Ok(agent) => agent,
        Err(error) => return fail(&error.to_string()),
    };
    if json {
        return print_json(&agent).map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS);
    }
    println!("name:    {}", agent.name);
    println!("model:   {}", agent.model);
    if let Some(skill) = &agent.skill {
        println!("skill:   {skill}");
    }
    if let Some(profile) = &agent.profile {
        println!("profile: {profile}");
    }
    println!("turns:   {}", agent.max_turns);
    println!(
        "timeout: {:?}",
        std::time::Duration::from_secs(agent.timeout_secs)
    );
    println!("---\n{}", agent.body);
    ExitCode::SUCCESS
}

/// One line of `kersh check --json`: a problem, or the closing summary.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CheckLine<'a> {
    Problem {
        name: &'a str,
        path: &'a Path,
        error: &'a str,
    },
    Summary {
        agents: usize,
        problems: usize,
        ok: bool,
        roots: &'a [PathBuf],
    },
}

fn check(client: &Client, json: bool) -> ExitCode {
    let report = client.check();
    let code = if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        let problems = report.problems.iter().map(|problem| CheckLine::Problem {
            name: &problem.name,
            path: &problem.path,
            error: &problem.error,
        });
        let summary = CheckLine::Summary {
            agents: report.agents,
            problems: report.problems.len(),
            ok: report.ok(),
            roots: &report.roots,
        };
        for line in problems.chain(std::iter::once(summary)) {
            if let Err(error) = print_json(&line) {
                return fail(&error);
            }
        }
        return code;
    }
    if report.ok() {
        println!("ok: {} agent(s)", report.agents);
    } else {
        for problem in &report.problems {
            eprintln!("kersh: {}", problem.error);
        }
    }
    code
}

fn render(
    client: &Client,
    name: &str,
    context_file: Option<&Path>,
    prompt: &str,
    json: bool,
) -> ExitCode {
    let context = match context_file.map(read_context).transpose() {
        Ok(context) => context,
        Err(error) => return fail(&error),
    };
    let rendered = match client.render(name, context.as_deref(), prompt) {
        Ok(rendered) => rendered,
        Err(error) => return fail(&error.to_string()),
    };
    if json {
        return print_json(&rendered).map_or_else(|error| fail(&error), |()| ExitCode::SUCCESS);
    }
    println!("=== system prompt ===\n{}\n", rendered.system);
    println!("=== first user turn ===\n{}", rendered.first_turn);
    ExitCode::SUCCESS
}

/// `kersh run <name> [--context-file <path|->] [--root <dir>] [prompt]`.
///
/// The agent runs through rig against the provider its `model` names.
/// Without `--output jsonl`, the final text goes to standard output and
/// nothing else does, because gaff reads a hook agent's verdict from that
/// stream. With it, each event is one JSON line and the result, with the
/// final text, is the last. The tools are confined to the current
/// directory, or `--root`.
fn run_agent(args: &RunArgs) -> ExitCode {
    let stream = (args.output == RunOutput::Jsonl).then(crate::jsonl::Stream::stdout);
    match run_inner(args, stream.as_ref()) {
        Ok(answer) => {
            match &stream {
                Some(stream) => stream.result(Ok(&answer)),
                None => println!("{answer}"),
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            if let Some(stream) = &stream {
                stream.result(Err(&message));
            }
            fail(&message)
        }
    }
}

/// Load the agent a run names: the session's file, or the stores' match.
fn load_run_agent(args: &RunArgs) -> Result<crate::agent::Agent, String> {
    let name = &args.name;
    let Some(path) = &args.agent_file else {
        let agent = client(args.root.as_deref())?
            .stores()
            .load(name)
            .map_err(|error| error.to_string())?;
        check_rig_harness(&agent)?;
        return Ok(agent);
    };
    // A session's settings already chose rig; its file is read as is.
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
    crate::agent::Agent::parse(&text, path, name).map_err(|error| error.to_string())
}

/// Refuse an agent whose runtime names another harness: `kersh run` runs
/// rig in this process, and `kersh start` hosts the others.
///
/// Only a runtime found on this machine that names another harness
/// refuses. gaff's hook agents run `kersh run` with a cleared environment
/// on any machine, so a missing runtime, a missing runtimes.yml, or one
/// that does not parse is not known to be another harness, and the run
/// goes ahead from the agent file's model.
fn check_rig_harness(agent: &crate::agent::Agent) -> Result<(), String> {
    let Some(runtime) = &agent.meta.runtime else {
        return Ok(());
    };
    let name = &agent.meta.name;
    let Ok(runtimes) = crate::runtime::load_runtimes(&crate::paths::Paths::discover()) else {
        return Ok(());
    };
    let Some(harness) = runtimes.get(runtime).and_then(|found| found.harness) else {
        return Ok(());
    };
    if harness != Harness::Rig {
        return Err(format!(
            "`{name}` runs on the {harness} harness (runtime `{runtime}`), not rig; \
             `kersh start {name}` runs it in a session"
        ));
    }
    Ok(())
}

fn run_inner(args: &RunArgs, stream: Option<&crate::jsonl::Stream>) -> Result<String, String> {
    let name = args.name.as_str();
    let mut agent = load_run_agent(args)?;
    if agent.meta.model.is_empty() {
        return Err(format!(
            "`{name}` names no model, so rig cannot run it; give it a `model` or another harness"
        ));
    }
    if let Some(profile) = &args.profile {
        agent.meta.profile = Some(profile.clone());
    }
    let stdin_twice = [&args.context_file, &args.prompt_file]
        .iter()
        .all(|path| path.as_deref() == Some(Path::new("-")));
    if stdin_twice {
        return Err("--context-file and --prompt-file cannot both read standard input".to_owned());
    }
    let context = args.context_file.as_deref().map(read_context).transpose()?;
    let prompt = match &args.prompt_file {
        Some(path) => read_context(path)?,
        None => args.prompt.clone().unwrap_or_default(),
    };
    let prompt = prompt.as_str();
    // An agent with a profile takes its situation from gaff's session-start
    // injection, so it needs no prompt on the command line. Without a
    // profile, the caller supplies the situation as a prompt or a context
    // file, or there is nothing for the agent to act on.
    if prompt.trim().is_empty() && context.is_none() && agent.meta.profile.is_none() {
        return Err(
            "give a prompt (as an argument or with --prompt-file), or context with --context-file"
                .to_owned(),
        );
    }

    let root_dir = args
        .root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = crate::tools::Root::new(&root_dir).map_err(|error| error.to_string())?;

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("cannot start the async runtime: {error}"))?;
    if let Some(stream) = stream {
        stream.init(name, &agent.meta.model);
    }

    // Governance is opt-in: an agent with a `profile` runs under gaff. The
    // session-start probe both fetches the injected context and proves
    // gaff can answer, so a broken or absent gaff aborts here rather than
    // leaving every tool call to fail closed into a dead run.
    let gaff = crate::gaff::for_agent(&agent);
    let gaff_context = match &gaff {
        Some(gaff) => match runtime.block_on(gaff.hook(crate::gaff::SESSION_START, None)) {
            crate::gaff::Outcome::Allow(context) => Some(context),
            crate::gaff::Outcome::Refuse(reason) => {
                return Err(format!("gaff refused the session start: {reason}"));
            }
            crate::gaff::Outcome::Failed(message) => {
                return Err(format!(
                    "gaff governance is on for `{name}`, but gaff could not start: {message}"
                ));
            }
        },
        None => None,
    };

    // The agent's almanac skill is its method, resolved before the run. A
    // named skill that cannot resolve aborts, so a run never silently drops
    // the agent's method. A session composed its system prompt at start,
    // with any skill or instructions it was given, and passes that file.
    let system = match &args.system_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?,
        ),
        None => None,
    };
    let skill = match system {
        Some(_) => None,
        None => crate::client::skill_body(&agent)?,
    };

    // gaff's session-start context and any --context-file are both
    // untrusted; the compose step wraps them together, gaff first.
    let combined = combine_context(gaff_context, context);
    let mut composed = crate::compose::compose(
        &agent,
        skill.as_deref(),
        combined.as_deref(),
        prompt,
        &crate::util::nonce(),
    );
    if let Some(system) = system {
        composed.system = system;
    }

    // A profiled agent takes its whole situation from gaff's session-start
    // injection. When that injection is empty and no prompt was given, the
    // first turn is empty and there is nothing to act on. The model layer
    // would report only an opaque empty-transcript error, so fail here with
    // the cause. The earlier guard cannot catch this: gaff answers the
    // injection at run time, not before.
    situation_present(&composed.first_turn, agent.meta.profile.as_deref(), name)?;

    runtime
        .block_on(crate::model::run(
            &agent,
            root,
            composed.system,
            composed.first_turn,
            gaff,
            stream.cloned(),
        ))
        .map_err(|error| error.to_string())
}

/// Join gaff's session-start context and the caller's context, gaff
/// first. Either may be absent.
fn combine_context(gaff: Option<String>, file: Option<String>) -> Option<String> {
    let gaff = gaff.filter(|s| !s.trim().is_empty());
    match (gaff, file) {
        (None, file) => file,
        (Some(gaff), None) => Some(gaff),
        (Some(gaff), Some(file)) => Some(format!("{gaff}\n\n{file}")),
    }
}

/// Check that the first user turn carries a situation to act on.
///
/// A profiled agent draws its situation from gaff's session-start
/// injection at run time. When that injection is empty and no prompt was
/// given, the turn is empty. The error names the profile so the operator
/// looks at what the profile should have injected. Without a profile the
/// empty turn means the caller gave neither a prompt nor a context file.
fn situation_present(first_turn: &str, profile: Option<&str>, name: &str) -> Result<(), String> {
    if !first_turn.trim().is_empty() {
        return Ok(());
    }
    Err(match profile {
        Some(profile) => format!(
            "the `{profile}` profile injected no context and no prompt was given, \
             so `{name}` has nothing to act on (is anything staged?)"
        ),
        None => {
            "give a prompt (as an argument or with --prompt-file), or context with --context-file"
                .to_owned()
        }
    })
}

/// Read `--context-file`, where `-` means standard input.
fn read_context(path: &Path) -> Result<String, String> {
    if path == Path::new("-") {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|error| format!("cannot read standard input: {error}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read `{}`: {error}", path.display()))
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("kersh: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::store::Stores;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn parse(list: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once(NAME).chain(list.iter().copied()))
            .unwrap()
            .command
    }

    #[test]
    fn the_parser_is_well_formed() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn an_empty_turn_under_a_profile_names_the_profile() {
        let error = situation_present("   \n", Some("commit-msg"), "commit-msg").unwrap_err();
        assert!(error.contains("commit-msg"), "{error}");
        assert!(error.contains("injected no context"), "{error}");
    }

    #[test]
    fn an_empty_turn_without_a_profile_asks_for_a_prompt() {
        let error = situation_present("", None, "explain").unwrap_err();
        assert!(error.contains("give a prompt"), "{error}");
        assert!(error.contains("--prompt-file"), "{error}");
    }

    #[test]
    fn a_turn_with_content_is_accepted() {
        assert!(situation_present("a diff", Some("commit-msg"), "commit-msg").is_ok());
    }

    fn root_with_agent() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("kersh.yml"), "").unwrap();
        let dir = tmp.path().join("agents").join("reviewer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("AGENT.md"),
            "---\nname: reviewer\ndescription: Reviews diffs.\nmodel: claude-code/haiku\n---\nYou review diffs.\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn list_succeeds_and_the_store_resolves_the_agent() {
        let tmp = root_with_agent();
        let root = tmp.path().to_str().unwrap();
        assert_eq!(main(args(&["list", "--root", root])), ExitCode::SUCCESS);
        let stores = Stores::from_root(PathBuf::from(root));
        assert_eq!(stores.names(), vec!["reviewer".to_string()]);
        assert!(stores.load("reviewer").is_ok());
    }

    #[test]
    fn init_scaffolds_a_root_and_a_valid_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_str().unwrap();
        assert_eq!(
            main(args(&["init", "--root", root, "auditor"])),
            ExitCode::SUCCESS
        );
        assert!(tmp.path().join("kersh.yml").exists());
        // The template it writes parses, so `check` passes at once.
        let stores = Stores::from_root(PathBuf::from(root));
        assert_eq!(stores.names(), vec!["auditor".to_string()]);
        assert!(stores.load("auditor").is_ok());
    }

    #[test]
    fn init_never_overwrites_an_edited_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_str().unwrap();
        let _ = main(args(&["init", "--root", root]));
        let agent = tmp.path().join("agents").join("reviewer").join("AGENT.md");
        std::fs::write(
            &agent,
            "---\nname: reviewer\nmodel: claude-code/haiku\n---\nmine\n",
        )
        .unwrap();
        // A second init leaves the edited file alone.
        assert_eq!(main(args(&["init", "--root", root])), ExitCode::SUCCESS);
        assert!(std::fs::read_to_string(&agent).unwrap().contains("mine"));
    }

    #[test]
    fn init_refuses_a_traversing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_str().unwrap();
        assert_eq!(
            main(args(&["init", "--root", root, "../evil"])),
            ExitCode::FAILURE
        );
        assert!(!tmp.path().join("agents").join("..").exists());
    }

    #[test]
    fn a_double_dash_lets_a_prompt_start_with_a_dash() {
        let Command::Run(args) =
            parse(&["run", "reviewer", "--root", "/r", "--", "-fix the tests"])
        else {
            panic!("not a run");
        };
        assert_eq!(args.name, "reviewer");
        assert_eq!(args.root, Some(PathBuf::from("/r")));
        assert_eq!(args.prompt.as_deref(), Some("-fix the tests"));
        assert_eq!(args.output, RunOutput::Text, "text is the default");
    }

    #[test]
    fn flags_may_follow_the_name_and_the_prompt() {
        let Command::Run(args) = parse(&["run", "reviewer", "review it", "--context-file", "-"])
        else {
            panic!("not a run");
        };
        assert_eq!(args.name, "reviewer");
        assert_eq!(args.context_file, Some(PathBuf::from("-")));
        assert_eq!(args.prompt.as_deref(), Some("review it"));
    }

    #[test]
    fn check_passes_on_a_good_store_and_fails_on_a_bad_one() {
        let tmp = root_with_agent();
        let root = tmp.path().to_str().unwrap();
        assert_eq!(main(args(&["check", "--root", root])), ExitCode::SUCCESS);

        // A file whose name does not match its directory is a problem.
        let bad = tmp.path().join("agents").join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("AGENT.md"),
            "---\nname: other\nmodel: claude-code/haiku\n---\nb\n",
        )
        .unwrap();
        assert_eq!(main(args(&["check", "--root", root])), ExitCode::FAILURE);
    }

    #[test]
    fn an_unknown_command_is_a_usage_error() {
        assert_eq!(main(args(&["frobnicate"])), ExitCode::from(2));
    }

    #[test]
    fn a_leading_kersh_word_is_dropped_once() {
        assert_eq!(main(args(&["kersh", "prime"])), ExitCode::SUCCESS);
        assert_eq!(main(args(&["kersh", "kersh", "prime"])), ExitCode::from(2));
    }

    #[test]
    fn docs_and_prime_succeed() {
        assert_eq!(main(args(&["docs"])), ExitCode::SUCCESS);
        assert_eq!(main(args(&["prime"])), ExitCode::SUCCESS);
    }

    #[test]
    fn the_primer_names_the_tui_only_in_a_build_with_it() {
        let text = format!("{PRIME}{FRONT_ENDS}");
        assert_eq!(text.contains("kersh tui"), cfg!(feature = "tui"), "{text}");
        assert!(text.contains("kersh serve"), "{text}");
    }
}
