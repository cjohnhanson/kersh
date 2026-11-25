//! Restore end to end: sessions on a real session server come back after
//! `kill -9` of the server, after one session vanishes, and after
//! `kersh restart --mux`, each by its harness's own resume or held.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::{CODEX_CONVERSATION, Env, alive, stderr};

const RESUME_PROMPT: &str = "run the wait again";

/// Add the `worker` runtime: its resume prompt is [`RESUME_PROMPT`].
fn add_worker(env: &Env, harness: &str) {
    let file = env.work().join(format!("{harness}.yml"));
    std::fs::write(
        &file,
        format!("harness: {harness}\nresume_prompt: {RESUME_PROMPT}\n"),
    )
    .unwrap();
    let name = format!("{harness}-worker");
    let output = env.kersh(&["runtime", "add", &name, file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
}

/// Stop the host, `kill -9` the server, start the host again.
fn crash_and_restart(env: &Env) {
    env.stop_host();
    env.kill_mux_hard();
    env.start_host();
}

fn restore_line(record: &Value) -> &Value {
    &record["restores"].as_array().unwrap().last().unwrap()["line"]
}

#[test]
fn a_server_killed_then_a_host_restart_resumes_claude_and_codex_with_the_resume_prompt() {
    let env = Env::new();
    add_worker(&env, "claude-code");
    add_worker(&env, "codex");
    let claude = env.start(&["--runtime", "claude-code-worker"], "wait");
    let codex = env.start(&["--runtime", "codex-worker"], "go");
    env.running_with_pid(&claude);
    env.wait(&codex, "the rollout id", |r| r["conversation"].is_object());
    let uuid = env.record(&claude)["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    crash_and_restart(&env);

    let record = env.wait_restored(&claude, 1, "running");
    assert_eq!(record["launches"], 2);
    let line = restore_line(&record).as_str().unwrap();
    assert!(line.starts_with("restored at "), "{line}");
    assert!(
        line.contains("tools that were running did not finish"),
        "{line}"
    );
    let args = env.launch_args(&claude, 2);
    assert_eq!(
        args.get(..2).unwrap(),
        ["--resume", uuid.as_str()],
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--settings", r#"{"autoMemoryEnabled": false}"#]),
        "{args:?}"
    );
    assert_eq!(&args[args.len() - 2..], ["--", RESUME_PROMPT], "{args:?}");
    let screen = env.kersh(&["screen", &claude, "--json"]);
    let screen: Value = serde_json::from_slice(&screen.stdout).unwrap();
    assert_eq!(screen["cols"], 100, "the pane keeps its recorded size");

    let record = env.wait_restored(&codex, 1, "running");
    assert_eq!(record["conversation"]["id"], CODEX_CONVERSATION);
    let args = env.launch_args(&codex, 2);
    assert_eq!(
        &args[args.len() - 3..],
        ["resume", CODEX_CONVERSATION, RESUME_PROMPT],
        "{args:?}"
    );
}

#[test]
fn a_missing_transcript_holds_and_ret_runs_the_fresh_command_gated_like_resume() {
    let env = Env::new();
    let id = env.start(
        &[
            "--harness",
            "claude-code",
            "--group",
            "g",
            "--max-running",
            "1",
        ],
        "notranscript",
    );
    env.running_with_pid(&id);
    let uuid = env.record(&id)["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    crash_and_restart(&env);

    let record = env.wait_restored(&id, 1, "held");
    let line = restore_line(&record).as_str().unwrap();
    assert!(line.starts_with("held at "), "{line}");
    assert!(line.contains("no transcript"), "{line}");
    env.wait_screen(&id, "RET runs");
    assert_eq!(env.launches(&id).len(), 1, "nothing ran on restore");

    // RET goes through the host, which gates it like a resume.
    let other = env.start(&["--harness", "claude-code", "--group", "g"], "wait");
    env.running_with_pid(&other);
    env.type_into(&id, "\r");
    env.wait_screen(&id, "1 of 1");
    assert_eq!(env.record(&id)["state"], "held");

    assert!(env.kersh(&["stop", &other]).status.success());
    env.type_into(&id, "\r");
    let record = env.running_with_pid(&id);
    assert_eq!(record["launches"], 3);
    let args = env.launch_args(&id, 2);
    assert!(
        args.windows(2)
            .any(|w| w == ["--session-id", uuid.as_str()]),
        "{args:?}"
    );
    assert_eq!(&args[args.len() - 2..], ["--", "notranscript"]);
}

#[test]
fn a_headless_session_comes_back_held() {
    let env = Env::new();
    let ran = env.start(&["--harness", "claude-code", "--mode", "headless"], "hang");
    let fresh = env.start(
        &["--harness", "claude-code", "--mode", "headless"],
        "hang notranscript",
    );
    env.running_with_pid(&ran);
    env.running_with_pid(&fresh);

    crash_and_restart(&env);

    for id in [&ran, &fresh] {
        let record = env.wait_restored(id, 1, "held");
        assert_eq!(env.launches(id).len(), 1, "a headless run is not resumed");
        let text = env.wait_screen(id, "--prompt-file");
        assert!(text.contains(&format!("kersh resume {id}")), "{text}");
        assert!(record["reason"].as_str().unwrap().contains("headless"));
    }
    let text = env.wait_screen(&fresh, "RET");
    assert!(text.contains("first prompt again"), "{text}");
    let text = env.wait_screen(&ran, "--prompt-file");
    assert!(
        !text.contains("RET"),
        "a turn ran, so RET offers nothing: {text}"
    );
}

#[test]
fn an_ended_record_is_not_restored() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "exit0");
    env.wait(&id, "ended", |r| r["state"] == "ended");
    crash_and_restart(&env);
    // Give a restore every chance to happen.
    std::thread::sleep(Duration::from_secs(2));
    let record = env.record(&id);
    assert_eq!(record["state"], "ended");
    assert_eq!(record["restores"], serde_json::json!([]));
    assert_eq!(env.launches(&id).len(), 1);
}

#[test]
fn a_session_killed_alone_is_restored_without_a_host_restart() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    let host = env.host_pid().unwrap();
    let mux = env.mux_pid().unwrap();
    env.kill_session(&id);
    let record = env.wait_restored(&id, 1, "running");
    assert_eq!(record["launches"], 2);
    assert_eq!(env.host_pid(), Some(host), "the same host saw it");
    assert_eq!(env.mux_pid(), Some(mux), "the server stayed up");
    let args = env.launch_args(&id, 2);
    assert!(args.contains(&"--resume".to_owned()), "{args:?}");
}

#[test]
fn a_server_killed_under_a_live_host_is_seen_and_restored() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    let host = env.host_pid().unwrap();
    let old = env.kill_mux_hard();
    let record = env.wait_restored(&id, 1, "running");
    assert!(
        restore_line(&record)
            .as_str()
            .unwrap()
            .contains("the session server stopped"),
        "{record:#}"
    );
    assert_eq!(env.host_pid(), Some(host));
    assert_ne!(env.mux_pid(), Some(old));
}

