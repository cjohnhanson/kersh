//! Starting kersh's own long-lived processes, the host and the session
//! server, and the `kersh-host` service unit that starts the host at
//! login.
//!
//! Internal calls never search `PATH` for kersh: each process is started
//! with the entry argv of the running executable, built from its
//! canonical path. With `service_manager: none` the process is spawned
//! detached in its own process group with no terminal; with `launchd`
//! (macOS) it runs as a one-shot launchd job, and with `systemd` (Linux)
//! as a transient `systemd-run --user --collect` unit, so quitting the
//! terminal app that started it cannot take it down.
//!
//! `kersh service install` writes the `kersh-host` unit: a launchd agent
//! in `~/Library/LaunchAgents`, or a systemd user unit in
//! `~/.config/systemd/user`. It runs `<entry> host` at login, and the
//! host restores lost sessions when it starts. A client that finds no
//! host starts the unit when one is installed for its state directory,
//! else a one-shot job. The system tools (`launchctl`, `systemctl`,
//! `systemd-run`) are looked up on `PATH`, then at their usual paths.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::runtime::ServiceManager;

/// The service unit's name, the same for every binary that owns the host.
pub const UNIT: &str = "kersh-host";

static MULTICALL: AtomicBool = AtomicBool::new(false);

/// Mark this process as a multi-call binary, whose kersh commands follow
/// a `kersh` word (`acme kersh ...`).
pub fn set_multicall() {
    MULTICALL.store(true, Ordering::SeqCst);
}

/// The argv that runs a kersh command in this executable: its canonical
/// path, then `kersh` for a multi-call binary.
#[must_use]
pub fn entry() -> Vec<String> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_or_else(|_| "kersh".to_owned(), |path| path.display().to_string());
    let mut argv = vec![exe];
    if MULTICALL.load(Ordering::SeqCst) {
        argv.push("kersh".to_owned());
    }
    argv
}

/// This machine's boot id, so a record can tell a pid from before a
/// reboot.
#[must_use]
pub fn boot_id() -> Option<String> {
    if let Ok(text) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return Some(text.trim().to_owned());
    }
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// A short, stable hash of `text` (FNV-1a), to name a launchd job per
/// state directory.
#[must_use]
pub fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}").chars().take(10).collect()
}

/// Whether a systemd user manager answers for this user: its private
/// socket exists under `XDG_RUNTIME_DIR`.
#[must_use]
pub fn systemd_user_manager() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .is_some_and(|dir| Path::new(&dir).join("systemd").join("private").exists())
}

/// A system tool: the first match on `PATH`, else `fixed`.
fn tool(name: &str, fixed: &str) -> PathBuf {
    std::env::var("PATH")
        .ok()
        .and_then(|path| crate::harness::find_on_path(name, &path))
        .unwrap_or_else(|| PathBuf::from(fixed))
}

fn launchctl() -> PathBuf {
    tool("launchctl", "/bin/launchctl")
}

fn systemctl() -> PathBuf {
    tool("systemctl", "/usr/bin/systemctl")
}

fn systemd_run() -> PathBuf {
    tool("systemd-run", "/usr/bin/systemd-run")
}

