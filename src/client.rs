//! The library API over a kersh root.
//!
//! A linked program reads agents and drives sessions through [`Client`];
//! the CLI calls the same methods. Every method has a command that prints
//! the same data as JSON with `--json`, one object per line for a list,
//! so anything a linked program can do also works from a shell.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::{Agent, LoadError};
use crate::paths::Paths;
use crate::store::Stores;

mod session_calls;

pub use session_calls::RecordStream;

/// Reads over the agent stores, and the session calls.
#[derive(Debug, Clone)]
pub struct Client {
    stores: Stores,
    paths: Paths,
}

/// One row of [`Client::list`]: an agent that loads, or a file that does
/// not. `kersh list --json` prints one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ListEntry {
    Agent(AgentSummary),
    Broken(BrokenAgent),
}

/// An agent that loads, as `kersh list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
    pub model: String,
    pub skill: Option<String>,
    pub profile: Option<String>,
    pub path: PathBuf,
}

/// An agent file that fails to load, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokenAgent {
    pub name: String,
    pub path: PathBuf,
    pub error: String,
}

/// One agent in full, as `kersh show` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDetail {
    pub name: String,
    pub description: String,
    pub model: String,
    pub skill: Option<String>,
    pub profile: Option<String>,
    pub max_turns: usize,
    pub timeout_secs: u64,
    pub path: PathBuf,
    /// The system prompt: the file's body after the frontmatter.
    pub body: String,
}

/// The result of [`Client::check`]: how many agents the stores name, and
/// every file that fails to load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub agents: usize,
    pub problems: Vec<BrokenAgent>,
}

impl CheckReport {
    /// Whether every agent file loads.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// A composed prompt, as `kersh render` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rendered {
    pub system: String,
    pub first_turn: String,
}

/// Why a read failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Load(#[from] LoadError),
    /// The agent names an almanac skill that does not resolve.
    #[error("{0}")]
    Skill(String),
}

impl Client {
    /// A client over the given stores.
    #[must_use]
    pub fn new(stores: Stores) -> Self {
        Self {
            stores,
            paths: Paths::discover(),
        }
    }

    /// A client over one explicit root, as `--root` gives.
    #[must_use]
    pub fn from_root(root: PathBuf) -> Self {
        Self::new(Stores::from_root(root))
    }

    /// A client over the stores found from `cwd`: the nearest root up the
    /// tree, then the user-level store under the home directory.
    #[must_use]
    pub fn discover(cwd: &Path) -> Self {
        let home = crate::store::user_home();
        Self::new(Stores::discover(cwd, home.as_deref()))
    }

    /// Every agent in every store, sorted by name. `kersh list`.
    #[must_use]
    pub fn list(&self) -> Vec<ListEntry> {
        self.stores
            .names()
            .into_iter()
            .map(|name| {
                let path = self.path_of(&name);
                match self.stores.load(&name) {
                    Ok(agent) => ListEntry::Agent(AgentSummary {
                        name,
                        description: agent.meta.description,
                        model: agent.meta.model,
                        skill: agent.meta.skill,
                        profile: agent.meta.profile,
                        path,
                    }),
                    Err(error) => ListEntry::Broken(BrokenAgent {
                        name,
                        path,
                        error: error.to_string(),
                    }),
                }
            })
            .collect()
    }

    /// One agent in full. `kersh show <name>`.
    pub fn show(&self, name: &str) -> Result<AgentDetail, ClientError> {
        let agent = self.stores.load(name)?;
        Ok(AgentDetail {
            path: self.path_of(name),
            name: agent.meta.name,
            description: agent.meta.description,
            model: agent.meta.model,
            skill: agent.meta.skill,
            profile: agent.meta.profile,
            max_turns: agent.meta.max_turns,
            timeout_secs: agent.meta.timeout.0.as_secs(),
            body: agent.body,
        })
    }

    /// Load every agent file and report the ones that fail. `kersh check`.
    #[must_use]
    pub fn check(&self) -> CheckReport {
        let names = self.stores.names();
        let problems = names
            .iter()
            .filter_map(|name| {
                self.stores.load(name).err().map(|error| BrokenAgent {
                    name: name.clone(),
                    path: self.path_of(name),
                    error: error.to_string(),
                })
            })
            .collect();
        CheckReport {
            agents: names.len(),
            problems,
        }
    }

