//! What kersh runs for each harness and mode, and which build runs.
//!
//! One function per harness builds the argument vector from the record's
//! persistent parts, so a launch and every resume pass the same
//! instructions, settings, and flags. No shell parses the vector.
//!
//! Flags a caller adds are checked against a per-harness allow-list that
//! denies by default. It leaves out every flag that would skip or replace
//! the hooks gaff registers: Claude Code's `--bare`, `--setting-sources`,
//! and `--settings` (kersh composes its own), and Codex's
//! `--dangerously-bypass-hook-trust`, `--enable`, `--disable`, and `-c`
//! keys under `features` or `hooks`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::{Build, Harness, HarnessBuild, Mode};

/// The settings kersh passes to every Claude Code session: native auto
/// memory off, because memory lives in the user's notes store.
pub const CLAUDE_SETTINGS: &str = r#"{"autoMemoryEnabled": false}"#;

/// The Codex feature kersh turns off in every session, for the same
/// reason.
pub const CODEX_DISABLED_FEATURE: &str = "memories";

/// Which conversation a launch starts or continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conversation<'a> {
    /// A new conversation. Claude Code takes the id at launch; Codex
    /// chooses its own, so the id is `None` there.
    New(Option<&'a str>),
    /// Continue this conversation.
    Resume(&'a str),
}

/// Everything a launch needs from the record.
#[derive(Debug, Clone)]
pub struct LaunchSpec<'a> {
    pub harness: Harness,
    pub mode: Mode,
    pub program: &'a Path,
    pub model: Option<&'a str>,
    /// The composed instructions file.
    pub system_prompt: Option<&'a Path>,
    /// Its text, for a harness that takes the text rather than a path.
    pub system_text: Option<&'a str>,
    pub flags: &'a [String],
    pub conversation: Conversation<'a>,
    /// An interactive session's prompt, passed as its argument.
    pub prompt_arg: Option<&'a str>,
    /// What a rig session runs; `None` for the other harnesses.
    pub rig: Option<RigRun<'a>>,
}

/// A rig session's run: `kersh run --output jsonl` of its agent file.
#[derive(Debug, Clone, Copy)]
pub struct RigRun<'a> {
    /// The running kersh's entry argv, never one found on `PATH`.
    pub entry: &'a [String],
    pub agent: &'a str,
    pub agent_file: &'a Path,
    /// The gaff profile the session resolved.
    pub profile: Option<&'a str>,
}

/// The argument vector for a launch. A headless launch reads its prompt
/// on standard input; the caller connects the prompt file there.
#[must_use]
pub fn argv(spec: &LaunchSpec<'_>) -> Vec<String> {
    match spec.harness {
        Harness::ClaudeCode => claude_argv(spec),
        Harness::Codex => codex_argv(spec),
        Harness::Rig => rig_argv(spec),
    }
}

fn rig_argv(spec: &LaunchSpec<'_>) -> Vec<String> {
    let Some(rig) = spec.rig else {
        return vec![spec.program.display().to_string()];
    };
    let mut argv = rig.entry.to_vec();
    argv.extend([
        "run".to_owned(),
        rig.agent.to_owned(),
        "--output".to_owned(),
        "jsonl".to_owned(),
        "--agent-file".to_owned(),
        rig.agent_file.display().to_string(),
    ]);
    if let Some(path) = spec.system_prompt {
        argv.extend(["--system-file".to_owned(), path.display().to_string()]);
    }
    if let Some(profile) = rig.profile {
        argv.extend(["--profile".to_owned(), profile.to_owned()]);
    }
    argv.extend(["--prompt-file".to_owned(), "-".to_owned()]);
    argv
}

/// The build a rig session runs: this kersh, by its entry argv.
#[must_use]
pub fn rig_build(entry: &[String]) -> Build {
    Build {
        installer: "entry".to_owned(),
        path: PathBuf::from(entry.first().map_or("kersh", String::as_str)),
        version: format!("kersh {}", env!("CARGO_PKG_VERSION")),
    }
}

