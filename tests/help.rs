//! `--help` snapshots for the top level and every subcommand.
//!
//! Each case runs the built binary with a cleared environment, a synthetic
//! `HOME`, and `COLUMNS=100`, so the text does not depend on the machine.
//! The output must equal `tests/snapshots/help/<command>.txt`. Set
//! `KERSH_UPDATE_SNAPSHOTS=1` to rewrite the snapshots after a deliberate
//! help-text change, then read the diff.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

/// The subcommands, in the order the top-level help lists them. `tui`
/// exists only in a build with the `tui` feature.
#[rustfmt::skip]
const COMMANDS: &[&str] = &[
    "init", "list", "show", "check", "render", "run", "docs", "prime", "ps", "start", "attach",
    "stop", "resume", "restart", "send", "events", "screen", "rm", "harness", "agent", "runtime",
    "setup", "doctor", "service", "memories", "serve",
    #[cfg(feature = "tui")]
    "tui",
    "host",
];

/// The top-level help's snapshot: a build without the TUI lists one
/// command less.
const TOP: &str = if cfg!(feature = "tui") {
    "kersh"
} else {
    "kersh-no-tui"
};

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join("help")
}

/// The help text for `args`, run with a hermetic environment.
fn help(args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kersh"))
        .args(args)
        .arg("--help")
        .env_clear()
        .env("HOME", home.path())
        .env("COLUMNS", "100")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "`kersh {} --help` exited {:?}: {}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Compare `actual` with the snapshot `name`, or rewrite it on request.
fn check(name: &str, actual: &str) -> Option<String> {
    let path = snapshot_dir().join(format!("{name}.txt"));
    if std::env::var_os("KERSH_UPDATE_SNAPSHOTS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(snapshot_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return None;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    (expected != actual).then(|| {
        format!(
            "{} differs from the help output.\n--- expected\n{expected}\n--- actual\n{actual}",
            path.display()
        )
    })
}

#[test]
fn help_matches_the_snapshots() {
    let mut failures = Vec::new();
    failures.extend(check(TOP, &help(&[])));
    for command in COMMANDS {
        failures.extend(check(command, &help(&[command])));
    }
    assert!(
        failures.is_empty(),
        "{}\n\nAfter a deliberate change: KERSH_UPDATE_SNAPSHOTS=1 cargo test --all-features --test help",
        failures.join("\n\n")
    );
}

#[test]
fn every_subcommand_has_a_snapshot_and_appears_in_the_top_level_help() {
    let top = help(&[]);
    for command in COMMANDS {
        assert!(
            top.contains(&format!("  {command} ")),
            "the top-level help must list `{command}`:\n{top}"
        );
    }
}