    /// Compose the system prompt and first user turn a run would send,
    /// without a model call. `kersh render <name>`. The agent's almanac
    /// skill resolves here, so a skill that cannot resolve is an error.
    pub fn render(
        &self,
        name: &str,
        context: Option<&str>,
        prompt: &str,
    ) -> Result<Rendered, ClientError> {
        let agent = self.stores.load(name)?;
        let skill = skill_body(&agent).map_err(ClientError::Skill)?;
        let composed = crate::compose::compose(
            &agent,
            skill.as_deref(),
            context,
            prompt,
            &crate::util::nonce(),
        );
        Ok(Rendered {
            system: composed.system,
            first_turn: composed.first_turn,
        })
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.stores.agent_path(name).unwrap_or_default()
    }
}

/// Resolve the agent's almanac skill body, if it names one. An agent
/// without a `skill` resolves to `None`; a named skill that almanac
/// cannot resolve is an error, so a run aborts rather than dropping the
/// method.
pub(crate) fn skill_body(agent: &Agent) -> Result<Option<String>, String> {
    match &agent.meta.skill {
        None => Ok(None),
        Some(name) => crate::skill::resolve(name).map(Some),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("kersh.yml"), "").unwrap();
        for (dir, text) in [
            (
                "reviewer",
                "---\nname: reviewer\ndescription: Reviews diffs.\nmodel: claude-code/haiku\n---\nYou review diffs.\n",
            ),
            (
                "broken",
                "---\nname: other\nmodel: claude-code/haiku\n---\nb\n",
            ),
        ] {
            let path = tmp.path().join("agents").join(dir);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("AGENT.md"), text).unwrap();
        }
        tmp
    }

    #[test]
    fn list_reports_loaded_and_broken_agents_in_name_order() {
        let tmp = root();
        let client = Client::from_root(tmp.path().to_path_buf());
        let entries = client.list();
        let [ListEntry::Broken(broken), ListEntry::Agent(reviewer)] = entries.as_slice() else {
            panic!("{entries:?}");
        };
        assert_eq!(broken.name, "broken");
        assert!(broken.error.contains("names `other`"), "{}", broken.error);
        assert_eq!(reviewer.description, "Reviews diffs.");
        assert_eq!(
            reviewer.path,
            tmp.path().join("agents/reviewer/AGENT.md"),
            "the path is the file that loaded"
        );
    }

    #[test]
    fn show_returns_the_body_and_the_limits() {
        let tmp = root();
        let detail = Client::from_root(tmp.path().to_path_buf())
            .show("reviewer")
            .unwrap();
        assert_eq!(detail.body, "You review diffs.");
        assert_eq!(detail.max_turns, 4);
        assert_eq!(detail.timeout_secs, 120);
    }

    #[test]
    fn show_of_a_missing_agent_is_not_found() {
        let tmp = root();
        let error = Client::from_root(tmp.path().to_path_buf())
            .show("nope")
            .unwrap_err();
        assert!(matches!(error, ClientError::Load(LoadError::NotFound(_))));
    }

    #[test]
    fn check_counts_agents_and_names_each_problem() {
        let tmp = root();
        let report = Client::from_root(tmp.path().to_path_buf()).check();
        assert_eq!(report.agents, 2);
        assert!(!report.ok());
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems.first().unwrap().name, "broken");
    }

    #[test]
    fn render_composes_without_a_skill() {
        let tmp = root();
        let rendered = Client::from_root(tmp.path().to_path_buf())
            .render("reviewer", None, "go")
            .unwrap();
        assert_eq!(rendered.system, "You review diffs.");
        assert_eq!(rendered.first_turn, "go");
    }

    #[test]
    fn a_list_entry_serializes_flat() {
        let entry = ListEntry::Broken(BrokenAgent {
            name: "b".to_owned(),
            path: PathBuf::from("/r/agents/b/AGENT.md"),
            error: "bad".to_owned(),
        });
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"name":"b","path":"/r/agents/b/AGENT.md","error":"bad"}"#
        );
    }
}
