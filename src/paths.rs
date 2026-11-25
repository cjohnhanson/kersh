//! Where kersh keeps its state and its configuration.
//!
//! State lives in `$XDG_STATE_HOME/kersh/` (default `~/.local/state/kersh/`):
//! the host's lock and socket, the session server's lock and socket,
//! `groups.json`, and one directory per session. The directory is 0700
//! and every file in it 0600, because session files carry prompts and
//! tool inputs.
//!
//! Configuration lives in `~/.config/kersh/`, beside the user-level agent
//! store: `config.yml` and `runtimes.yml`.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// The paths of one kersh state directory and its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `$XDG_STATE_HOME/kersh`.
    pub state: PathBuf,
    /// `~/.config/kersh`.
    pub config: PathBuf,
}

impl Paths {
    /// The paths for this user: `XDG_STATE_HOME` when set, else
    /// `~/.local/state`; the config under the home directory.
    #[must_use]
    pub fn discover() -> Self {
        let home = crate::store::user_home().unwrap_or_else(|| PathBuf::from("/"));
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.join(".local").join("state"), PathBuf::from);
        Self {
            state: state_home.join("kersh"),
            config: home.join(".config").join("kersh"),
        }
    }

    /// Paths under explicit directories, for tests and embedders.
    #[must_use]
    pub fn new(state: PathBuf, config: PathBuf) -> Self {
        Self { state, config }
    }

    #[must_use]
    pub fn host_lock(&self) -> PathBuf {
        self.state.join("host.lock")
    }

    #[must_use]
    pub fn host_sock(&self) -> PathBuf {
        self.state.join("host.sock")
    }

    #[must_use]
    pub fn mux_lock(&self) -> PathBuf {
        self.state.join("mux.lock")
    }

    #[must_use]
    pub fn mux_sock(&self) -> PathBuf {
        self.state.join("mux.sock")
    }

    #[must_use]
    pub fn mux_conf(&self) -> PathBuf {
        self.state.join("mux.conf")
    }

    #[must_use]
    pub fn groups(&self) -> PathBuf {
        self.state.join("groups.json")
    }

    #[must_use]
    pub fn login_env(&self) -> PathBuf {
        self.state.join("login-env")
    }

    #[must_use]
    pub fn sessions(&self) -> PathBuf {
        self.state.join("sessions")
    }

    #[must_use]
    pub fn session(&self, id: &str) -> PathBuf {
        self.sessions().join(id)
    }

    #[must_use]
    pub fn record(&self, id: &str) -> PathBuf {
        self.session(id).join("record.json")
    }

    #[must_use]
    pub fn events(&self, id: &str) -> PathBuf {
        self.session(id).join("events.jsonl")
    }

    #[must_use]
    pub fn launch(&self, id: &str) -> PathBuf {
        self.session(id).join("launch.json")
    }

    #[must_use]
    pub fn note(&self, id: &str) -> PathBuf {
        self.session(id).join("note.json")
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yml")
    }

    #[must_use]
    pub fn runtimes(&self) -> PathBuf {
        self.config.join("runtimes.yml")
    }

    /// The user-level agent store, the same directory `kersh list` reads.
    #[must_use]
    pub fn user_agents(&self) -> PathBuf {
        self.config.join("agents")
    }
}

/// Create `dir` and its parents, with `dir` itself 0700.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Write `bytes` to `path` as a 0600 file, through a temporary file and a
/// rename, so a reader never sees half of it.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Write `value` as pretty JSON through [`write_private`].
pub fn write_json(path: &Path, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_private(path, &bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_private_write_is_0600_and_replaces_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let file = dir.join("a.json");
        write_private(&file, b"one").unwrap();
        write_private(&file, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
