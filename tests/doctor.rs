//! `kersh doctor` end to end, against fake tools on the login `PATH`:
//! the JSON shape of every line, the guard states from the probe
//! results, and the exit code for a missing required piece.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use serde_json::Value;
use support::{Env, stderr, write_exe};

fn lines(output: &std::process::Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("not JSON ({error}): {line}"))
        })
        .collect()
}

fn find<'a>(items: &'a [Value], area: &str, check: &str) -> &'a Value {
    items
        .iter()
        .find(|item| item["area"] == area && item["check"] == check)
        .unwrap_or_else(|| panic!("no {area} {check} in {items:#?}"))
}

fn fake(env: &Env, name: &str, version: &str) {
    write_exe(
        &env.bin().join(name),
        &format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '{version}'\nexit 0\n"),
    );
}

/// Every fake tool, and gaff's hooks registered for both harnesses.
fn wired() -> Env {
    let env = Env::new();
    fake(&env, "gaff", "gaff 9.9.9");
    fake(&env, "kersh", "kersh 0.8.0");
    for name in ["zettel", "almanac", "tisket", "tundish"] {
        fake(&env, name, &format!("{name} 1.0.0"));
    }
    std::fs::create_dir_all(env.home().join(".claude")).unwrap();
    std::fs::write(
        env.home().join(".claude/settings.json"),
        r#"{"autoMemoryEnabled": false, "hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "gaff hook"}]}]}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(env.home().join(".codex")).unwrap();
    std::fs::write(
        env.home().join(".codex/hooks.json"),
        r#"{"hooks": {"PreToolUse": [{"hooks": [{"type": "command", "command": "gaff hook --host codex"}]}]}}"#,
    )
    .unwrap();
    env
}

#[test]
fn every_line_has_the_same_shape() {
    let env = wired();
    let output = env.kersh(&["doctor", "--json"]);
    let items = lines(&output);
    assert!(items.len() > 10, "{items:#?}");
    for item in &items {
        for key in ["area", "check", "state", "detail"] {
            assert!(item[key].is_string(), "{key} in {item}");
        }
        assert!(item["required"].is_boolean(), "{item}");
    }
    let claude = find(&items, "harness", "claude-code");
    assert_eq!(claude["state"], "ok");
    assert_eq!(claude["version"], "2.1.999 (Claude Code)");
    assert_eq!(
        find(&items, "harness", "codex")["version"],
        "codex-cli 0.999.0"
    );
    assert_eq!(find(&items, "path", "gaff")["version"], "gaff 9.9.9");
    assert_eq!(find(&items, "path", "kersh")["required"], true);
    assert_eq!(find(&items, "host", "host")["state"], "stopped");
    assert_eq!(find(&items, "host", "session server")["state"], "stopped");
    // The environment's config.yml says `service_manager: none`.
    let unit = find(&items, "service", "kersh-host");
    assert_eq!(unit["state"], "not_applicable");
    assert_eq!(unit["required"], false);
    for tool in ["zettel", "almanac", "tisket", "tundish"] {
        assert_eq!(find(&items, "eng.rip", tool)["state"], "ok");
    }
    assert_eq!(
        find(&items, "eng.rip", "section memory-howto")["state"],
        "missing"
    );
    assert_eq!(
        find(&items, "eng.rip", "claude-code auto memory off")["state"],
        "ok"
    );
}

#[test]
fn a_wired_machine_passes_and_names_each_guard() {
    let env = wired();
    let output = env.kersh(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let items = lines(&output);
    assert_eq!(find(&items, "gaff", "claude-code hooks")["state"], "ok");
    assert_eq!(find(&items, "gaff", "codex hooks")["state"], "ok");
    assert_eq!(find(&items, "guard", "rig headless")["state"], "guarded");
    assert_eq!(
        find(&items, "guard", "claude-code headless")["state"],
        "guarded"
    );
    assert_eq!(
        find(&items, "guard", "claude-code interactive")["state"],
        "guarded"
    );
    let codex = find(&items, "guard", "codex headless");
    assert_eq!(codex["state"], "unconfirmed");
    assert!(
        codex["detail"].as_str().unwrap().contains("hook trust"),
        "{codex}"
    );
    assert_eq!(
        find(&items, "guard", "codex interactive")["state"],
        "unguarded"
    );

    let text = env.kersh(&["doctor"]);
    assert!(text.status.success(), "{}", stderr(&text));
    let shown = String::from_utf8_lossy(&text.stdout);
    assert!(shown.contains("unconfirmed"), "{shown}");
    assert!(shown.contains("codex interactive"), "{shown}");
    let unit = shown
        .lines()
        .find(|line| line.contains("service: kersh-host") || line.contains("service  kersh-host"))
        .unwrap_or_else(|| panic!("no unit line: {shown}"));
    assert!(unit.starts_with("not_applicable "), "{unit}");
}

/// `host.lock` held as a running host holds it. The pid it names is a
/// `sleep`, because the environment's cleanup signals that pid.
struct HeldLock {
    _file: std::fs::File,
    sleeper: std::process::Child,
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = self.sleeper.kill();
        let _ = self.sleeper.wait();
    }
}

/// Hold `host.lock` as a running host would, naming `version`.
fn hold_host_lock(env: &Env, version: &str) -> HeldLock {
    std::fs::create_dir_all(env.state()).unwrap();
    let sleeper = std::process::Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let path = env.state().join("host.lock");
    std::fs::write(
        &path,
        serde_json::json!({
            "pid": sleeper.id(),
            "entry": ["kersh"],
            "version": version,
            "schema": 1,
        })
        .to_string(),
    )
    .unwrap();
    let file = std::fs::File::open(&path).unwrap();
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    HeldLock {
        _file: file,
        sleeper,
    }
}

#[test]
fn a_host_of_another_version_fails_the_report() {
    let env = wired();
    let _lock = hold_host_lock(&env, "0.0.0-other");
    let output = env.kersh(&["doctor", "--json"]);
    let items = lines(&output);
    let host = find(&items, "host", "host");
    assert_eq!(host["state"], "mismatch", "{host}");
    assert_eq!(host["required"], true, "{host}");
    assert_eq!(host["version"], "0.0.0-other", "{host}");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[test]
fn a_host_of_this_version_passes() {
    let env = wired();
    let _lock = hold_host_lock(&env, env!("CARGO_PKG_VERSION"));
    let output = env.kersh(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(find(&lines(&output), "host", "host")["state"], "ok");
}

/// Every path under `root` with its size, mode, modification time, and
/// contents.
fn tree(
    root: &std::path::Path,
) -> Vec<(std::path::PathBuf, u64, u32, std::time::SystemTime, Vec<u8>)> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let contents = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if meta.is_dir() {
                stack.push(path.clone());
            }
            found.push((
                path,
                meta.len(),
                meta.permissions().mode(),
                meta.modified().unwrap(),
                contents,
            ));
        }
    }
    found.sort();
    found
}

