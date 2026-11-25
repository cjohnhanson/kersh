//! `kersh service install|stop` end to end, against fake `launchctl` and
//! `systemctl` scripts first on `PATH`. The fakes record each call and
//! keep a marker file for "loaded", so no test loads a real job.
//!
//! The unit's content is checked by the unit tests in `src/service.rs`;
//! these tests check the files written, the calls made, idempotence, and
//! that a client starts an installed unit instead of a one-shot job.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use support::{Env, stderr, write_exe};

/// A fake launchctl. `print` answers from the marker; `bootstrap` and
/// `bootout` of the unit set and clear it; `bootstrap` of a one-shot job
/// (the session server) runs its program arguments; `kickstart` starts
/// the host named by `FAKE_KERSH` and `FAKE_STATE`, as launchd would run
/// the unit.
const FAKE_LAUNCHCTL: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/launchctl.log"
case "$1" in
  print) [ -f "$HOME/launchd-loaded" ] || { echo "Could not find service" >&2; exit 113; } ;;
  bootstrap)
    case "$3" in
      */kersh-host.plist) : > "$HOME/launchd-loaded" ;;
      *)
        args=$(sed -n '/<key>ProgramArguments<\/key>/,/<\/array>/s/.*<string>\(.*\)<\/string>.*/\1/p' "$3")
        set -f; old=$IFS; IFS='
'
        set -- $args
        IFS=$old; set +f
        nohup "$@" >> "$HOME/fake-job.log" 2>&1 &
        ;;
    esac ;;
  bootout) case "$2" in */kersh-host) rm -f "$HOME/launchd-loaded" ;; esac ;;
  kickstart)
    [ -f "$HOME/launchd-loaded" ] || { echo "Could not find service" >&2; exit 113; }
    if [ -n "$FAKE_KERSH" ]; then
      nohup "$FAKE_KERSH" host --state "$FAKE_STATE" >> "$HOME/fake-host.log" 2>&1 &
    fi ;;
esac
exit 0
"#;

/// A fake systemctl that records each call. With the no-bus marker every
/// call fails as systemctl fails without a user manager. `enable` and
/// `disable` set and clear the enabled marker; `is-enabled` prints the
/// state as systemd does: the masked marker's text (`masked` or
/// `masked-runtime`), `enabled`, or `disabled`. A masked unit refuses
/// `enable`, `start` and `restart`; with the fails marker `start` and
/// `restart` fail as a unit whose host exits at once; otherwise they
/// start the host named by `FAKE_KERSH` and `FAKE_STATE`, enabled or
/// not, as systemd would.
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/systemctl.log"
[ -f "$HOME/no-bus" ] && { echo "Failed to connect to bus: No medium found" >&2; exit 1; }
[ "$1" = --user ] && shift
case "$1" in
  enable)
    [ -f "$HOME/systemd-masked" ] && { echo "Failed to enable unit: Unit file kersh-host.service is masked." >&2; exit 1; }
    : > "$HOME/systemd-enabled" ;;
  disable) rm -f "$HOME/systemd-enabled" ;;
  is-enabled)
    if [ -f "$HOME/systemd-masked" ]; then cat "$HOME/systemd-masked"; exit 1; fi
    if [ -f "$HOME/systemd-enabled" ]; then echo enabled; else echo disabled; exit 1; fi ;;
  start|restart)
    [ -f "$HOME/systemd-masked" ] && { echo "Unit kersh-host.service is masked." >&2; exit 1; }
    [ -f "$HOME/systemd-fails" ] && { echo "Job for kersh-host.service failed because the control process exited with error code." >&2; exit 1; }
    if [ -n "$FAKE_KERSH" ]; then
      nohup "$FAKE_KERSH" host --state "$FAKE_STATE" >> "$HOME/fake-host.log" 2>&1 &
    fi ;;
esac
exit 0
"#;

/// A fake systemd-run that records each call and runs the argv after
/// `--` in the background, as a transient unit would. With the no-bus
/// marker it fails as systemd-run fails without a user manager.
const FAKE_SYSTEMD_RUN: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/systemd-run.log"
[ -f "$HOME/no-bus" ] && { echo "Failed to connect to bus: No medium found" >&2; exit 1; }
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
shift
nohup "$@" >> "$HOME/fake-job.log" 2>&1 &
exit 0
"#;

