//! An end-to-end run against the real `claude` CLI. Ignored by default,
//! because it needs a logged-in CLI and spends that login's usage.
//!
//! Run with `cargo test --test real_run -- --ignored`.

#![allow(clippy::unwrap_used)]

use std::process::Command;

#[test]
#[ignore = "needs a logged-in claude CLI and spends usage"]
fn an_agent_reads_a_file_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kersh.yml"), "").unwrap();
    std::fs::write(dir.path().join("note.txt"), "The code is PLUM-42.\n").unwrap();
    let agent = dir.path().join("agents").join("reader");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("AGENT.md"),
        "---\nname: reader\nmodel: claude-code/haiku\nmax_turns: 3\n---\nRead note.txt with read_file and state the code.\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kersh"))
        .current_dir(dir.path())
        .args(["run", "reader", "What is the code?"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("PLUM-42"),
        "the model did not read the file: {stdout}"
    );
}

/// A governed run: a fake gaff injects a fact at session start, and the
/// model reports it. Ignored, because it needs a logged-in claude CLI.
#[test]
#[ignore = "needs a logged-in claude CLI and spends usage"]
fn a_governed_run_injects_the_session_start_context() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kersh.yml"), "").unwrap();
    let fake = dir.path().join("fakegaff");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \"$(cat)\" in *session_start*) printf '{\"event\":\"session_start\",\"context\":\"The deploy id is DEPLOY-777.\"}';; esac\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = dir.path().join("agents").join("greeter");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("AGENT.md"),
        "---\nname: greeter\nmodel: claude-code/haiku\nprofile: reviewer\nmax_turns: 2\n---\nAnswer in one short sentence.\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kersh"))
        .current_dir(dir.path())
        .env("KERSH_GAFF", &fake)
        .args([
            "run",
            "greeter",
            "What is the deploy id? Answer from the reference facts.",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("DEPLOY-777"),
        "the gaff context did not reach the model: {stdout}"
    );
}