/// Run a system tool with no input; its output on success, else an
/// error naming the call.
fn call(program: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(format!(
        "`{} {}` failed: {}",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn launchd_domain() -> String {
    format!("gui/{}", rustix::process::getuid().as_raw())
}

/// The environment a host starts with: the variables that say who and
/// where the user is, and a minimal `PATH`. The host captures the login
/// environment itself.
#[must_use]
pub fn host_env() -> Vec<(String, String)> {
    crate::login_env::PASSED
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .chain(std::iter::once((
            "PATH".to_owned(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
        )))
        .collect()
}

/// The argv that runs the host for `paths`.
#[must_use]
pub fn host_argv(paths: &Paths) -> Vec<String> {
    let mut argv = entry();
    argv.extend([
        "host".to_owned(),
        "--state".to_owned(),
        paths.state.display().to_string(),
    ]);
    argv
}

/// Start `argv` detached, with `env`, writing its output to `log`.
pub fn spawn(
    manager: ServiceManager,
    label: &str,
    argv: &[String],
    env: &[(String, String)],
    log: &Path,
    state: &Path,
) -> Result<(), String> {
    match manager {
        ServiceManager::None => spawn_detached(argv, env, log),
        ServiceManager::Launchd => spawn_launchd(label, argv, env, log, state),
        ServiceManager::Systemd => {
            let words = systemd_run_args(&label.replace('.', "-"), argv, env, log);
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            call(&systemd_run(), &words).map(drop)
        }
    }
}

fn spawn_detached(argv: &[String], env: &[(String, String)], log: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| "nothing to start".to_owned())?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|error| format!("{}: {error}", log.display()))?;
    let err = out.try_clone().map_err(|error| error.to_string())?;
    let mut child = std::process::Command::new(program)
        .args(rest)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir("/")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|error| format!("cannot start `{program}`: {error}"))?;
    // Reap the child in the background when it exits early, so it leaves
    // no zombie in a long-lived caller.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The `systemd-run` arguments for a transient user unit that runs
/// `argv` once. `--collect` unloads the unit when it ends, even after a
/// failure, so the next start can reuse its name. The user manager adds
/// its own few variables to `env`.
#[must_use]
pub fn systemd_run_args(
    unit: &str,
    argv: &[String],
    env: &[(String, String)],
    log: &Path,
) -> Vec<String> {
    let log = systemd_escape(&log.display().to_string(), false);
    let mut words = vec![
        "--user".to_owned(),
        "--collect".to_owned(),
        "--quiet".to_owned(),
        format!("--unit={unit}"),
        "--working-directory=/".to_owned(),
        format!("--property=StandardOutput=append:{log}"),
        format!("--property=StandardError=append:{log}"),
    ];
    words.extend(
        env.iter()
            .map(|(key, value)| format!("--setenv={key}={value}")),
    );
    words.push("--".to_owned());
    words.extend(argv.iter().cloned());
    words
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A launchd property list: the label, the argv, the environment, and
/// `rest`, the keys after them.
fn plist_with(label: &str, argv: &[String], env: &[(String, String)], rest: &str) -> String {
    use std::fmt::Write as _;
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let _ = writeln!(
        text,
        "  <key>Label</key>\n  <string>{}</string>",
        xml_escape(label)
    );
    text.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for arg in argv {
        let _ = writeln!(text, "    <string>{}</string>", xml_escape(arg));
    }
    text.push_str("  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n");
    for (key, value) in env {
        let _ = writeln!(
            text,
            "    <key>{}</key>\n    <string>{}</string>",
            xml_escape(key),
            xml_escape(value)
        );
    }
    text.push_str("  </dict>\n");
    text.push_str(rest);
    text.push_str("</dict>\n</plist>\n");
    text
}

fn log_keys(log: &Path) -> String {
    let log = xml_escape(&log.display().to_string());
    format!(
        "  <key>StandardOutPath</key>\n  <string>{log}</string>\n  \
         <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
    )
}

/// The plist of a one-shot launchd job.
#[must_use]
pub fn plist(label: &str, argv: &[String], env: &[(String, String)], log: &Path) -> String {
    let rest = format!(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <false/>\n  \
         <key>AbandonProcessGroup</key>\n  <true/>\n  <key>ProcessType</key>\n  \
         <string>Interactive</string>\n{}",
        log_keys(log)
    );
    plist_with(label, argv, env, &rest)
}

/// The plist of the `kersh-host` launch agent: it runs at login, and
/// launchd starts it again only after a failure, so a host that exits
/// cleanly because another host holds the lock stays down.
#[must_use]
pub fn service_plist(argv: &[String], env: &[(String, String)], log: &Path) -> String {
    let rest = format!(
        "  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    \
         <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ThrottleInterval</key>\n  \
         <integer>10</integer>\n  <key>AbandonProcessGroup</key>\n  <true/>\n  \
         <key>ProcessType</key>\n  <string>Interactive</string>\n{}",
        log_keys(log)
    );
    plist_with(UNIT, argv, env, &rest)
}

/// `text` as one systemd word: in double quotes, with C escapes, `%` for
/// a specifier doubled, and `$` doubled when the line expands variables.
fn systemd_quote(text: &str, dollars: bool) -> String {
    format!("\"{}\"", systemd_escape(text, dollars))
}

fn systemd_escape(text: &str, dollars: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '%' => out.push_str("%%"),
            '$' if dollars => out.push_str("$$"),
            ch => out.push(ch),
        }
    }
    out
}

/// The `kersh-host` systemd user unit. `KillMode=process` stops only the
/// host, so a stop never reaches the session server or an agent, and
/// `Restart=on-failure` leaves down a host that exits cleanly because
/// another host holds the lock.
#[must_use]
pub fn systemd_unit(argv: &[String], env: &[(String, String)], log: &Path) -> String {
    use std::fmt::Write as _;
    let mut text = String::from(
        "[Unit]\nDescription=kersh session host\n\n[Service]\nType=simple\nExecStart=",
    );
    text.push_str(
        &argv
            .iter()
            .map(|arg| systemd_quote(arg, true))
            .collect::<Vec<_>>()
            .join(" "),
    );
    text.push('\n');
    for (key, value) in env {
        let _ = writeln!(
            text,
            "Environment={}",
            systemd_quote(&format!("{key}={value}"), false)
        );
    }
    let log = systemd_escape(&log.display().to_string(), false);
    let _ = write!(
        text,
        "WorkingDirectory=/\nKillMode=process\nRestart=on-failure\nRestartSec=10\n\
         StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
         [Install]\nWantedBy=default.target\n"
    );
    text
}

/// Where the `kersh-host` unit lives for `manager`, under `home` and
/// `config_home` (`XDG_CONFIG_HOME`, else `~/.config`). `none` has no
/// unit.
#[must_use]
pub fn unit_path(manager: ServiceManager, home: &Path, config_home: &Path) -> Option<PathBuf> {
    match manager {
        ServiceManager::None => None,
        ServiceManager::Launchd => Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{UNIT}.plist")),
        ),
        ServiceManager::Systemd => Some(
            config_home
                .join("systemd")
                .join("user")
                .join(format!("{UNIT}.service")),
        ),
    }
}