fn claude_argv(spec: &LaunchSpec<'_>) -> Vec<String> {
    let mut argv = vec![spec.program.display().to_string()];
    if spec.mode == Mode::Headless {
        argv.push("-p".to_owned());
    }
    match spec.conversation {
        Conversation::New(Some(id)) => {
            argv.extend(["--session-id".to_owned(), id.to_owned()]);
        }
        Conversation::New(None) => {}
        Conversation::Resume(id) => argv.extend(["--resume".to_owned(), id.to_owned()]),
    }
    if spec.mode == Mode::Headless {
        argv.extend([
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
        ]);
    }
    if let Some(path) = spec.system_prompt {
        argv.extend([
            "--append-system-prompt-file".to_owned(),
            path.display().to_string(),
        ]);
    }
    argv.extend(["--settings".to_owned(), CLAUDE_SETTINGS.to_owned()]);
    if let Some(model) = spec.model {
        argv.extend(["--model".to_owned(), model.to_owned()]);
    }
    argv.extend(spec.flags.iter().cloned());
    if spec.mode == Mode::Interactive
        && let Some(prompt) = spec.prompt_arg
    {
        argv.extend(["--".to_owned(), prompt.to_owned()]);
    }
    argv
}

fn codex_argv(spec: &LaunchSpec<'_>) -> Vec<String> {
    let mut argv = vec![spec.program.display().to_string()];
    if spec.mode == Mode::Headless {
        argv.extend(["exec".to_owned(), "--json".to_owned()]);
    }
    if let Some(text) = spec.system_text.filter(|text| !text.trim().is_empty()) {
        argv.extend([
            "-c".to_owned(),
            format!("developer_instructions={}", toml_string(text)),
        ]);
    }
    argv.extend(["--disable".to_owned(), CODEX_DISABLED_FEATURE.to_owned()]);
    if let Some(model) = spec.model {
        argv.extend(["-m".to_owned(), model.to_owned()]);
    }
    argv.extend(spec.flags.iter().cloned());
    let resume = if let Conversation::Resume(id) = spec.conversation {
        argv.extend(["resume".to_owned(), id.to_owned()]);
        true
    } else {
        false
    };
    match (spec.mode, spec.prompt_arg) {
        (Mode::Headless, _) => argv.push("-".to_owned()),
        // `codex resume [SESSION_ID] [PROMPT]`, as `codex resume --help`
        // documents it: the prompt follows the id. A prompt that looks
        // like an option goes after `--`.
        (Mode::Interactive, Some(prompt)) if resume && !prompt.starts_with('-') => {
            argv.push(prompt.to_owned());
        }
        (Mode::Interactive, Some(prompt)) => {
            argv.extend(["--".to_owned(), prompt.to_owned()]);
        }
        (Mode::Interactive, None) => {}
    }
    argv
}

/// `text` as a TOML basic string, for a Codex `-c key=value` override.
#[must_use]
pub fn toml_string(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One allowed flag: its spellings and whether it takes a value.
struct Allowed {
    names: &'static [&'static str],
    value: bool,
}

const CLAUDE_FLAGS: &[Allowed] = &[
    Allowed {
        names: &["--permission-mode"],
        value: true,
    },
    Allowed {
        names: &["--allowedTools", "--allowed-tools"],
        value: true,
    },
    Allowed {
        names: &["--disallowedTools", "--disallowed-tools"],
        value: true,
    },
    Allowed {
        names: &["--add-dir"],
        value: true,
    },
    Allowed {
        names: &["--effort"],
        value: true,
    },
    Allowed {
        names: &["--fallback-model"],
        value: true,
    },
    Allowed {
        names: &["--max-turns"],
        value: true,
    },
];

