//! The command surface. `main` is a thin shim so the whole surface is
//! testable in process, as the sibling tools are.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::store::Stores;

const PRIME: &str = "\
# kersh
Run a declarative agent. An agent is `agents/<name>/AGENT.md`: frontmatter
names a model, a skill, and a gaff profile; the body is the system prompt.
The profile carries the agent's context, guards, and stop rule.
Commands:
  kersh list
  kersh show <name>
  kersh check
  kersh render <name> [--context-file <path>] [prompt]
  kersh docs
";

const DOCS: &str = include_str!("../docs/kersh.md");

/// Run the CLI. Returns the process exit code.
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    let Some((command, rest)) = args.split_first() else {
        eprintln!("usage: kersh <list|show|check|render|run|docs|prime> [args]");
        return ExitCode::FAILURE;
    };
    let command = command.as_str();
    match command {
        "list" => list(rest),
        "show" => show(rest),
        "check" => check(rest),
        "render" => render(rest),
        "run" => run_agent(rest),
        "docs" => {
            print!("{DOCS}");
            ExitCode::SUCCESS
        }
        "prime" => {
            print!("{PRIME}");
            ExitCode::SUCCESS
        }
        "-h" | "--help" | "help" => {
            println!("usage: kersh <list|show|check|render|run|docs|prime> [args]");
            ExitCode::SUCCESS
        }
        other => fail(&format!("unknown command `{other}`")),
    }
}

/// Resolve the stores from `--root` or by discovery.
fn stores(rest: &[String]) -> Stores {
    if let Some(root) = flag_value(rest, "--root") {
        return Stores::from_root(PathBuf::from(root));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Stores::discover(&cwd, home.as_deref())
}

fn list(rest: &[String]) -> ExitCode {
    let stores = stores(rest);
    let names = stores.names();
    if names.is_empty() {
        println!("no agents. Add agents/<name>/AGENT.md under a kersh.yml root.");
        return ExitCode::SUCCESS;
    }
    for name in names {
        match stores.load(&name) {
            Ok(agent) if !agent.meta.description.is_empty() => {
                println!("{name}  {}", agent.meta.description);
            }
            _ => println!("{name}"),
        }
    }
    ExitCode::SUCCESS
}

fn show(rest: &[String]) -> ExitCode {
    let Some(name) = positional(rest) else {
        return fail("usage: kersh show <name>");
    };
    let stores = stores(rest);
    match stores.load(&name) {
        Ok(agent) => {
            println!("name:    {}", agent.meta.name);
            println!("model:   {}", agent.meta.model);
            if let Some(skill) = &agent.meta.skill {
                println!("skill:   {skill}");
            }
            if let Some(profile) = &agent.meta.profile {
                println!("profile: {profile}");
            }
            println!("turns:   {}", agent.meta.max_turns);
            println!("timeout: {:?}", agent.meta.timeout.0);
            println!("---\n{}", agent.body);
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error.to_string()),
    }
}

fn check(rest: &[String]) -> ExitCode {
    let stores = stores(rest);
    let names = stores.names();
    let mut problems = Vec::new();
    for name in &names {
        if let Err(error) = stores.load(name) {
            problems.push(error.to_string());
        }
    }
    if problems.is_empty() {
        println!("ok: {} agent(s)", names.len());
        ExitCode::SUCCESS
    } else {
        for problem in &problems {
            eprintln!("kersh: {problem}");
        }
        ExitCode::FAILURE
    }
}

fn render(rest: &[String]) -> ExitCode {
    let Some(name) = positional(rest) else {
        return fail("usage: kersh render <name> [--context-file <path>] [prompt]");
    };
    let stores = stores(rest);
    let agent = match stores.load(&name) {
        Ok(agent) => agent,
        Err(error) => return fail(&error.to_string()),
    };
    let context = match flag_value(rest, "--context-file") {
        Some(path) => match read_context(&path) {
            Ok(text) => Some(text),
            Err(error) => return fail(&error),
        },
        None => None,
    };
    let prompt = positional_after(rest, &name).unwrap_or_default();
    let composed = crate::compose::compose(
        &agent,
        None,
        context.as_deref(),
        &prompt,
        &crate::util::nonce(),
    );
    println!("=== system prompt ===\n{}\n", composed.system);
    println!("=== first user turn ===\n{}", composed.first_turn);
    ExitCode::SUCCESS
}

/// `kersh run <name> [--context-file <path|->] [--root <dir>] [prompt]`.
///
/// The agent runs through rig against the provider its `model` names.
/// The final text goes to standard output. The tools are confined to the
/// current directory, or `--root`.
fn run_agent(rest: &[String]) -> ExitCode {
    let Some(name) = positional(rest) else {
        return fail("usage: kersh run <name> [--context-file <path>] [prompt]");
    };
    let stores = stores(rest);
    let agent = match stores.load(&name) {
        Ok(agent) => agent,
        Err(error) => return fail(&error.to_string()),
    };
    let context = match flag_value(rest, "--context-file") {
        Some(path) => match read_context(&path) {
            Ok(text) => Some(text),
            Err(error) => return fail(&error),
        },
        None => None,
    };
    let prompt = positional_after(rest, &name).unwrap_or_default();
    if prompt.trim().is_empty() && context.is_none() {
        return fail("give a prompt, or context with --context-file");
    }

    let root_dir = flag_value(rest, "--root")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let root = match crate::tools::Root::new(&root_dir) {
        Ok(root) => root,
        Err(error) => return fail(&error.to_string()),
    };

    let composed = crate::compose::compose(
        &agent,
        None,
        context.as_deref(),
        &prompt,
        &crate::util::nonce(),
    );

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return fail(&format!("cannot start the async runtime: {error}")),
    };
    let outcome = runtime.block_on(crate::model::run(
        &agent,
        root,
        composed.system,
        composed.first_turn,
    ));
    match outcome {
        Ok(answer) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error.to_string()),
    }
}

