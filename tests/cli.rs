//! The command surface through the built binary: exit codes, the leading
//! `kersh` word a multi-call binary passes, `--version`, and the `--json`
//! shape of every read.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn kersh(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kersh"))
        .args(args)
        .env_remove("KERSH_ALMANAC")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn write_agent(root: &Path, dir: &str, text: &str) {
    let path = root.join("agents").join(dir);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("AGENT.md"), text).unwrap();
}

/// A root with a good agent, a minimal agent, and a broken one.
fn root() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("kersh.yml"), "").unwrap();
    write_agent(
        tmp.path(),
        "reviewer",
        "---\nname: reviewer\ndescription: Reviews diffs.\nmodel: claude-code/haiku\nprofile: reviewer\nmax_turns: 3\ntimeout: 2m\n---\nYou review diffs.\n",
    );
    write_agent(
        tmp.path(),
        "plain",
        "---\nname: plain\nmodel: anthropic/claude-sonnet-5\n---\nPlain body.\n",
    );
    write_agent(
        tmp.path(),
        "broken",
        "---\nname: other\nmodel: claude-code/haiku\n---\nb\n",
    );
    tmp
}

fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

#[test]
fn a_leading_kersh_word_dispatches_the_same_command() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let plain = kersh(&["list", "--root", r]);
    let multi = kersh(&["kersh", "list", "--root", r]);
    assert!(plain.status.success());
    assert_eq!(plain.status.code(), multi.status.code());
    assert_eq!(stdout(&plain), stdout(&multi));
    assert_eq!(
        kersh(&["kersh", "--version"]).stdout,
        kersh(&["--version"]).stdout
    );
}

#[test]
fn version_prints_the_package_version() {
    let output = kersh(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        format!("kersh {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn a_usage_error_exits_2_on_stderr() {
    for args in [&["frobnicate"][..], &["show"], &["list", "--bogus"], &[]] {
        let output = kersh(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {}", stdout(&output));
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn a_failure_exits_1() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["show", "missing", "--root", r]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no agent named `missing`"));
}

#[test]
fn list_text_is_unchanged() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["list", "--root", r]);
    assert_eq!(stdout(&output), "broken\nplain\nreviewer  Reviews diffs.\n");
}

#[test]
fn list_json_is_one_object_per_agent() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["list", "--json", "--root", r]);
    assert!(output.status.success());
    let rows = lines(&stdout(&output));
    assert_eq!(rows.len(), 3);

    let broken = &rows[0];
    assert_eq!(broken["name"], "broken");
    assert!(broken["error"].as_str().unwrap().contains("names `other`"));
    assert!(
        broken["path"]
            .as_str()
            .unwrap()
            .ends_with("agents/broken/AGENT.md")
    );

    let plain = &rows[1];
    assert_eq!(plain["name"], "plain");
    assert_eq!(plain["description"], "");
    assert_eq!(plain["model"], "anthropic/claude-sonnet-5");
    assert_eq!(plain["skill"], Value::Null);
    assert_eq!(plain["profile"], Value::Null);
    assert!(plain.get("error").is_none());

    let reviewer = &rows[2];
    assert_eq!(reviewer["description"], "Reviews diffs.");
    assert_eq!(reviewer["profile"], "reviewer");
    assert!(
        reviewer["path"]
            .as_str()
            .unwrap()
            .ends_with("agents/reviewer/AGENT.md")
    );
}

#[test]
fn an_empty_store_lists_no_json_lines() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("kersh.yml"), "").unwrap();
    let output = kersh(&["list", "--json", "--root", tmp.path().to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}

#[test]
fn show_json_is_one_object_with_the_body() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["show", "reviewer", "--json", "--root", r]);
    assert!(output.status.success());
    let rows = lines(&stdout(&output));
    assert_eq!(rows.len(), 1);
    let agent = &rows[0];
    assert_eq!(agent["name"], "reviewer");
    assert_eq!(agent["model"], "claude-code/haiku");
    assert_eq!(agent["max_turns"], 3);
    assert_eq!(agent["timeout_secs"], 120);
    assert_eq!(agent["body"], "You review diffs.");
}

#[test]
fn show_text_is_unchanged() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["show", "reviewer", "--root", r]);
    assert_eq!(
        stdout(&output),
        "name:    reviewer\nmodel:   claude-code/haiku\nprofile: reviewer\nturns:   3\ntimeout: 120s\n---\nYou review diffs.\n"
    );
}

