//! rig, the built-in harness: `kersh run --output jsonl`, the refusal of
//! an agent on another harness, and rig agents in sessions. The fake
//! model plays a script from `FAKE_MODEL_SCRIPT`; no real model runs.

#![cfg(feature = "fake-model")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::{Env, stderr};

/// A tool call, then the final text.
const TOOL_THEN_TEXT: &str = r#"[{"kind":"tool","name":"bash","args":{"command":"echo hi"}},{"kind":"text","text":"all done"}]"#;

/// A tool call that runs for a while, so the session stays running.
const SLOW: &str = r#"[{"kind":"tool","name":"bash","args":{"command":"sleep 25"}},{"kind":"text","text":"late"}]"#;

fn write_agent(root: &Path, name: &str, extra: &str) {
    std::fs::write(root.join("kersh.yml"), "").unwrap();
    let dir = root.join("agents").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("AGENT.md"),
        format!(
            "---\nname: {name}\nmodel: fake/scripted\nmax_turns: 4\n{extra}---\nYou do one task.\n"
        ),
    )
    .unwrap();
}

fn lines(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

fn run(root: &Path, args: &[&str], script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kersh"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("FAKE_MODEL_SCRIPT", script)
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn run_output_jsonl_streams_each_turn_tool_call_and_result_then_the_final_text() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "worker", "");
    let output = run(
        tmp.path(),
        &["run", "worker", "--output", "jsonl", "go"],
        TOOL_THEN_TEXT,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let events = lines(&output);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "init",
            "turn",
            "tool",
            "tool_result",
            "turn",
            "text",
            "result"
        ]
    );
    assert_eq!(events[0]["agent"], "worker");
    assert_eq!(events[0]["model"], "fake/scripted");
    assert_eq!(events[1]["turn"], 1);
    assert_eq!(events[2]["tool"], "bash");
    assert_eq!(events[2]["input"], "echo hi");
    assert_eq!(events[3]["tool"], "bash");
    assert_eq!(events[3]["status"], "success");
    assert_eq!(events[3]["is_error"], false);
    assert_eq!(events[4]["turn"], 2);
    assert_eq!(events[5]["text"], "all done");
    let last = events.last().unwrap();
    assert_eq!(last["status"], "success");
    assert_eq!(last["is_error"], false);
    assert_eq!(last["text"], "all done", "the final text is the last event");

    // Without the flag, stdout is the final text only.
    let plain = run(tmp.path(), &["run", "worker", "go"], TOOL_THEN_TEXT);
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert_eq!(String::from_utf8(plain.stdout).unwrap(), "all done\n");
}

#[test]
fn run_output_jsonl_ends_a_failed_run_with_an_error_result() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "worker", "");
    let output = run(
        tmp.path(),
        &["run", "worker", "--output", "jsonl", "go"],
        "not a script",
    );
    assert_eq!(output.status.code(), Some(1));
    let events = lines(&output);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["status"], "error");
    assert_eq!(last["is_error"], true);
    assert!(
        last["text"]
            .as_str()
            .unwrap()
            .contains("not a valid script"),
        "{last}"
    );
    assert!(stderr(&output).contains("not a valid script"));
}

#[test]
fn run_reads_the_prompt_from_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "worker", "");
    std::fs::write(tmp.path().join("prompt.md"), "PROMPT-FROM-FILE-7").unwrap();
    let output = run(
        tmp.path(),
        &["run", "worker", "--prompt-file", "prompt.md"],
        r#"[{"kind":"echo_prompt"}]"#,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "PROMPT-FROM-FILE-7\n"
    );
}

#[test]
fn run_refuses_an_agent_whose_harness_is_not_rig_naming_kersh_start() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "coder", "runtime: worker\n");
    let config = tmp.path().join(".config/kersh");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("runtimes.yml"),
        "worker:\n  harness: claude-code\n",
    )
    .unwrap();
    let output = run(tmp.path(), &["run", "coder", "go"], TOOL_THEN_TEXT);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = stderr(&output);
    assert!(error.contains("claude-code"), "{error}");
    assert!(error.contains("`kersh start coder`"), "{error}");
}

/// gaff's hook agents run `kersh run` with a cleared environment, so a
/// runtime this machine does not define is not known to be another
/// harness: the run goes ahead from the agent file's model.
#[test]
fn run_goes_ahead_when_the_agent_runtime_is_not_defined_here() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "hook", "runtime: elsewhere\n");
    let output = run(tmp.path(), &["run", "hook", "go"], TOOL_THEN_TEXT);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "all done\n");
}

#[test]
fn run_goes_ahead_when_runtimes_yml_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    write_agent(tmp.path(), "hook", "runtime: elsewhere\n");
    let config = tmp.path().join(".config/kersh");
    std::fs::create_dir_all(&config).unwrap();
    for yaml in [
        "elsewhere:\n  env:\n    PATH: /tmp\n",
        "elsewhere: [not, a, runtime\n",
    ] {
        std::fs::write(config.join("runtimes.yml"), yaml).unwrap();
        let output = run(tmp.path(), &["run", "hook", "go"], TOOL_THEN_TEXT);
        assert!(output.status.success(), "{yaml}: {}", stderr(&output));
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "all done\n");
    }
}

