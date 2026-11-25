//! The login environment: the variables a fresh login shell sets.
//!
//! The host captures it once, by running the user's login shell on
//! kersh's own `env-dump` command, and starts the session server and
//! every harness with it as an argument array. Nothing from the process
//! that started kersh reaches a session: an agent's
//! `CLAUDE_CODE_CHILD_SESSION`, for one, turns Claude Code's transcripts
//! off, which would break resume.
//!
//! The shell gets only the variables that say who and where the user is
//! (`HOME`, `USER`, `SHELL`, the XDG directories, the locale, `TMPDIR`)
//! and a minimal `PATH`; its profile builds the rest. The entry path
//! reaches the shell as an argument (`exec "$@"`), never spliced into
//! shell text.

use std::path::PathBuf;
use std::time::Duration;

/// The marker before the variables, so profile output on stdout is
/// skipped.
const BEGIN: &str = "KERSH-ENV-BEGIN";
const END: &str = "KERSH-ENV-END";

/// Variables passed from kersh's own environment to the login shell, and
/// to a host kersh starts.
pub const PASSED: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Variables the shell sets about itself, dropped from the capture.
const DROPPED: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// The user's login shell: `SHELL` when set, else `/bin/sh`.
fn login_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from)
}

/// Print this process's environment, NUL-framed, for [`capture`].
#[must_use]
pub fn dump() -> std::process::ExitCode {
    use std::io::Write as _;
    let mut out = Vec::new();
    out.extend_from_slice(b"\0");
    out.extend_from_slice(BEGIN.as_bytes());
    out.push(0);
    for (key, value) in std::env::vars_os() {
        out.extend_from_slice(key.as_encoded_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_encoded_bytes());
        out.push(0);
    }
    out.extend_from_slice(END.as_bytes());
    out.push(0);
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(&out)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// Parse [`dump`]'s output, skipping anything a profile printed first.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(bytes);
    let start = text.find(&format!("\0{BEGIN}\0"))? + BEGIN.len() + 2;
    let body = text.get(start..)?;
    let mut vars = Vec::new();
    for item in body.split('\0') {
        if item == END {
            return Some(vars);
        }
        if let Some((key, value)) = item.split_once('=')
            && !key.is_empty()
            && !DROPPED.contains(&key)
        {
            vars.push((key.to_owned(), value.to_owned()));
        }
    }
    None
}

/// Run the login shell on `entry env-dump` and return what it set.
pub fn capture(entry: &[String]) -> Result<Vec<(String, String)>, String> {
    let shell = login_shell();
    let mut command = std::process::Command::new(&shell);
    command
        .arg("-l")
        .arg("-c")
        .arg("exec \"$@\"")
        .arg("kersh-login")
        .args(entry)
        .arg("env-dump")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", &shell)
        .current_dir(crate::store::user_home().unwrap_or_else(|| PathBuf::from("/")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for key in PASSED {
        if *key == "SHELL" {
            continue;
        }
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            command.env(key, value);
        }
    }
    if std::env::var_os("HOME").is_none_or(|home| home.is_empty())
        && let Some(home) = crate::store::user_home()
    {
        command.env("HOME", home);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run the login shell {}: {error}", shell.display()))?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut bytes);
        }
        bytes
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the login shell {} did not finish within 30 s",
                    shell.display()
                ));
            }
        }
    }
    let bytes = reader.join().unwrap_or_default();
    parse(&bytes).ok_or_else(|| {
        format!(
            "the login shell {} printed no environment; check that it runs `-l -c`",
            shell.display()
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn profile_noise_before_the_marker_is_skipped_and_shell_variables_dropped() {
        let mut bytes = b"welcome to the shell\n".to_vec();
        bytes.extend_from_slice(b"\0KERSH-ENV-BEGIN\0A=1\0PWD=/x\0B=two=2\0KERSH-ENV-END\0");
        let vars = parse(&bytes).unwrap();
        assert_eq!(
            vars,
            vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "two=2".to_owned())
            ]
        );
    }

    #[test]
    fn output_without_the_end_marker_is_no_environment() {
        assert_eq!(parse(b"\0KERSH-ENV-BEGIN\0A=1\0"), None);
        assert_eq!(parse(b"nothing"), None);
    }
}