/// The unit path for this user.
#[must_use]
pub fn user_unit_path(manager: ServiceManager) -> Option<PathBuf> {
    let home = crate::store::user_home().unwrap_or_else(|| PathBuf::from("/"));
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from);
    unit_path(manager, &home, &config_home)
}

/// The unit text for `manager`.
fn unit_text(
    manager: ServiceManager,
    argv: &[String],
    env: &[(String, String)],
    log: &Path,
) -> Option<String> {
    match manager {
        ServiceManager::None => None,
        ServiceManager::Launchd => Some(service_plist(argv, env, log)),
        ServiceManager::Systemd => Some(systemd_unit(argv, env, log)),
    }
}

/// The `kersh-host` unit's state, as `kersh service install|stop --json`
/// and `kersh doctor` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub manager: ServiceManager,
    pub unit: Option<PathBuf>,
    /// The unit file exists and runs the host for this state directory.
    pub installed: bool,
    /// This call wrote or removed the unit file.
    pub changed: bool,
    pub detail: String,
}

/// Whether the unit file for `manager` runs the host for `paths`.
#[must_use]
pub fn installed_for(manager: ServiceManager, paths: &Paths) -> bool {
    let Some(path) = user_unit_path(manager) else {
        return false;
    };
    let state = paths.state.display().to_string();
    let wanted = match manager {
        ServiceManager::Launchd => format!("<string>{}</string>", xml_escape(&state)),
        ServiceManager::Systemd => systemd_quote(&state, true),
        ServiceManager::None => return false,
    };
    std::fs::read_to_string(path).is_ok_and(|text| text.contains(&wanted))
}

/// The unit's state without changing anything.
#[must_use]
pub fn status(manager: ServiceManager, paths: &Paths) -> ServiceStatus {
    let unit = user_unit_path(manager);
    let installed = installed_for(manager, paths);
    let detail = match (&unit, installed) {
        (None, _) => "service_manager is none, so no unit starts the host".to_owned(),
        (Some(path), true) => format!("{UNIT} unit at {}", path.display()),
        (Some(path), false) if path.exists() => format!(
            "{} runs a host for another state directory than {}",
            path.display(),
            paths.state.display()
        ),
        (Some(path), false) => format!(
            "no {UNIT} unit at {}; `kersh service install` writes one",
            path.display()
        ),
    };
    ServiceStatus {
        manager,
        unit,
        installed,
        changed: false,
        detail,
    }
}

