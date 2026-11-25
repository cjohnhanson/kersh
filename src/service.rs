//! Starting kersh's own long-lived processes: the host and the session
//! server.
//!
//! Internal calls never search `PATH`: each process is started with the
//! entry argv of the running executable, built from its canonical path.
//! With `service_manager: none` the process is spawned detached in its own
//! process group with no terminal; with `launchd` (macOS) it runs as a
//! one-shot launchd job, so quitting the terminal app that started it
//! cannot take it down.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::ServiceManager;

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

/// Start `argv` detached, with exactly `env`, writing its output to `log`.
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

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The plist of a one-shot launchd job.
#[must_use]
pub fn plist(label: &str, argv: &[String], env: &[(String, String)], log: &Path) -> String {
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
    let log = xml_escape(&log.display().to_string());
    let _ = write!(
        text,
        "  </dict>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <false/>\n\
         <key>AbandonProcessGroup</key>\n  <true/>\n  <key>ProcessType</key>\n  \
         <string>Interactive</string>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  \
         <key>StandardErrorPath</key>\n  <string>{log}</string>\n</dict>\n</plist>\n"
    );
    text
}

fn spawn_launchd(
    label: &str,
    argv: &[String],
    env: &[(String, String)],
    log: &Path,
    state: &Path,
) -> Result<(), String> {
    let uid = rustix::process::getuid().as_raw();
    let domain = format!("gui/{uid}");
    let path = state.join(format!("{label}.plist"));
    crate::paths::write_private(&path, plist(label, argv, env, log).as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // A job that ran before stays loaded after it exits; unload it first.
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let output = std::process::Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run launchctl: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "launchctl bootstrap {domain} {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    fn the_hash_is_stable() {
        assert_eq!(short_hash("/s"), short_hash("/s"));
        assert_ne!(short_hash("/s"), short_hash("/t"));
        assert_eq!(short_hash("x").len(), 10);
    }
}
