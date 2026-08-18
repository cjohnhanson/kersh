//! The gaff subprocess seam, driven against a fake `gaff` binary that
//! records what it received. No model, no real gaff.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt as _;

use kersh::gaff::{Gaff, Outcome};

/// Write an executable fake `gaff` that records its argv, stdin, and env
/// to `rec`, then behaves as `mode` says.
fn fake_gaff(dir: &std::path::Path, rec: &std::path::Path, mode: &str) -> std::path::PathBuf {
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > {rec}/argv\n\
         cat > {rec}/stdin\n\
         env > {rec}/env\n\
         {mode}\n",
        rec = rec.display(),
        mode = mode,
    );
    // Sync and close the script before chmod and exec. A plain write that
    // is exec'd at once can flake on CI: the spawn fails to find or read
    // the not-yet-durable file, gaff records nothing, and a later read of
    // the recording panics. sync_all makes the content durable first.
    use std::io::Write as _;
    let path = dir.join("gaff");
    {
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn gaff_with(binary: &std::path::Path) -> Gaff {
    Gaff {
        binary: binary.to_string_lossy().into_owned(),
        session_id: "abc123".to_owned(),
        profile: "reviewer".to_owned(),
    }
}

#[tokio::test]
async fn session_start_allows_and_parses_the_context() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec");
    std::fs::create_dir(&rec).unwrap();
    let bin = fake_gaff(
        dir.path(),
        &rec,
        r#"printf '{"event":"session_start","context":"HOUSE RULES"}'; exit 0"#,
    );
    let outcome = gaff_with(&bin).hook("session_start", None).await;
    assert_eq!(outcome, Outcome::Allow("HOUSE RULES".to_owned()));

    // The payload and the environment carried gaff's own vocabulary.
    let payload = std::fs::read_to_string(rec.join("stdin")).unwrap();
    assert!(
        payload.contains("\"gaff_event\":\"session_start\""),
        "{payload}"
    );
    assert!(payload.contains("\"session_id\":\"abc123\""), "{payload}");
    let env = std::fs::read_to_string(rec.join("env")).unwrap();
    assert!(env.contains("GAFF_HOST=generic"), "{env}");
    assert!(env.contains("GAFF_PROFILE=reviewer"), "{env}");
    assert!(env.contains("GAFF_SESSION_ID=abc123"), "{env}");
    let argv = std::fs::read_to_string(rec.join("argv")).unwrap();
    assert_eq!(argv.trim(), "hook");
}

#[tokio::test]
async fn a_pre_tool_call_carries_the_tool_and_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec");
    std::fs::create_dir(&rec).unwrap();
    let bin = fake_gaff(dir.path(), &rec, "exit 0");
    let _ = gaff_with(&bin)
        .hook(
            "pre_tool_call",
            Some(("read_file", r#"{"path":"/x/.env"}"#)),
        )
        .await;
    let payload = std::fs::read_to_string(rec.join("stdin")).unwrap();
    // The event name is the enforcement contract: gaff runs a guard only
    // for the event it recognizes. Pin it so a rename fails a test, not
    // silently a guard.
    assert!(
        payload.contains("\"gaff_event\":\"pre_tool_call\""),
        "{payload}"
    );
    assert!(payload.contains("\"tool_name\":\"read_file\""), "{payload}");
    assert!(payload.contains("\"path\":\"/x/.env\""), "{payload}");
}

#[tokio::test]
async fn each_enforcement_event_carries_its_exact_name() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec");
    std::fs::create_dir(&rec).unwrap();
    let bin = fake_gaff(dir.path(), &rec, "exit 0");
    let gaff = gaff_with(&bin);
    for (event, tool) in [
        ("tool_call", None),
        ("stop", None),
        ("pre_tool_call", Some(("list", "{}"))),
    ] {
        let _ = gaff.hook(event, tool).await;
        let payload = std::fs::read_to_string(rec.join("stdin")).unwrap();
        assert!(
            payload.contains(&format!("\"gaff_event\":\"{event}\"")),
            "{event}: {payload}"
        );
    }
}

#[tokio::test]
async fn exit_two_is_a_refusal_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec");
    std::fs::create_dir(&rec).unwrap();
    let bin = fake_gaff(
        dir.path(),
        &rec,
        "echo 'that path holds secrets' 1>&2; exit 2",
    );
    let outcome = gaff_with(&bin)
        .hook("pre_tool_call", Some(("read_file", "{}")))
        .await;
    match outcome {
        Outcome::Refuse(reason) => assert!(reason.contains("secrets"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_unexpected_exit_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec");
    std::fs::create_dir(&rec).unwrap();
    let bin = fake_gaff(dir.path(), &rec, "exit 1");
    let outcome = gaff_with(&bin)
        .hook("pre_tool_call", Some(("read_file", "{}")))
        .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
}

#[tokio::test]
async fn an_absent_binary_fails_closed() {
    let gaff = Gaff {
        binary: "/nonexistent/gaff".to_owned(),
        session_id: "abc123".to_owned(),
        profile: "reviewer".to_owned(),
    };
    assert!(matches!(
        gaff.hook("session_start", None).await,
        Outcome::Failed(_)
    ));
}
