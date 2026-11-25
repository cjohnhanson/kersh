//! The session server and the rmux implementation of [`Sessions`].
//!
//! This is the only module that names an rmux type. The server role
//! (`kersh mux`, hidden) runs `rmux-server` on kersh's socket with
//! kersh's own config, in its own process, so rmux's signal handlers stay
//! out of the host and a host restart never touches an agent. The attach
//! client hands the person's terminal to a session the way `rmux attach`
//! does.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::sessions::{SessionInfo, SessionSpec, Sessions, Size};

/// The server config: no prefix key, `C-]` detaches, Alt+Page Up scrolls,
/// no status line or mouse, and the server stays up with no sessions.
pub const CONFIG: &str = "\
set-option -g prefix None
set-option -g prefix2 None
unbind-key -a -T prefix
unbind-key -a -T root
bind-key -n M-PPage copy-mode -u
bind-key -n C-] detach-client
set-option -g detach-on-destroy on
set-option -g status off
set-option -g mouse off
set-option -g exit-empty off
set-option -g extended-keys on
";

/// The rmux wire version this build speaks.
pub const WIRE_VERSION: u32 = rmux_proto::RMUX_WIRE_VERSION;

/// What the server writes to `mux.lock`, because it outlives any host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxLock {
    pub pid: u32,
    pub entry: Vec<String>,
    pub version: String,
    pub wire_version: u32,
    /// The executable's size and modification time, to tell builds apart.
    pub build_id: String,
}

/// An error from the session server, as text.
#[derive(Debug, thiserror::Error)]
#[error("session server: {0}")]
pub struct MuxError(pub String);