#[test]
fn doctor_writes_nothing() {
    let env = wired();
    let root = env.home().parent().unwrap().to_path_buf();
    let before = tree(&root);
    let output = env.kersh(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = env.kersh(&["doctor"]);
    assert!(text.status.success(), "{}", stderr(&text));
    let after = tree(&root);
    let paths = |list: &[(std::path::PathBuf, u64, u32, std::time::SystemTime, Vec<u8>)]| {
        list.iter().map(|entry| entry.0.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        paths(&before),
        paths(&after),
        "doctor added or removed a path"
    );
    assert!(before == after, "doctor changed a file");
}

#[test]
fn a_missing_required_piece_exits_1() {
    let env = Env::new();
    let output = env.kersh(&["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let items = lines(&output);
    for (area, check) in [
        ("path", "kersh"),
        ("path", "gaff"),
        ("gaff", "claude-code hooks"),
        ("gaff", "codex hooks"),
    ] {
        let item = find(&items, area, check);
        assert_eq!(item["state"], "missing", "{item}");
        assert_eq!(item["required"], true, "{item}");
    }
    assert_eq!(
        find(&items, "guard", "claude-code headless")["state"],
        "unguarded"
    );
    assert_eq!(find(&items, "guard", "rig headless")["state"], "unguarded");

    let text = env.kersh(&["doctor"]);
    assert_eq!(text.status.code(), Some(1));
    assert!(stderr(&text).contains("required"), "{}", stderr(&text));
}

#[test]
fn a_missing_harness_is_not_required() {
    let env = wired();
    std::fs::remove_file(env.bin().join("codex")).unwrap();
    let output = env.kersh(&["doctor", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let items = lines(&output);
    let codex = find(&items, "harness", "codex");
    assert_eq!(codex["state"], "missing");
    assert_eq!(codex["required"], false);
    assert!(
        !items
            .iter()
            .any(|item| item["area"] == "guard" && item["check"] == "codex headless"),
        "a guard line for a harness that is not installed"
    );
}