fn env_with(manager: &str) -> Env {
    let env = Env::new();
    std::fs::write(
        env.home().join(".config/kersh/config.yml"),
        format!("service_manager: {manager}\n"),
    )
    .unwrap();
    write_exe(&env.bin().join("launchctl"), FAKE_LAUNCHCTL);
    write_exe(&env.bin().join("systemctl"), FAKE_SYSTEMCTL);
    write_exe(&env.bin().join("systemd-run"), FAKE_SYSTEMD_RUN);
    env
}

/// The calls a fake recorded since the last take, and clear them.
fn take(env: &Env, tool: &str) -> Vec<String> {
    let path = env.home().join(format!("{tool}.log"));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    text.lines().map(str::to_owned).collect()
}

fn uid() -> String {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            stderr(output)
        )
    })
}

#[test]
fn a_launchd_unit_installs_once_and_stops_cleanly() {
    let env = env_with("launchd");
    let domain = format!("gui/{}", uid());
    let plist = env.home().join("Library/LaunchAgents/kersh-host.plist");

    let first = env.kersh(&["service", "install", "--json"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(json(&first)["changed"], true);
    let text = std::fs::read_to_string(&plist).unwrap();
    assert!(text.contains("<string>kersh-host</string>"), "{text}");
    assert!(text.contains("<string>host</string>"), "{text}");
    assert!(
        text.contains(&format!("<string>{}</string>", env.state().display())),
        "{text}"
    );
    assert_eq!(
        take(&env, "launchctl"),
        [
            format!("print {domain}/kersh-host"),
            format!("bootstrap {domain} {}", plist.display()),
        ]
    );

    let again = env.kersh(&["service", "install", "--json"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(json(&again)["changed"], false);
    assert_eq!(
        take(&env, "launchctl"),
        [format!("print {domain}/kersh-host")]
    );

    let stop = env.kersh(&["service", "stop", "--json"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(json(&stop)["installed"], false);
    assert!(!plist.exists());
    assert_eq!(
        take(&env, "launchctl"),
        [
            format!("print {domain}/kersh-host"),
            format!("bootout {domain}/kersh-host"),
        ]
    );

    let stop_again = env.kersh(&["service", "stop"]);
    assert!(stop_again.status.success(), "{}", stderr(&stop_again));
    assert_eq!(
        take(&env, "launchctl"),
        [format!("print {domain}/kersh-host")]
    );
}

#[test]
fn a_systemd_unit_installs_once_and_stops_cleanly() {
    let env = env_with("systemd");
    let unit = env.home().join(".config/systemd/user/kersh-host.service");

    let first = env.kersh(&["service", "install"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(text.contains("\"host\" \"--state\""), "{text}");
    assert!(text.contains("WantedBy=default.target"), "{text}");
    assert_eq!(
        take(&env, "systemctl"),
        [
            "--user is-enabled kersh-host.service",
            "--user daemon-reload",
            "--user enable kersh-host.service",
            "--user restart kersh-host.service",
        ]
    );

    let again = env.kersh(&["service", "install"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(
        take(&env, "systemctl"),
        [
            "--user is-enabled kersh-host.service",
            "--user enable kersh-host.service",
            "--user start kersh-host.service",
        ]
    );

    let stop = env.kersh(&["service", "stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(!unit.exists());
    assert_eq!(
        take(&env, "systemctl"),
        [
            "--user disable --now kersh-host.service",
            "--user daemon-reload"
        ]
    );

    let stop_again = env.kersh(&["service", "stop"]);
    assert!(stop_again.status.success(), "{}", stderr(&stop_again));
    assert!(take(&env, "systemctl").is_empty());
}

#[test]
fn no_service_manager_installs_no_unit() {
    let env = env_with("none");
    let output = env.kersh(&["service", "install"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("service_manager is none"),
        "{}",
        stderr(&output)
    );
    assert!(take(&env, "launchctl").is_empty());
    assert!(take(&env, "systemctl").is_empty());
}

fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_unit_file_is_private() {
    for (manager, unit) in [
        ("launchd", "Library/LaunchAgents/kersh-host.plist"),
        ("systemd", ".config/systemd/user/kersh-host.service"),
    ] {
        let env = env_with(manager);
        let install = env.kersh(&["service", "install"]);
        assert!(install.status.success(), "{}", stderr(&install));
        assert_eq!(mode(&env.home().join(unit)), 0o600, "{manager}");
    }
}

#[test]
fn an_install_over_an_unloaded_unit_loads_it() {
    let env = env_with("launchd");
    let domain = format!("gui/{}", uid());
    let plist = env.home().join("Library/LaunchAgents/kersh-host.plist");
    let first = env.kersh(&["service", "install"]);
    assert!(first.status.success(), "{}", stderr(&first));
    take(&env, "launchctl");

    // Unloaded outside kersh, as `launchctl bootout` would leave it.
    std::fs::remove_file(env.home().join("launchd-loaded")).unwrap();
    let again = env.kersh(&["service", "install", "--json"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(json(&again)["changed"], false);
    assert_eq!(
        take(&env, "launchctl"),
        [
            format!("print {domain}/kersh-host"),
            format!("bootstrap {domain} {}", plist.display()),
        ]
    );
    assert!(env.home().join("launchd-loaded").exists());
}

/// `kersh doctor --json`'s line for the unit, with `state_home` as
/// `XDG_STATE_HOME`.
fn doctor_unit(env: &Env, state_home: &std::path::Path) -> serde_json::Value {
    let output = env
        .command(&["doctor", "--json"])
        .env("XDG_STATE_HOME", state_home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|item| item["area"] == "service")
        .unwrap_or_else(|| panic!("no service line: {}", stderr(&output)))
}

#[test]
fn a_unit_for_another_state_directory_is_not_this_ones() {
    let env = env_with("launchd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    let own = env.state().parent().unwrap().to_path_buf();
    assert_eq!(doctor_unit(&env, &own)["state"], "ok");

    let other = env.home().join("other-state");
    let unit = doctor_unit(&env, &other);
    assert_eq!(unit["state"], "missing", "{unit}");
    assert!(
        unit["detail"]
            .as_str()
            .unwrap()
            .contains("another state directory"),
        "{unit}"
    );
}

#[test]
fn doctor_reports_an_unloaded_unit() {
    let env = env_with("launchd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::remove_file(env.home().join("launchd-loaded")).unwrap();
    let unit = doctor_unit(&env, env.state().parent().unwrap());
    assert_eq!(unit["state"], "unloaded", "{unit}");
    assert!(
        unit["detail"]
            .as_str()
            .unwrap()
            .contains("kersh service install"),
        "{unit}"
    );
}

#[test]
fn an_unloaded_unit_falls_back_to_a_one_shot_host() {
    let env = env_with("launchd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::remove_file(env.home().join("launchd-loaded")).unwrap();
    take(&env, "launchctl");

    let output = env
        .command(&["stop", "nosuch"])
        .env("FAKE_KERSH", env!("CARGO_BIN_EXE_kersh"))
        .env("FAKE_STATE", env.state())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let calls = take(&env, "launchctl");
    env.stop_host();
    let err = stderr(&output);
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("kickstart gui/{}/kersh-host", uid())),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("bootstrap ") && call.contains("kersh.host.")),
        "no one-shot host after the unit failed: {calls:?}"
    );
    assert!(err.contains("nosuch"), "the host did not answer: {err}");
    assert_eq!(
        err.matches("kersh service install").count(),
        1,
        "the fallback is not said once: {err}"
    );
}

#[test]
fn doctor_reports_a_systemd_unit_that_is_not_enabled() {
    let env = env_with("systemd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    let state_home = env.state().parent().unwrap().to_path_buf();
    assert_eq!(doctor_unit(&env, &state_home)["state"], "ok");

    // Disabled outside kersh, as `systemctl --user disable` would leave it.
    std::fs::remove_file(env.home().join("systemd-enabled")).unwrap();
    let unit = doctor_unit(&env, &state_home);
    assert_eq!(unit["state"], "unloaded", "{unit}");
    let detail = unit["detail"].as_str().unwrap();
    assert!(detail.contains("not enabled"), "{unit}");
    assert!(detail.contains("does not start at login"), "{unit}");
    assert!(detail.contains("kersh service install"), "{unit}");
}

/// A runtime directory with a systemd user manager's private socket in
/// it, as `XDG_RUNTIME_DIR` names it in a login session.
fn user_manager_dir(env: &Env) -> std::path::PathBuf {
    let dir = env.home().join("run");
    std::fs::create_dir_all(dir.join("systemd")).unwrap();
    std::fs::write(dir.join("systemd/private"), "").unwrap();
    dir
}

/// Run `kersh stop nosuch` with the fakes able to start a host and a
/// systemd user manager present; the command's output and each fake's
/// calls.
fn client_run(env: &Env) -> (std::process::Output, Vec<String>, Vec<String>) {
    client_run_with(env, true)
}

/// `client_run`, with or without a systemd user manager.
fn client_run_with(
    env: &Env,
    user_manager: bool,
) -> (std::process::Output, Vec<String>, Vec<String>) {
    let mut command = env.command(&["stop", "nosuch"]);
    command
        .env("FAKE_KERSH", env!("CARGO_BIN_EXE_kersh"))
        .env("FAKE_STATE", env.state())
        .stdin(std::process::Stdio::null());
    if user_manager {
        command.env("XDG_RUNTIME_DIR", user_manager_dir(env));
    }
    let output = command.output().unwrap();
    let systemctl = take(env, "systemctl");
    let systemd_run = take(env, "systemd-run");
    env.stop_host();
    (output, systemctl, systemd_run)
}

#[test]
fn a_systemd_unit_that_is_not_enabled_still_starts_the_host() {
    let env = env_with("systemd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::remove_file(env.home().join("systemd-enabled")).unwrap();
    take(&env, "systemctl");

    let (output, systemctl, systemd_run) = client_run(&env);
    let err = stderr(&output);
    assert!(
        systemctl.contains(&"--user start kersh-host.service".to_owned()),
        "{systemctl:?}"
    );
    // The host starts the session server through systemd-run; only a
    // host job there would be a one-shot host.
    assert!(
        !systemd_run
            .iter()
            .any(|call| call.contains(" host --state ")),
        "a one-shot host beside the unit: {systemd_run:?}"
    );
    assert!(err.contains("nosuch"), "the host did not answer: {err}");
    assert!(!err.contains("kersh service install"), "{err}");
}

#[test]
fn a_systemd_unit_that_fails_to_start_falls_back_to_a_one_shot_host() {
    let env = env_with("systemd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::write(env.home().join("systemd-fails"), "").unwrap();
    take(&env, "systemctl");

    let (output, systemctl, systemd_run) = client_run(&env);
    let err = stderr(&output);
    assert!(
        systemctl.contains(&"--user start kersh-host.service".to_owned()),
        "{systemctl:?}"
    );
    assert!(
        systemd_run
            .iter()
            .any(|call| call.contains("--unit=kersh-host-") && call.contains(" host --state ")),
        "no one-shot host after the unit failed: {systemd_run:?}"
    );
    assert!(err.contains("nosuch"), "the host did not answer: {err}");
    assert_eq!(
        err.matches("kersh service install").count(),
        1,
        "the fallback is not said once: {err}"
    );
    assert!(!err.contains("unmask"), "{err}");
}

/// Install the systemd unit, then mask it outside kersh with `state`
/// (`masked` or `masked-runtime`), as `systemctl --user mask` would.
fn masked_env(state: &str) -> Env {
    let env = env_with("systemd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::remove_file(env.home().join("systemd-enabled")).unwrap();
    std::fs::write(env.home().join("systemd-masked"), format!("{state}\n")).unwrap();
    take(&env, "systemctl");
    env
}

#[test]
fn a_masked_systemd_unit_falls_back_and_names_unmask() {
    let env = masked_env("masked");
    let (output, _, systemd_run) = client_run(&env);
    let err = stderr(&output);
    assert!(
        systemd_run
            .iter()
            .any(|call| call.contains("--unit=kersh-host-") && call.contains(" host --state ")),
        "no one-shot host after the masked unit: {systemd_run:?}"
    );
    assert!(err.contains("nosuch"), "the host did not answer: {err}");
    assert_eq!(
        err.matches("systemctl --user unmask kersh-host.service")
            .count(),
        1,
        "the remedy is not said once: {err}"
    );
}

#[test]
fn doctor_names_unmask_for_a_masked_systemd_unit() {
    for (state, remedy) in [
        ("masked", "systemctl --user unmask kersh-host.service"),
        (
            "masked-runtime",
            "systemctl --user unmask --runtime kersh-host.service",
        ),
    ] {
        let env = masked_env(state);
        let unit = doctor_unit(&env, env.state().parent().unwrap());
        assert_eq!(unit["state"], "unloaded", "{unit}");
        let detail = unit["detail"].as_str().unwrap();
        assert!(detail.contains(remedy), "{state}: {unit}");
        assert!(!detail.contains("not enabled"), "{state}: {unit}");
    }
}

#[test]
fn an_install_over_a_masked_unit_names_unmask() {
    let env = masked_env("masked");
    let output = env.kersh(&["service", "install"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("systemctl --user unmask kersh-host.service"),
        "{err}"
    );
    assert!(
        !take(&env, "systemctl")
            .iter()
            .any(|call| call.contains(" enable ")),
        "install tried to enable a masked unit"
    );
}

/// The message for `service_manager: systemd` without a user manager.
fn says_no_user_manager(err: &str) {
    assert!(err.contains("no systemd user manager"), "{err}");
    assert!(err.contains("service_manager: none"), "{err}");
    assert!(!err.contains("Failed to connect to bus"), "{err}");
}

#[test]
fn systemd_without_a_user_manager_says_so() {
    let env = env_with("systemd");
    std::fs::write(env.home().join("no-bus"), "").unwrap();
    let (output, _, systemd_run) = client_run_with(&env, false);
    assert!(!output.status.success());
    assert_eq!(systemd_run.len(), 1, "{systemd_run:?}");
    says_no_user_manager(&stderr(&output));
}

#[test]
fn service_install_and_stop_without_a_user_manager_say_so() {
    let env = env_with("systemd");
    let first = env.kersh(&["service", "install"]);
    assert!(first.status.success(), "{}", stderr(&first));
    std::fs::write(env.home().join("no-bus"), "").unwrap();

    let stop = env.kersh(&["service", "stop"]);
    assert_eq!(stop.status.code(), Some(1), "{}", stderr(&stop));
    says_no_user_manager(&stderr(&stop));

    std::fs::remove_file(env.home().join(".config/systemd/user/kersh-host.service")).unwrap();
    let install = env.kersh(&["service", "install"]);
    assert_eq!(install.status.code(), Some(1), "{}", stderr(&install));
    says_no_user_manager(&stderr(&install));
}

#[test]
fn an_installed_systemd_unit_without_a_user_manager_says_so_once() {
    let env = env_with("systemd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    std::fs::write(env.home().join("no-bus"), "").unwrap();
    take(&env, "systemctl");

    let (output, systemctl, systemd_run) = client_run_with(&env, false);
    let err = stderr(&output);
    assert!(!output.status.success());
    assert!(
        systemctl.contains(&"--user start kersh-host.service".to_owned()),
        "{systemctl:?}"
    );
    assert!(
        systemd_run.is_empty(),
        "a one-shot host needs the same user manager: {systemd_run:?}"
    );
    assert!(!err.contains("one-shot"), "{err}");
    assert_eq!(err.matches("no systemd user manager").count(), 1, "{err}");
    says_no_user_manager(&err);
}

#[test]
fn a_client_starts_the_installed_unit() {
    let env = env_with("launchd");
    let install = env.kersh(&["service", "install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    take(&env, "launchctl");

    let output = env
        .command(&["stop", "nosuch"])
        .env("FAKE_KERSH", env!("CARGO_BIN_EXE_kersh"))
        .env("FAKE_STATE", env.state())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let calls = take(&env, "launchctl");
    env.stop_host();
    assert!(
        calls
            .iter()
            .any(|call| call == &format!("kickstart gui/{}/kersh-host", uid())),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("kersh.host.")),
        "a one-shot host job started beside the unit: {calls:?}"
    );
    assert!(
        stderr(&output).contains("nosuch"),
        "the host did not answer: {}",
        stderr(&output)
    );
}