impl MuxError {
    fn from_display(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

/// The size and modification time of the running executable.
#[must_use]
pub fn build_id() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unknown".to_owned();
    };
    let Ok(meta) = std::fs::metadata(exe) else {
        return "unknown".to_owned();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    format!("{}-{mtime}", meta.len())
}

/// Read `mux.lock`, if the server wrote one.
#[must_use]
pub fn read_lock(paths: &Paths) -> Option<MuxLock> {
    let text = std::fs::read_to_string(paths.mux_lock()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a live server holds `mux.lock`. A lock file no process holds
/// is left from a server that is gone.
#[must_use]
pub fn lock_held(paths: &Paths) -> bool {
    let Ok(file) = std::fs::File::open(paths.mux_lock()) else {
        return false;
    };
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared).is_err()
}

/// Refuse a live server that speaks another wire version.
pub fn check_wire(paths: &Paths) -> Result<(), String> {
    match read_lock(paths) {
        Some(lock) if lock.wire_version != WIRE_VERSION && lock_held(paths) => Err(format!(
            "the session server runs kersh {} (rmux wire {}), and this kersh {} speaks wire {}; \
             `kersh restart --mux` restarts it and restores every session",
            lock.version,
            lock.wire_version,
            env!("CARGO_PKG_VERSION"),
            WIRE_VERSION,
        )),
        _ => Ok(()),
    }
}

/// Stop the live server by the pid it recorded, with SIGTERM, and wait up
/// to `wait` for it to release `mux.lock`.
pub fn stop_server(paths: &Paths, wait: Duration) -> Result<(), String> {
    if !lock_held(paths) {
        return Ok(());
    }
    let lock = read_lock(paths).ok_or_else(|| "mux.lock names no server".to_owned())?;
    let pid = i32::try_from(lock.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| format!("mux.lock names pid {}", lock.pid))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .map_err(|error| format!("cannot stop the session server (pid {}): {error}", lock.pid))?;
    let deadline = std::time::Instant::now() + wait;
    while lock_held(paths) {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the session server (pid {}) did not stop after SIGTERM",
                lock.pid
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// The server role: hold `mux.lock`, write the config, and serve the
/// socket until the server shuts down.
pub fn serve(paths: &Paths, entry: Vec<String>) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    crate::paths::private_dir(&paths.state).map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.mux_lock())
        .map_err(|error| format!("{}: {error}", paths.mux_lock().display()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "another session server holds mux.lock".to_owned())?;
    let record = MuxLock {
        pid: std::process::id(),
        entry,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        wire_version: WIRE_VERSION,
        build_id: build_id(),
    };
    write_lock_file(&lock, &record)?;
    crate::paths::write_private(&paths.mux_conf(), CONFIG.as_bytes())
        .map_err(|error| error.to_string())?;
    let socket = paths.mux_sock();
    // Under the lock, a socket file left behind is stale.
    let _ = std::fs::remove_file(&socket);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let config = rmux_server::DaemonConfig::new(socket).with_config_files(
        vec![paths.mux_conf()],
        false,
        None,
    );
    let result = runtime.block_on(async move {
        let handle = rmux_server::ServerDaemon::new(config).bind().await?;
        handle.wait().await
    });
    drop(lock);
    result.map_err(|error| error.to_string())
}

fn write_lock_file(file: &std::fs::File, record: &MuxLock) -> Result<(), String> {
    use std::io::{Seek as _, Write as _};
    let text = serde_json::to_string(record).map_err(|error| error.to_string())?;
    let mut file = file;
    file.set_len(0).map_err(|error| error.to_string())?;
    file.rewind().map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

/// A connection to the session server.
pub struct Rmux {
    sdk: rmux_sdk::Rmux,
}

impl std::fmt::Debug for Rmux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rmux").finish_non_exhaustive()
    }
}

fn session_name(name: &str) -> Result<rmux_sdk::SessionName, MuxError> {
    rmux_sdk::SessionName::new(name).map_err(MuxError::from_display)
}

impl Rmux {
    /// Connect to the server on `socket`. Never starts one.
    pub async fn connect(socket: &Path) -> Result<Self, MuxError> {
        let sdk = rmux_sdk::Rmux::builder()
            .unix_socket(socket)
            .default_timeout(Duration::from_secs(10))
            .connect()
            .await
            .map_err(MuxError::from_display)?;
        Ok(Self { sdk })
    }

    async fn pane(&self, name: &str) -> Result<rmux_sdk::Pane, MuxError> {
        let target = rmux_sdk::PaneRef::new(session_name(name)?, 0, 0);
        self.sdk.pane(target).await.map_err(MuxError::from_display)
    }

    /// The screen and its size.
    pub async fn screen(&self, name: &str) -> Result<(Size, Vec<String>), MuxError> {
        let snapshot = self
            .pane(name)
            .await?
            .snapshot()
            .await
            .map_err(MuxError::from_display)?;
        Ok((
            Size {
                cols: snapshot.cols,
                rows: snapshot.rows,
            },
            snapshot.visible_lines(),
        ))
    }
}

impl Sessions for Rmux {
    type Error = MuxError;

    async fn create(&self, spec: &SessionSpec) -> Result<(), Self::Error> {
        let mut process = rmux_sdk::ProcessSpec::argv(spec.argv.clone());
        process.environment = Some(
            spec.env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
        );
        let ensure = rmux_sdk::EnsureSession::named(session_name(&spec.name)?)
            .create_only()
            .detached(true)
            .size(rmux_sdk::TerminalSizeSpec::new(
                spec.size.cols,
                spec.size.rows,
            ))
            .working_directory(spec.cwd.display().to_string())
            .process(process);
        self.sdk
            .ensure_session(ensure)
            .await
            .map(|_| ())
            .map_err(MuxError::from_display)
    }

    async fn list(&self) -> Result<Vec<SessionInfo>, Self::Error> {
        let names = self
            .sdk
            .list_sessions()
            .await
            .map_err(MuxError::from_display)?;
        Ok(names
            .into_iter()
            .map(|name| SessionInfo {
                name: name.as_str().to_owned(),
                size: Size { cols: 0, rows: 0 },
                alive: true,
            })
            .collect())
    }

    async fn exists(&self, name: &str) -> Result<bool, Self::Error> {
        self.sdk
            .has_session(session_name(name)?)
            .await
            .map_err(MuxError::from_display)
    }

    async fn snapshot(&self, name: &str) -> Result<Vec<String>, Self::Error> {
        Ok(self.screen(name).await?.1)
    }

    async fn send_text(&self, name: &str, text: &str) -> Result<(), Self::Error> {
        self.pane(name)
            .await?
            .send_text(text)
            .await
            .map_err(MuxError::from_display)
    }

    async fn send_key(&self, name: &str, key: &str) -> Result<(), Self::Error> {
        self.pane(name)
            .await?
            .send_key(key)
            .await
            .map_err(MuxError::from_display)
    }

    async fn kill(&self, name: &str) -> Result<(), Self::Error> {
        let session = self
            .sdk
            .session(session_name(name)?)
            .await
            .map_err(MuxError::from_display)?;
        session
            .kill()
            .await
            .map(|_| ())
            .map_err(MuxError::from_display)
    }
}

/// Run `future` on a runtime of its own, on its own thread, so a caller
/// already inside an async runtime can make a blocking call.
pub fn block_on<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, String> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| runtime.block_on(future))
            .map_err(|error| error.to_string())
    })
    .join()
    .map_err(|_| "the session server call panicked".to_owned())?
}