fn launchd_loaded() -> bool {
    call(
        &launchctl(),
        &["print", &format!("{}/{UNIT}", launchd_domain())],
    )
    .is_ok()
}

/// Write the `kersh-host` unit for `paths` and load it, so the host runs
/// now and at every login. A second install with the same unit changes
/// nothing; one with a new unit replaces the old.
pub fn install(manager: ServiceManager, paths: &Paths) -> Result<ServiceStatus, String> {
    let path = user_unit_path(manager).ok_or_else(|| {
        "service_manager is none in config.yml; set it to launchd or systemd to install the \
         kersh-host unit"
            .to_owned()
    })?;
    crate::paths::private_dir(&paths.state).map_err(|error| error.to_string())?;
    let text = unit_text(
        manager,
        &host_argv(paths),
        &host_env(),
        &paths.state.join("host.log"),
    )
    .unwrap_or_default();
    let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str());
    if changed {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        crate::paths::write_private(&path, text.as_bytes())
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    match manager {
        ServiceManager::Launchd => {
            let domain = launchd_domain();
            let loaded = launchd_loaded();
            if changed && loaded {
                call(&launchctl(), &["bootout", &format!("{domain}/{UNIT}")])?;
            }
            if changed || !loaded {
                call(
                    &launchctl(),
                    &["bootstrap", &domain, &path.display().to_string()],
                )?;
            }
        }
        ServiceManager::Systemd => {
            let service = format!("{UNIT}.service");
            if changed {
                call(&systemctl(), &["--user", "daemon-reload"])?;
            }
            call(&systemctl(), &["--user", "enable", &service])?;
            call(
                &systemctl(),
                &[
                    "--user",
                    if changed { "restart" } else { "start" },
                    &service,
                ],
            )?;
        }
        ServiceManager::None => {}
    }
    Ok(ServiceStatus {
        manager,
        unit: Some(path.clone()),
        installed: true,
        changed,
        detail: format!(
            "{UNIT} unit {} at {}; it starts the host at login",
            if changed { "written" } else { "unchanged" },
            path.display()
        ),
    })
}

/// Stop the `kersh-host` unit, unload it, and remove its file, so the
/// host no longer starts at login. The session server and its agents
/// keep running. Stopping a unit that is not installed changes nothing.
pub fn stop(manager: ServiceManager) -> Result<ServiceStatus, String> {
    let Some(path) = user_unit_path(manager) else {
        return Ok(ServiceStatus {
            manager,
            unit: None,
            installed: false,
            changed: false,
            detail: "service_manager is none, so no unit runs".to_owned(),
        });
    };
    let mut changed = false;
    match manager {
        ServiceManager::Launchd => {
            if launchd_loaded() {
                call(
                    &launchctl(),
                    &["bootout", &format!("{}/{UNIT}", launchd_domain())],
                )?;
                changed = true;
            }
        }
        ServiceManager::Systemd => {
            if path.exists() {
                call(
                    &systemctl(),
                    &["--user", "disable", "--now", &format!("{UNIT}.service")],
                )?;
                changed = true;
            }
        }
        ServiceManager::None => {}
    }
    if path.exists() {
        std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        changed = true;
        if manager == ServiceManager::Systemd {
            call(&systemctl(), &["--user", "daemon-reload"])?;
        }
    }
    Ok(ServiceStatus {
        manager,
        unit: Some(path.clone()),
        installed: false,
        changed,
        detail: if changed {
            format!("{UNIT} unit stopped and removed from {}", path.display())
        } else {
            format!("no {UNIT} unit at {}", path.display())
        },
    })
}

/// Start the installed `kersh-host` unit. A unit that already runs is
/// left as it is.
pub fn start_unit(manager: ServiceManager) -> Result<(), String> {
    match manager {
        ServiceManager::Launchd => call(
            &launchctl(),
            &["kickstart", &format!("{}/{UNIT}", launchd_domain())],
        )
        .map(drop),
        ServiceManager::Systemd => call(
            &systemctl(),
            &["--user", "start", &format!("{UNIT}.service")],
        )
        .map(drop),
        ServiceManager::None => Err("service_manager is none, so no unit starts".to_owned()),
    }
}