/// Read `--context-file`, where `-` means standard input.
fn read_context(path: &str) -> Result<String, String> {
    if path == "-" {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|error| format!("cannot read standard input: {error}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).map_err(|error| format!("cannot read `{path}`: {error}"))
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("kersh: {message}");
    ExitCode::FAILURE
}

/// The value after `flag`, if present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The first positional argument (not a flag or a flag value).
fn positional(args: &[String]) -> Option<String> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            skip_next = takes_value(arg);
            continue;
        }
        return Some(arg.clone());
    }
    None
}

/// The first positional after the one equal to `first`, so `render <name>
/// <prompt>` finds the prompt.
fn positional_after(args: &[String], first: &str) -> Option<String> {
    let mut seen_first = false;
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            skip_next = takes_value(arg);
            continue;
        }
        if !seen_first && arg == first {
            seen_first = true;
            continue;
        }
        if seen_first {
            return Some(arg.clone());
        }
    }
    None
}

/// Whether a flag consumes the following argument.
fn takes_value(flag: &str) -> bool {
    matches!(flag, "--root" | "--context-file")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
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
    fn list_names_the_agents() {
        let tmp = root_with_agent();
        let code = list(&args(&["--root", tmp.path().to_str().unwrap()]));
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[test]
    fn check_passes_on_a_good_store_and_fails_on_a_bad_one() {
        let tmp = root_with_agent();
        let root = tmp.path().to_str().unwrap();
        assert_eq!(check(&args(&["--root", root])), ExitCode::SUCCESS);

        // A file whose name does not match its directory is a problem.
        let bad = tmp.path().join("agents").join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("AGENT.md"),
            "---\nname: other\nmodel: claude-code/haiku\n---\nb\n",
        )
        .unwrap();
        assert_eq!(check(&args(&["--root", root])), ExitCode::FAILURE);
    }

    #[test]
    fn unknown_command_fails_without_panicking() {
        assert_eq!(run(&args(&["frobnicate"])), ExitCode::FAILURE);
    }

    #[test]
    fn docs_and_prime_succeed() {
        assert_eq!(run(&args(&["docs"])), ExitCode::SUCCESS);
        assert_eq!(run(&args(&["prime"])), ExitCode::SUCCESS);
    }
}