#[test]
fn two_restores_never_put_two_processes_on_one_conversation() {
    let env = Env::new();
    let first = env.start(&["--harness", "claude-code"], "wait");
    let record = env.running_with_pid(&first);
    let uuid = record["conversation"]["id"].as_str().unwrap().to_owned();

    // A second record on the same conversation, as a copy of the first.
    let second = "bbbbbbbbbbbb";
    let from = env.state().join("sessions").join(&first);
    let to = env.state().join("sessions").join(second);
    std::fs::create_dir_all(&to).unwrap();
    std::fs::copy(from.join("launch.json"), to.join("launch.json")).unwrap();
    let mut copy = record.clone();
    copy["id"] = second.into();
    copy["rmux_session"] = format!("kersh-{second}").into();
    copy.as_object_mut().unwrap().remove("progress");
    std::fs::write(to.join("record.json"), copy.to_string()).unwrap();

    crash_and_restart(&env);

    let deadline = Instant::now() + Duration::from_secs(40);
    let (a, b) = loop {
        let a = env.record(&first);
        let b = env.record(second);
        let settled = |r: &Value| r["restores"].as_array().is_some_and(|l| l.len() == 1);
        if settled(&a) && settled(&b) && (a["state"] == "running" || b["state"] == "running") {
            break (a, b);
        }
        assert!(
            Instant::now() < deadline,
            "{a:#}\n{b:#}\n{}",
            env.host_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let states = [a["state"].as_str().unwrap(), b["state"].as_str().unwrap()];
    assert!(
        states == ["running", "held"] || states == ["held", "running"],
        "{states:?}"
    );
    let held = if a["state"] == "held" { &a } else { &b };
    assert!(
        held["reason"]
            .as_str()
            .unwrap()
            .contains("already holds conversation"),
        "{held:#}"
    );
    env.running_with_pid(if a["state"] == "running" {
        &first
    } else {
        second
    });
    assert_eq!(
        env.live_on(&uuid).len(),
        1,
        "one process on the conversation"
    );

    // RET on the held one is refused: the other holds the conversation.
    let (held_id, running_id) = if a["state"] == "held" {
        (first.as_str(), second)
    } else {
        (second, first.as_str())
    };
    env.type_into(held_id, "\r");
    env.wait_screen(held_id, "RET refused");
    let text = env.wait_screen_flat(held_id, "already holds conversation");
    assert!(text.contains(running_id), "{text}");
    assert_eq!(env.record(held_id)["state"], "held");
    assert_eq!(env.live_on(&uuid).len(), 1);

    // A second host restart restores nothing that still runs.
    env.stop_host();
    env.start_host();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(env.live_on(&uuid).len(), 1);
}

#[test]
fn a_process_left_in_the_old_group_holds_the_session_and_refuses_ret_and_resume() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "survivor");
    env.running_with_pid(&id);
    let survivor = env.survivor(&id);

    crash_and_restart(&env);

    // The restore holds it: a process of the old group still runs.
    let record = env.wait_restored(&id, 1, "held");
    assert!(alive(survivor), "the survivor outlived the server");
    let pgid = record["pgid"].as_i64().unwrap();
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("process group {pgid}")),
        "{reason}"
    );
    assert_eq!(env.launches(&id).len(), 1, "nothing ran on restore");
    env.wait_screen(&id, "RET resumes it");

    // RET is refused while the group runs.
    env.type_into(&id, "\r");
    env.wait_screen(&id, "RET refused");
    let text = env.wait_screen_flat(&id, "end it first");
    assert!(text.contains(&format!("kill -- -{pgid}")), "{text}");
    assert_eq!(env.record(&id)["state"], "held");
    assert_eq!(env.launches(&id).len(), 1);

    // `kersh resume` is refused, and the held pane is left alone.
    let rmux = record["rmux_session"].clone();
    let output = env.kersh(&["resume", &id]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("kill -- -{pgid}")),
        "{}",
        stderr(&output)
    );
    std::thread::sleep(Duration::from_secs(2));
    let after = env.record(&id);
    assert_eq!(after["state"], "held");
    assert_eq!(after["rmux_session"], rmux);
    assert_eq!(after["restores"].as_array().unwrap().len(), 1, "{after:#}");
    assert_eq!(after["launches"], record["launches"]);
    assert_eq!(env.launches(&id).len(), 1, "no new launch");
    env.wait_screen(&id, "RET resumes it");
}