const CODEX_FLAGS: &[Allowed] = &[
    Allowed {
        names: &["-s", "--sandbox"],
        value: true,
    },
    Allowed {
        names: &["-a", "--ask-for-approval"],
        value: true,
    },
    Allowed {
        names: &["--add-dir"],
        value: true,
    },
    Allowed {
        names: &["-c", "--config"],
        value: true,
    },
    Allowed {
        names: &["--search"],
        value: false,
    },
    Allowed {
        names: &["--full-auto"],
        value: false,
    },
];

/// Codex `-c` keys a caller may not set: the feature and hook tables,
/// and the instructions kersh composes.
const CODEX_DENIED_KEYS: &[&str] = &[
    "features",
    "hooks",
    "developer_instructions",
    "model_instructions_file",
];

/// Check caller flags against the harness's allow-list. The model has
/// its own option (`--model`), so it is not a harness flag here.
pub fn check_flags(harness: Harness, flags: &[String]) -> Result<(), String> {
    let table = match harness {
        Harness::ClaudeCode => CLAUDE_FLAGS,
        Harness::Codex => CODEX_FLAGS,
        Harness::Rig => {
            if let Some(flag) = flags.first() {
                return Err(format!("rig takes no harness flags; `{flag}` is refused"));
            }
            return Ok(());
        }
    };
    let mut iter = flags.iter();
    while let Some(flag) = iter.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (flag.as_str(), None),
        };
        let Some(allowed) = table.iter().find(|allowed| allowed.names.contains(&name)) else {
            return Err(format!(
                "`{name}` is not on the {harness} flag allow-list; kersh passes only flags \
                 that keep the session's hooks and settings"
            ));
        };
        if !allowed.value {
            if inline.is_some() {
                return Err(format!("`{name}` takes no value"));
            }
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("`{name}` needs a value"))?,
        };
        if harness == Harness::Codex && matches!(name, "-c" | "--config") {
            let key = value.split_once('=').map_or(value.as_str(), |(key, _)| key);
            let key = key.trim();
            let root = key.split('.').next().unwrap_or(key);
            if CODEX_DENIED_KEYS.contains(&root) {
                return Err(format!(
                    "`-c {key}` is refused: kersh does not override Codex's {root} settings"
                ));
            }
        }
    }
    Ok(())
}

/// Find `program` on `path_var` (a `PATH` value): the first executable
/// file.
#[must_use]
pub fn find_on_path(program: &str, path_var: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The `path` installer: the build on the login `PATH`, and its version.
pub fn resolve(harness: Harness, login_env: &[(String, String)]) -> Result<Build, String> {
    let path_var = login_env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    let program = harness.program();
    let path = find_on_path(program, path_var)
        .ok_or_else(|| format!("`{program}` is not on the login PATH"))?;
    let version = version_of(&path, login_env)?;
    Ok(Build {
        installer: "path".to_owned(),
        path,
        version,
    })
}

/// The build a restore runs: the recorded path when it still runs, else
/// the one on the login `PATH`, with the version it reports now.
pub fn recheck(
    harness: Harness,
    recorded: &Build,
    login_env: &[(String, String)],
) -> Result<Build, String> {
    if recorded.path.is_file()
        && let Ok(version) = version_of(&recorded.path, login_env)
    {
        return Ok(Build {
            installer: recorded.installer.clone(),
            path: recorded.path.clone(),
            version,
        });
    }
    resolve(harness, login_env)
}

/// The first line `<path> --version` prints, run with the login
/// environment and a deadline.
fn version_of(path: &Path, login_env: &[(String, String)]) -> Result<String, String> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .env_clear()
        .envs(login_env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run `{} --version`: {error}", path.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`{} --version` did not answer", path.display()));
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read as _;
        let _ = stdout.read_to_string(&mut out);
    }
    Ok(out.lines().next().unwrap_or_default().trim().to_owned())
}