fn spawn_launchd(
    label: &str,
    argv: &[String],
    env: &[(String, String)],
    log: &Path,
    state: &Path,
) -> Result<(), String> {
    let domain = launchd_domain();
    let path = state.join(format!("{label}.plist"));
    crate::paths::write_private(&path, plist(label, argv, env, log).as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // A job that ran before stays loaded after it exits; unload it first.
    let _ = call(&launchctl(), &["bootout", &format!("{domain}/{label}")]);
    call(
        &launchctl(),
        &["bootstrap", &domain, &path.display().to_string()],
    )
    .map(drop)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_plist_escapes_its_values_and_runs_once() {
        let text = plist(
            "kersh.host.x",
            &["/a b/kersh".to_owned(), "host".to_owned()],
            &[("A".to_owned(), "<&>".to_owned())],
            Path::new("/s/host.log"),
        );
        assert!(text.contains("<string>/a b/kersh</string>"));
        assert!(text.contains("<string>&lt;&amp;&gt;</string>"));
        assert!(text.contains("<key>KeepAlive</key>\n  <false/>"));
    }

    #[test]
    fn the_service_plist_runs_the_host_at_login_and_after_a_failure() {
        let text = service_plist(
            &strings(&["/opt/kersh", "host", "--state", "/s/<k>"]),
            &[("HOME".to_owned(), "/h & co".to_owned())],
            Path::new("/s/host.log"),
        );
        assert!(text.contains("<key>Label</key>\n  <string>kersh-host</string>"));
        assert!(text.contains(
            "  <array>\n    <string>/opt/kersh</string>\n    <string>host</string>\n    \
             <string>--state</string>\n    <string>/s/&lt;k&gt;</string>\n  </array>"
        ));
        assert!(text.contains("<key>HOME</key>\n    <string>/h &amp; co</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(text.contains(
            "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>"
        ));
        assert!(text.contains("<key>StandardErrorPath</key>\n  <string>/s/host.log</string>"));
        assert!(text.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    fn the_systemd_unit_quotes_every_word_and_starts_at_login() {
        let text = systemd_unit(
            &strings(&["/a b/kersh", "host", "--state", "/s/50%$x\"q"]),
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            Path::new("/s/host.log"),
        );
        assert!(
            text.contains("ExecStart=\"/a b/kersh\" \"host\" \"--state\" \"/s/50%%$$x\\\"q\"\n"),
            "{text}"
        );
        assert!(text.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(text.contains("KillMode=process\n"));
        assert!(text.contains("Restart=on-failure\n"));
        assert!(text.contains("StandardOutput=append:/s/host.log\n"));
        assert!(text.contains("[Install]\nWantedBy=default.target\n"));
        assert!(text.starts_with("[Unit]\n"));
    }

    #[test]
    fn systemd_run_starts_a_collected_user_unit() {
        let args = systemd_run_args(
            "kersh-mux-abc",
            &strings(&["/k", "mux"]),
            &[("HOME".to_owned(), "/h".to_owned())],
            Path::new("/s/mux.log"),
        );
        assert_eq!(
            args,
            strings(&[
                "--user",
                "--collect",
                "--quiet",
                "--unit=kersh-mux-abc",
                "--working-directory=/",
                "--property=StandardOutput=append:/s/mux.log",
                "--property=StandardError=append:/s/mux.log",
                "--setenv=HOME=/h",
                "--",
                "/k",
                "mux",
            ])
        );
    }

    #[test]
    fn each_manager_keeps_its_unit_in_its_own_place() {
        let home = Path::new("/h");
        let config = Path::new("/c");
        assert_eq!(
            unit_path(ServiceManager::Launchd, home, config).unwrap(),
            PathBuf::from("/h/Library/LaunchAgents/kersh-host.plist")
        );
        assert_eq!(
            unit_path(ServiceManager::Systemd, home, config).unwrap(),
            PathBuf::from("/c/systemd/user/kersh-host.service")
        );
        assert!(unit_path(ServiceManager::None, home, config).is_none());
    }

    #[test]
    fn the_hash_is_stable() {
        assert_eq!(short_hash("/s"), short_hash("/s"));
        assert_ne!(short_hash("/s"), short_hash("/t"));
        assert_eq!(short_hash("x").len(), 10);
    }
}