#[test]
fn a_restore_while_the_dead_pane_s_wrapper_still_runs_waits_on_the_group() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "hupsurvivor");
    let before = env.running_with_pid(&id);
    let survivor = env.survivor(&id);
    let pgid = before["pgid"].as_i64().unwrap();

    // The harness ignores the hang-up, so its wrapper outlives the pane
    // for a while; the watcher restores inside that time.
    env.kill_mux_hard();

    let record = env.wait_restored(&id, 1, "held");
    assert!(alive(survivor), "the survivor outlived the server");
    assert_eq!(record["pgid"].as_i64(), Some(pgid), "{record:#}");
    let reason = record["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("process group {pgid}")),
        "{reason}"
    );
    assert_eq!(
        env.launches(&id).len(),
        1,
        "nothing ran beside the survivor"
    );

    let output = env.kersh(&["resume", &id]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("kill -- -{pgid}")),
        "{}",
        stderr(&output)
    );
    assert_eq!(env.launches(&id).len(), 1, "no launch beside the survivor");
}

#[test]
fn a_session_that_exits_while_the_host_is_stopped_stays_ended() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "exitlater");
    let record = env.running_with_pid(&id);
    let harness = i32::try_from(record["harness_pid"].as_i64().unwrap()).unwrap();
    env.stop_host();
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(harness) {
        assert!(Instant::now() < deadline, "the fake never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The wrapper writes its note and the pane closes.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        env.record(&id)["state"],
        "running",
        "nothing folded the exit"
    );

    env.start_host();
    std::thread::sleep(Duration::from_secs(2));
    let record = env.record(&id);
    assert_eq!(record["state"], "ended", "{record:#}");
    assert_eq!(record["restores"], serde_json::json!([]));
    assert_eq!(env.launches(&id).len(), 1);
}

