//! Where agent files live, and how a name resolves to one.
//!
//! A directory with `kersh.yml` is a root. The nearest one up the tree is
//! the active root; `~/.config/kersh/agents/` is the user-level fallback.
//! An agent named `<name>` is the file `agents/<name>/AGENT.md` under a
//! root's directory. The nearer root wins a name collision.

use std::path::{Path, PathBuf};

use crate::agent::{Agent, LoadError};

/// The root marker file. Its presence names a directory as a kersh root.
pub const ROOT_MARKER: &str = "kersh.yml";

/// The agents subdirectory under a root.
const AGENTS_DIR: &str = "agents";

/// The ordered directories an agent name resolves against: the repo root
/// first, then the user-level fallback.
#[derive(Debug, Clone)]
pub struct Stores {
    dirs: Vec<PathBuf>,
}

impl Stores {
    /// Discover the stores for `cwd`: the nearest `kersh.yml` up the tree,
    /// then `~/.config/kersh`. Either may be absent.
    #[must_use]
    pub fn discover(cwd: &Path, home: Option<&Path>) -> Self {
        let mut dirs = Vec::new();
        if let Some(root) = nearest_root(cwd) {
            dirs.push(root);
        }
        if let Some(home) = home {
            let user = home.join(".config").join("kersh");
            if user.join(AGENTS_DIR).is_dir() && !dirs.contains(&user) {
                dirs.push(user);
            }
        }
        Self { dirs }
    }

    /// Build stores from an explicit root, for `--root`.
    #[must_use]
    pub fn from_root(root: PathBuf) -> Self {
        Self { dirs: vec![root] }
    }

    /// The active root directory, if any.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.dirs.first().map(PathBuf::as_path)
    }

    /// The path to an agent file, searching the stores in order.
    fn agent_path(&self, name: &str) -> Option<PathBuf> {
        if !is_single_component(name) {
            return None;
        }
        self.dirs
            .iter()
            .map(|dir| dir.join(AGENTS_DIR).join(name).join("AGENT.md"))
            .find(|path| path.is_file())
    }

    /// Load the agent named `name`, from the nearest store that has it.
    pub fn load(&self, name: &str) -> Result<Agent, LoadError> {
        let path = self
            .agent_path(name)
            .ok_or_else(|| LoadError::NotFound(name.to_owned()))?;
        let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        Agent::parse(&text, &path, name)
    }

    /// The names of every agent in every store, sorted and deduplicated,
    /// with the nearer store winning a name.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir.join(AGENTS_DIR)) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().join("AGENT.md").is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
        }
        names.sort();
        names
    }
}

/// Whether `name` is a single normal path component, so it cannot
/// traverse. A `/`, a `\`, `.`, `..`, or an empty name is refused, so a
/// name never reaches outside a store's `agents` directory.
fn is_single_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// Walk up from `start` to the nearest directory holding `kersh.yml`.
fn nearest_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join(ROOT_MARKER).is_file() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn write_agent(root: &Path, name: &str, model: &str) {
        let dir = root.join(AGENTS_DIR).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("AGENT.md"),
            format!("---\nname: {name}\nmodel: {model}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_the_nearest_root_and_loads_an_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(ROOT_MARKER), "").unwrap();
        write_agent(root, "reviewer", "claude-code/haiku");
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();

        let stores = Stores::discover(&deep, None);
        assert_eq!(stores.root(), Some(root));
        let agent = stores.load("reviewer").unwrap();
        assert_eq!(agent.meta.name, "reviewer");
        assert_eq!(stores.names(), vec!["reviewer".to_string()]);
    }

    #[test]
    fn a_name_that_would_traverse_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        let stores = Stores::discover(tmp.path(), None);
        for name in ["../secret", "..", ".", "a/b", "x\\y"] {
            assert!(
                matches!(stores.load(name), Err(LoadError::NotFound(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn a_missing_agent_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        let stores = Stores::discover(tmp.path(), None);
        assert!(matches!(stores.load("nope"), Err(LoadError::NotFound(_))));
    }

    #[test]
    fn the_nearer_store_wins_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(ROOT_MARKER), "").unwrap();
        write_agent(&repo, "reviewer", "claude-code/haiku");
        write_agent(
            &home.join(".config").join("kersh"),
            "reviewer",
            "anthropic/claude-sonnet-5",
        );

        let stores = Stores::discover(&repo, Some(&home));
        let agent = stores.load("reviewer").unwrap();
        assert_eq!(agent.meta.model, "claude-code/haiku", "the repo store wins");
    }
}