#[test]
fn check_json_is_a_line_per_problem_then_a_summary() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["check", "--json", "--root", r]);
    assert_eq!(output.status.code(), Some(1));
    let rows = lines(&stdout(&output));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["kind"], "problem");
    assert_eq!(rows[0]["name"], "broken");
    assert!(rows[0]["error"].as_str().unwrap().contains("names `other`"));
    assert_eq!(rows[1]["kind"], "summary");
    assert_eq!(rows[1]["agents"], 3);
    assert_eq!(rows[1]["problems"], 1);
    assert_eq!(rows[1]["ok"], false);

    std::fs::remove_dir_all(tmp.path().join("agents").join("broken")).unwrap();
    let output = kersh(&["check", "--json", "--root", r]);
    assert!(output.status.success());
    let rows = lines(&stdout(&output));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[0]["agents"], 2);
}

#[test]
fn check_text_is_unchanged() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["check", "--root", r]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("kersh: "));
    std::fs::remove_dir_all(tmp.path().join("agents").join("broken")).unwrap();
    assert_eq!(stdout(&kersh(&["check", "--root", r])), "ok: 2 agent(s)\n");
}

#[test]
fn a_root_without_kersh_yml_is_refused_in_every_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("bare");
    std::fs::create_dir(&bare).unwrap();
    let typo = tmp.path().join("typo");
    for dir in [&bare, &typo] {
        let d = dir.to_str().unwrap();
        let want = format!("kersh: `{d}` is not a kersh root (no kersh.yml)\n");
        for args in [
            &["check", "--root", d][..],
            &["check", "--json", "--root", d],
            &["list", "--root", d],
            &["list", "--json", "--root", d],
            &["show", "reviewer", "--root", d],
            &["show", "reviewer", "--json", "--root", d],
            &["render", "reviewer", "--root", d, "go"],
            &["render", "reviewer", "--json", "--root", d, "go"],
            &["run", "reviewer", "--root", d, "go"],
        ] {
            let output = kersh(args);
            assert_eq!(output.status.code(), Some(1), "{args:?}");
            assert!(output.stdout.is_empty(), "{args:?}: {}", stdout(&output));
            assert_eq!(String::from_utf8_lossy(&output.stderr), want, "{args:?}");
        }
    }
}

/// Run a program and return its trimmed stdout.
fn tool(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(output.status.success(), "{program} {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// The home directory the user database records for this user, read
/// through the platform's own tool rather than `HOME`.
fn user_database_home() -> PathBuf {
    let user = tool("id", &["-un"]);
    let home = if cfg!(target_os = "macos") {
        let record = tool(
            "dscl",
            &[".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"],
        );
        record.split_once(':').unwrap().1.trim().to_owned()
    } else {
        let record = tool("getent", &["passwd", &user]);
        record.split(':').nth(5).unwrap().to_owned()
    };
    PathBuf::from(home)
}

#[test]
fn with_no_home_the_user_store_comes_from_the_user_database() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("kersh.yml"), "").unwrap();

    // `env -i`: no HOME, no PATH, nothing inherited, as gaff runs a hook
    // agent.
    let output = Command::new("env")
        .arg("-i")
        .arg(env!("CARGO_BIN_EXE_kersh"))
        .args(["check", "--json"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = lines(&stdout(&output));
    let user_store = user_database_home().join(".config").join("kersh");
    assert_eq!(
        rows.last().unwrap()["roots"],
        serde_json::json!([root, user_store]),
        "the user store resolves under the user database's home"
    );

    // A set HOME wins over the user database.
    let home = root.join("home");
    let output = Command::new("env")
        .arg("-i")
        .arg(format!("HOME={}", home.display()))
        .arg(env!("CARGO_BIN_EXE_kersh"))
        .args(["check", "--json"])
        .current_dir(&root)
        .output()
        .unwrap();
    let rows = lines(&stdout(&output));
    assert_eq!(
        rows.last().unwrap()["roots"],
        serde_json::json!([root, home.join(".config").join("kersh")])
    );
}

#[test]
fn render_json_is_the_system_prompt_and_the_first_turn() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["render", "plain", "--json", "--root", r, "--", "-go now"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = lines(&stdout(&output));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["system"], "Plain body.");
    assert_eq!(rows[0]["first_turn"], "-go now");
}

#[test]
fn render_text_is_unchanged() {
    let tmp = root();
    let r = tmp.path().to_str().unwrap();
    let output = kersh(&["render", "plain", "--root", r, "go"]);
    assert_eq!(
        stdout(&output),
        "=== system prompt ===\nPlain body.\n\n=== first user turn ===\ngo\n"
    );
}