/// Every harness kersh can host, with the build each would run.
#[must_use]
pub fn list(login_env: &[(String, String)]) -> Vec<HarnessBuild> {
    [Harness::ClaudeCode, Harness::Codex]
        .into_iter()
        .map(|harness| match resolve(harness, login_env) {
            Ok(build) => HarnessBuild {
                harness,
                installer: build.installer,
                path: Some(build.path),
                version: Some(build.version),
                error: None,
            },
            Err(error) => HarnessBuild {
                harness,
                installer: "path".to_owned(),
                path: None,
                version: None,
                error: Some(error),
            },
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn spec(harness: Harness, mode: Mode, conversation: Conversation<'_>) -> LaunchSpec<'_> {
        LaunchSpec {
            harness,
            mode,
            program: Path::new("/bin/h"),
            model: None,
            system_prompt: Some(Path::new("/s/system.md")),
            system_text: Some("be brief"),
            flags: &[],
            conversation,
            prompt_arg: Some("go"),
            rig: None,
        }
    }

    #[test]
    fn rig_runs_this_kersh_with_jsonl_output_and_the_prompt_on_stdin() {
        let entry = strings(&["/opt/kersh", "kersh"]);
        let mut rig = spec(Harness::Rig, Mode::Headless, Conversation::New(None));
        rig.rig = Some(RigRun {
            entry: &entry,
            agent: "fixer",
            agent_file: Path::new("/a/agents/fixer/AGENT.md"),
            profile: Some("worker"),
        });
        assert_eq!(
            argv(&rig),
            strings(&[
                "/opt/kersh",
                "kersh",
                "run",
                "fixer",
                "--output",
                "jsonl",
                "--agent-file",
                "/a/agents/fixer/AGENT.md",
                "--system-file",
                "/s/system.md",
                "--profile",
                "worker",
                "--prompt-file",
                "-",
            ])
        );
        let build = rig_build(&entry);
        assert_eq!(build.path, Path::new("/opt/kersh"));
        assert_eq!(build.installer, "entry");
    }

    #[test]
    fn claude_headless_reads_the_prompt_on_stdin_with_the_probed_flags() {
        let argv = argv(&spec(
            Harness::ClaudeCode,
            Mode::Headless,
            Conversation::New(Some("u-1")),
        ));
        assert_eq!(
            argv,
            strings(&[
                "/bin/h",
                "-p",
                "--session-id",
                "u-1",
                "--output-format",
                "stream-json",
                "--verbose",
                "--append-system-prompt-file",
                "/s/system.md",
                "--settings",
                CLAUDE_SETTINGS,
            ])
        );
    }

    #[test]
    fn claude_interactive_takes_the_prompt_after_a_double_dash() {
        let argv = argv(&spec(
            Harness::ClaudeCode,
            Mode::Interactive,
            Conversation::Resume("u-1"),
        ));
        assert_eq!(argv.get(1..3).unwrap(), strings(&["--resume", "u-1"]));
        assert_eq!(argv.get(argv.len() - 2..).unwrap(), strings(&["--", "go"]));
        assert!(!argv.contains(&"--bare".to_owned()));
    }

    #[test]
    fn codex_headless_passes_instructions_as_a_toml_string_and_reads_stdin() {
        let argv = argv(&spec(
            Harness::Codex,
            Mode::Headless,
            Conversation::Resume("t-1"),
        ));
        assert_eq!(
            argv,
            strings(&[
                "/bin/h",
                "exec",
                "--json",
                "-c",
                "developer_instructions=\"be brief\"",
                "--disable",
                "memories",
                "resume",
                "t-1",
                "-",
            ])
        );
    }

    #[test]
    fn codex_interactive_resume_takes_the_prompt_after_the_id() {
        let argv = argv(&spec(
            Harness::Codex,
            Mode::Interactive,
            Conversation::Resume("t-1"),
        ));
        assert_eq!(
            argv.get(argv.len() - 3..).unwrap(),
            strings(&["resume", "t-1", "go"])
        );
        let mut dashed = spec(
            Harness::Codex,
            Mode::Interactive,
            Conversation::Resume("t-1"),
        );
        dashed.prompt_arg = Some("-x");
        let argv = super::argv(&dashed);
        assert_eq!(argv.get(argv.len() - 2..).unwrap(), strings(&["--", "-x"]));
        let fresh = super::argv(&spec(
            Harness::Codex,
            Mode::Interactive,
            Conversation::New(None),
        ));
        assert_eq!(
            fresh.get(fresh.len() - 2..).unwrap(),
            strings(&["--", "go"])
        );
    }

    #[test]
    fn a_toml_string_escapes_quotes_newlines_and_controls() {
        assert_eq!(toml_string("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\u0001""#);
    }

    #[test]
    fn the_allow_list_denies_by_default_and_refuses_hook_skipping_flags() {
        for flag in ["--bare", "--setting-sources", "--settings", "--anything"] {
            assert!(
                check_flags(Harness::ClaudeCode, &strings(&[flag, "x"])).is_err(),
                "{flag}"
            );
        }
        for flags in [
            vec!["--dangerously-bypass-hook-trust"],
            vec!["--enable", "memories"],
            vec!["-c", "features.hooks=false"],
            vec!["-c", "hooks.PreToolUse=[]"],
            vec!["--config=developer_instructions=x"],
        ] {
            assert!(
                check_flags(Harness::Codex, &strings(&flags)).is_err(),
                "{flags:?}"
            );
        }
    }

    #[test]
    fn every_denied_codex_key_is_refused_however_it_is_written() {
        for flags in [
            vec!["-c", "model_instructions_file=/x"],
            vec!["--config", "developer_instructions=x"],
            vec!["-c", " hooks.PreToolUse=[]"],
            vec!["-c", "features"],
        ] {
            let error = check_flags(Harness::Codex, &strings(&flags)).unwrap_err();
            assert!(error.contains("is refused"), "{flags:?}: {error}");
        }
    }

    #[test]
    fn a_flag_without_a_value_refuses_an_inline_one() {
        let error = check_flags(Harness::Codex, &strings(&["--search=x"])).unwrap_err();
        assert_eq!(error, "`--search` takes no value");
    }

    #[test]
    fn rig_takes_no_harness_flags() {
        let error =
            check_flags(Harness::Rig, &strings(&["--permission-mode", "plan"])).unwrap_err();
        assert!(error.contains("rig takes no harness flags"), "{error}");
        check_flags(Harness::Rig, &[]).unwrap();
    }

    #[test]
    fn allowed_flags_pass_with_their_values() {
        check_flags(
            Harness::ClaudeCode,
            &strings(&["--permission-mode", "plan", "--add-dir=/x"]),
        )
        .unwrap();
        check_flags(
            Harness::Codex,
            &strings(&[
                "-s",
                "workspace-write",
                "--search",
                "-c",
                "model_reasoning_effort=high",
            ]),
        )
        .unwrap();
        assert!(check_flags(Harness::ClaudeCode, &strings(&["--add-dir"])).is_err());
    }

    #[test]
    fn a_program_is_found_on_the_given_path_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("claude");
        std::fs::write(&program, "#!/bin/sh\necho '9.9.9 (fake)'\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_var = format!("/nonexistent:{}", tmp.path().display());
        assert_eq!(find_on_path("claude", &path_var), Some(program.clone()));
        assert_eq!(find_on_path("claude", "/nonexistent"), None);
        let env = vec![("PATH".to_owned(), format!("/bin:/usr/bin:{path_var}"))];
        let build = resolve(Harness::ClaudeCode, &env).unwrap();
        assert_eq!(build.version, "9.9.9 (fake)");
        assert_eq!(build.installer, "path");
    }
}