#[test]
fn one_unreadable_record_does_not_stop_a_restore() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    let bad = env.state().join("sessions").join("cccccccccccc");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("record.json"), "{not json").unwrap();

    // The first host's watcher may name the file too; only the log of
    // the host that restores is counted.
    env.stop_host();
    let earlier = env.host_log().len();
    env.kill_mux_hard();
    env.start_host();

    env.wait_restored(&id, 1, "running");
    // The watcher ticks many times; the bad file is named once.
    std::thread::sleep(Duration::from_secs(2));
    let log = env.host_log();
    let lines: Vec<&str> = log[earlier..]
        .lines()
        .filter(|line| line.contains("cccccccccccc/record.json"))
        .collect();
    assert_eq!(lines.len(), 1, "{log}");
    assert!(lines[0].contains("fix or remove"), "{log}");
}

#[test]
fn an_interactive_session_held_after_a_failure_resumes() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "exit3");
    env.wait(&id, "held", |r| {
        r["state"] == "held" && r["wrapper_pid"].is_i64() && r["pgid"].is_i64()
    });
    let output = env.kersh(&["resume", &id]);
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.running_with_pid(&id);
    assert_eq!(record["launches"], 2);
    assert_eq!(env.launches(&id).len(), 2);
}

#[test]
fn a_restart_whose_old_group_outlives_the_wait_is_refused() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "survivor");
    let before = env.running_with_pid(&id);
    let survivor = env.survivor(&id);
    let pgid = before["pgid"].as_i64().unwrap();

    let output = env.kersh(&["restart", &id]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(&format!("kill -- -{pgid}")),
        "{}",
        stderr(&output)
    );
    assert!(alive(survivor));
    // Refused after the pane ended: nothing launched, and the record is
    // the running record it was until a restore finds its pane gone.
    assert_eq!(env.launches(&id).len(), 1, "no new launch");
    let after = env.record(&id);
    assert_eq!(after["launches"], 1, "{after:#}");
}

#[test]
fn restore_false_leaves_a_held_session_whose_pane_vanished() {
    let env = Env::with_config("restore: false\n");
    let id = env.start(&["--harness", "claude-code"], "exit3");
    env.wait(&id, "held", |r| r["state"] == "held");
    env.kill_session(&id);
    env.stop_host();
    env.start_host();
    std::thread::sleep(Duration::from_secs(2));
    let record = env.record(&id);
    assert_eq!(record["state"], "held");
    assert_eq!(record["restores"], serde_json::json!([]));
    let screen = env.kersh(&["screen", &id]);
    assert!(!screen.status.success(), "the pane came back");
}