/// A kersh root in the working directory with a rig agent on the
/// runtime `name`, whose environment carries `script`.
fn rig_agent(env: &Env, name: &str, script: Option<&str>) {
    write_agent(&env.work(), name, &format!("runtime: {name}\n"));
    let file = env.work().join(format!("{name}.yml"));
    let yaml = match script {
        Some(script) => format!("env:\n  FAKE_MODEL_SCRIPT: '{script}'\n"),
        None => String::new(),
    };
    std::fs::write(&file, yaml).unwrap();
    let output = env.kersh(&["runtime", "add", name, file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
}

fn events(env: &Env, id: &str) -> Vec<Value> {
    let output = env.kersh(&["events", id]);
    assert!(output.status.success(), "{}", stderr(&output));
    lines(&output)
}

#[test]
fn a_rig_agent_in_a_session_numbers_its_events_and_ends_on_success() {
    let env = Env::new();
    rig_agent(&env, "fixer", Some(TOOL_THEN_TEXT));
    let id = env.start(&["fixer"], "fix the test");
    let record = env.wait(&id, "ended", |r| r["state"] == "ended");
    assert_eq!(record["harness"], "rig");
    assert_eq!(record["mode"], "headless");
    assert_eq!(record["exit_status"], 0);
    assert_eq!(record["build"]["installer"], "entry");

    let all = events(&env, &id);
    let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(
        seqs,
        (1..=u64::try_from(all.len()).unwrap()).collect::<Vec<_>>()
    );
    let kinds: Vec<&str> = all.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "init",
            "turn",
            "tool",
            "tool_result",
            "turn",
            "text",
            "result"
        ]
    );
    assert_eq!(all[0]["summary"], "init: fake/scripted");
    assert_eq!(all[2]["summary"], "tool bash: echo hi");
    assert_eq!(all[2]["tool"], "bash");
    assert_eq!(all[2]["input"], "echo hi");
    assert_eq!(all[3]["summary"], "tool result: ok");
    let result = all.last().unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["raw"]["text"], "all done");
    assert_eq!(record["progress"], "result: success");
}

#[test]
fn a_rig_session_runs_the_prompt_as_the_first_turn() {
    let env = Env::new();
    rig_agent(&env, "echo", Some(r#"[{"kind":"echo_prompt"}]"#));
    let id = env.start(&["echo"], "PROMPT-31");
    env.wait(&id, "ended", |r| r["state"] == "ended");
    let all = events(&env, &id);
    assert_eq!(all.last().unwrap()["raw"]["text"], "PROMPT-31");
}

#[test]
fn a_failing_rig_run_holds() {
    let env = Env::new();
    rig_agent(&env, "broken", None);
    let id = env.start(&["broken"], "go");
    let record = env.wait(&id, "held", |r| r["state"] == "held");
    assert_eq!(record["exit_status"], 1);
    let all = events(&env, &id);
    let result = all.iter().rev().find(|e| e["kind"] == "result").unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["is_error"], true);
    let text = env.wait_screen(&id, "held");
    assert!(text.contains(&format!("kersh stop {id}")), "{text}");
}

#[test]
fn a_group_limit_applies_to_rig_starts() {
    let env = Env::new();
    rig_agent(&env, "slow", Some(SLOW));
    let first = env.start(&["slow", "--group", "g", "--max-running", "1"], "go");
    env.running_with_pid(&first);
    let output = env.kersh_in(
        &["start", "slow", "--group", "g", "--prompt-file", "-"],
        "go",
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("1 of 1"), "{}", stderr(&output));
}

#[test]
fn a_rig_start_is_refused_in_interactive_mode_and_without_an_agent() {
    let env = Env::new();
    rig_agent(&env, "fixer", Some(TOOL_THEN_TEXT));
    let output = env.kersh_in(
        &[
            "start",
            "fixer",
            "--mode",
            "interactive",
            "--prompt-file",
            "-",
        ],
        "go",
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("headless"), "{}", stderr(&output));
    let output = env.kersh_in(&["start", "--harness", "rig", "--prompt-file", "-"], "go");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("agent"), "{}", stderr(&output));
}

#[test]
fn restore_holds_a_rig_session_with_ret_run_again() {
    let env = Env::new();
    rig_agent(&env, "slow", Some(SLOW));
    let id = env.start(&["slow"], "go");
    env.running_with_pid(&id);

    env.stop_host();
    env.kill_mux_hard();
    env.start_host();

    let record = env.wait_restored(&id, 1, "held");
    assert_eq!(record["held_run"], "fresh");
    let text = env.wait_screen(&id, "RET");
    assert!(text.contains("RET runs it again"), "{text}");
    assert!(text.contains("run slow --output jsonl"), "{text}");
    assert!(!text.contains("kersh resume"), "{text}");

    // RET runs it again, in a fresh pane.
    env.type_into(&id, "\r");
    let record = env.wait(&id, "the second launch", |r| {
        r["launches"] == 3 && r["state"] == "running" && r["harness_pid"].is_i64()
    });
    assert_eq!(record["held_run"], Value::Null);
    let output = env.kersh_in(&["resume", &id, "--prompt-file", "-"], "again");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("kersh start slow"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_profiled_rig_session_runs_under_its_gaff_profile() {
    let env = Env::new();
    let seen = env.root.path().join("gaff.seen");
    support::write_exe(
        &env.bin().join("gaff"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = \"profile list\" ]; then printf 'worker\\tagent or human\\n'; exit 0; fi\n\
             cat > /dev/null\n\
             echo \"$GAFF_PROFILE\" >> \"{}\"\n\
             exit 0\n",
            seen.display()
        ),
    );
    rig_agent(&env, "fixer", Some(TOOL_THEN_TEXT));
    let output = env.kersh_in(
        &["start", "fixer", "--profile", "nope", "--prompt-file", "-"],
        "go",
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("no profile `nope`"));

    let id = env.start(&["fixer", "--profile", "worker"], "go");
    env.wait(&id, "ended", |r| r["state"] == "ended");
    let seen = std::fs::read_to_string(&seen).unwrap();
    assert!(!seen.is_empty());
    assert!(seen.lines().all(|line| line == "worker"), "{seen}");
}