/// Read a session's screen straight from the server, with no host.
pub fn screen_direct(paths: &Paths, name: &str) -> Result<(Size, Vec<String>), String> {
    check_wire(paths)?;
    let socket = paths.mux_sock();
    let name = name.to_owned();
    block_on(async move {
        let mux = Rmux::connect(&socket).await?;
        // A pane handle does not need its pane to exist; a gone session
        // has no screen.
        if !mux.exists(&name).await? {
            return Err(MuxError("the session's pane is closed".to_owned()));
        }
        mux.screen(&name).await
    })?
    .map_err(|error| error.to_string())
}

/// Hand the terminal to the session `name` until the person detaches
/// with `C-]` or the session ends.
pub fn attach(paths: &Paths, name: &str) -> Result<(), String> {
    check_wire(paths)?;
    let socket: PathBuf = paths.mux_sock();
    let mut connection = rmux_client::connect(&socket)
        .map_err(|error| format!("cannot reach the session server: {error}"))?;
    let geometry = connection
        .supports_capability(rmux_proto::CAPABILITY_ATTACH_RESIZE_GEOMETRY)
        .unwrap_or(false);
    let size = terminal_size().map(|size| rmux_proto::TerminalSize {
        cols: size.cols,
        rows: size.rows,
    });
    let request = rmux_proto::AttachSessionExt3Request {
        target: Some(rmux_proto::SessionName::new(name).map_err(|error| error.to_string())?),
        target_spec: None,
        detach_other_clients: false,
        kill_other_clients: false,
        read_only: false,
        skip_environment_update: true,
        flags: None,
        working_directory: None,
        client_terminal: rmux_proto::ClientTerminalContext {
            terminal_features: Vec::new(),
            utf8: true,
        },
        client_size: size,
        attach_capabilities: if geometry {
            vec![rmux_proto::CAPABILITY_ATTACH_RESIZE_GEOMETRY.to_owned()]
        } else {
            Vec::new()
        },
    };
    match connection
        .begin_attach_with_capabilities(request)
        .map_err(|error| error.to_string())?
    {
        rmux_client::AttachTransition::Upgraded(upgrade) => {
            let (stream, initial) = upgrade.into_parts();
            let result = if geometry {
                rmux_client::attach_terminal_with_initial_bytes_and_resize_geometry(stream, initial)
            } else {
                rmux_client::attach_terminal_with_initial_bytes(stream, initial)
            };
            result.map_err(|error| error.to_string())
        }
        rmux_client::AttachTransition::Rejected(response) => Err(format!(
            "the session server refused the attach: {response:?}"
        )),
    }
}

/// The size of the terminal on standard output, when it is one.
#[must_use]
pub fn terminal_size() -> Option<Size> {
    let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some(Size {
        cols: size.ws_col,
        rows: size.ws_row,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_mismatch_on_a_live_server_names_restart_mux() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("state"), tmp.path().join("config"));
        crate::paths::private_dir(&paths.state).unwrap();
        assert!(check_wire(&paths).is_ok(), "no lock, no server to refuse");
        let lock = MuxLock {
            pid: 4242,
            entry: Vec::new(),
            version: "0.0.1".to_owned(),
            wire_version: WIRE_VERSION + 1,
            build_id: "x".to_owned(),
        };
        crate::paths::write_json(&paths.mux_lock(), &lock).unwrap();
        assert!(
            check_wire(&paths).is_ok(),
            "a lock no process holds is left from a server that is gone"
        );
        let held = std::fs::File::open(paths.mux_lock()).unwrap();
        // A process another test forks in this window can keep the shared
        // lock `check_wire` just took until it execs, so wait for it.
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();
        let error = check_wire(&paths).unwrap_err();
        assert!(error.contains("kersh 0.0.1"), "{error}");
        assert!(error.contains("`kersh restart --mux`"), "{error}");
    }
}