#[test]
fn a_changed_build_is_noted_in_the_restore_line() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    env.stop_host();
    env.kill_mux_hard();
    std::fs::write(env.home().join("claude-version"), "2.2.0 (Claude Code)\n").unwrap();
    env.start_host();
    let record = env.wait_restored(&id, 1, "running");
    let line = restore_line(&record).as_str().unwrap();
    assert!(
        line.contains("2.1.999 (Claude Code)") && line.contains("2.2.0 (Claude Code)"),
        "{line}"
    );
    assert_eq!(record["build"]["version"], "2.2.0 (Claude Code)");
}

#[test]
fn restore_false_leaves_a_vanished_session_lost() {
    let env = Env::with_config("restore: false\n");
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    crash_and_restart(&env);
    let record = env.wait(&id, "lost", |r| r["state"] == "lost");
    assert_eq!(record["restores"], serde_json::json!([]));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(env.record(&id)["state"], "lost");
    assert_eq!(env.launches(&id).len(), 1);
}

#[test]
fn a_server_of_another_wire_version_is_refused_until_restart_mux() {
    let env = Env::new();
    let id = env.start(&["--harness", "claude-code"], "wait");
    let before = env.running_with_pid(&id);
    let harness = i32::try_from(before["harness_pid"].as_i64().unwrap()).unwrap();
    env.stop_host();
    let lock = env.state().join("mux.lock");
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    value["wire_version"] = 999.into();
    value["version"] = "0.0.1".into();
    std::fs::write(&lock, value.to_string()).unwrap();
    let old = env.mux_pid().unwrap();
    env.start_host();

    let output = env.kersh(&["stop", &id]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("`kersh restart --mux` restarts it and restores every session"),
        "{}",
        stderr(&output)
    );
    let record = env.record(&id);
    assert_eq!(record["state"], "running", "reads still work from records");
    assert!(alive(harness), "the agent is left running");

    let output = env.kersh(&["restart", "--mux", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let record = env.wait_restored(&id, 1, "running");
    assert!(
        restore_line(&record)
            .as_str()
            .unwrap()
            .contains("kersh restart --mux"),
        "{record:#}"
    );
    assert!(!alive(old), "the old server was stopped");
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    assert_ne!(lock["wire_version"], 999);
    assert!(env.kersh(&["stop", &id]).status.success());
}

#[test]
fn the_last_capture_is_printed_above_the_resumed_agent() {
    let env = Env::with_config("capture_seconds: 1\n");
    let id = env.start(&["--harness", "claude-code"], "wait");
    env.running_with_pid(&id);
    env.wait_screen(&id, "fake claude waiting");
    env.type_into(&id, "before the crash");
    env.wait_screen(&id, "before the crash");
    let capture = env.state().join("sessions").join(&id).join("capture.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&capture).is_ok_and(|text| text.contains("before the crash")) {
        assert!(Instant::now() < deadline, "no capture:\n{}", env.host_log());
        std::thread::sleep(Duration::from_millis(100));
    }

    env.kill_mux_hard();
    env.wait_restored(&id, 1, "running");
    env.wait_screen(&id, "restored at");
    let deadline = Instant::now() + Duration::from_secs(20);
    let text = loop {
        let text = env.wait_screen(&id, "restored at");
        let after = text.split("restored at").nth(1).unwrap_or_default();
        if after.contains("fake claude waiting") {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "the agent never came back: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let lines: Vec<&str> = text.lines().collect();
    let captured = lines
        .iter()
        .position(|l| l.contains("before the crash"))
        .unwrap();
    let restored = lines
        .iter()
        .position(|l| l.contains("restored at"))
        .unwrap();
    let waiting = lines
        .iter()
        .rposition(|l| l.contains("fake claude waiting"))
        .unwrap();
    assert!(captured < restored && restored < waiting, "{text}");
}
