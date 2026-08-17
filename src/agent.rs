//! The agent file: `agents/<name>/AGENT.md`, markdown with frontmatter.
//!
//! The frontmatter names the model, an almanac skill, a gaff profile, and
//! limits. The body is the system prompt: the agent's identity, not its
//! situation. The situation comes from the caller and the profile at run
//! time, never from this file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A parsed agent definition: its frontmatter and its system-prompt body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub meta: Meta,
    /// The system prompt. Everything after the frontmatter, trimmed.
    pub body: String,
}

/// The frontmatter of an agent file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// The agent name. It must match the directory the file lives in.
    pub name: String,
    /// One line for `kersh list`.
    #[serde(default)]
    pub description: String,
    /// The model, as `<provider>/<model>`, e.g. `claude-code/haiku`.
    pub model: String,
    /// An almanac skill whose body is appended to the system prompt.
    #[serde(default)]
    pub skill: Option<String>,
    /// A gaff profile that carries the agent's context, guards, and stop
    /// rule. Selected by session state at run time.
    #[serde(default)]
    pub profile: Option<String>,
    /// The most model turns a run may take. rig enforces it.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// The per-model-call deadline. The transport has no default.
    #[serde(default = "default_timeout")]
    pub timeout: Duration,
}

const fn default_max_turns() -> usize {
    4
}

const fn default_timeout() -> Duration {
    Duration(std::time::Duration::from_secs(120))
}

/// A wall-clock duration parsed from `120s`, `2m`, or a bare seconds count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration(pub std::time::Duration);

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_duration(&raw).map(Duration).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{raw}` is not a duration; use `120s`, `2m`, or a seconds count"
            ))
        })
    }
}

/// Parse `120s`, `2m`, `1h`, or a bare integer as seconds.
fn parse_duration(raw: &str) -> Option<std::time::Duration> {
    let raw = raw.trim();
    let (digits, unit) = raw
        .find(|c: char| !c.is_ascii_digit())
        .map_or((raw, "s"), |i| raw.split_at(i));
    let value: u64 = digits.parse().ok()?;
    let secs = match unit.trim() {
        "" | "s" => value,
        "m" => value.checked_mul(60)?,
        "h" => value.checked_mul(3600)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

/// Why an agent file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("no agent named `{0}` in any store")]
    NotFound(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: the file has no `---` frontmatter block")]
    NoFrontmatter { path: PathBuf },
    #[error("{path}: the frontmatter is not valid: {message}")]
    BadFrontmatter { path: PathBuf, message: String },
    #[error("{path}: the frontmatter names `{declared}`, but the directory is `{dir}`")]
    NameMismatch {
        path: PathBuf,
        declared: String,
        dir: String,
    },
    #[error("{path}: the model `{model}` is not `<provider>/<model>` with a safe name")]
    BadModel { path: PathBuf, model: String },
}

impl Agent {
    /// Parse an agent from the text of an `AGENT.md`, checking the name
    /// against `dir_name` (the directory it lives in).
    ///
    /// The name must match the directory so a file cannot claim another
    /// agent's identity, and the model must not read as a CLI option.
    pub fn parse(text: &str, path: &Path, dir_name: &str) -> Result<Self, LoadError> {
        let (front, body) = split_frontmatter(text).ok_or_else(|| LoadError::NoFrontmatter {
            path: path.to_path_buf(),
        })?;
        let meta: Meta =
            serde_yaml_ng::from_str(front).map_err(|error| LoadError::BadFrontmatter {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        if meta.name != dir_name {
            return Err(LoadError::NameMismatch {
                path: path.to_path_buf(),
                declared: meta.name,
                dir: dir_name.to_owned(),
            });
        }
        if !model_is_safe(&meta.model) {
            return Err(LoadError::BadModel {
                path: path.to_path_buf(),
                model: meta.model,
            });
        }
        Ok(Self {
            meta,
            body: body.trim().to_owned(),
        })
    }

    /// The provider half of the model, before the first `/`.
    #[must_use]
    pub fn provider(&self) -> &str {
        self.meta.model.split_once('/').map_or("", |(p, _)| p)
    }

    /// The model half, after the first `/`.
    #[must_use]
    pub fn model_id(&self) -> &str {
        self.meta.model.split_once('/').map_or("", |(_, m)| m)
    }
}

/// Split `---\n<front>\n---\n<body>` into the frontmatter and the body.
///
/// The opening `---` must be the first line, so a document that merely
/// contains a `---` rule is not mistaken for a frontmatter block.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let after = &rest[end + 4..];
    let body = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after);
    Some((front, body))
}

/// Whether a model string is `<provider>/<model>` with characters that
/// cannot be read as a CLI option.
///
/// The model reaches a child process's argument vector. A value such as
/// `haiku --settings=...` would otherwise execute a hook before any turn.
/// Both halves are non-empty and drawn from `[a-z0-9._-]`.
fn model_is_safe(model: &str) -> bool {
    let Some((provider, id)) = model.split_once('/') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
    };
    ok(provider) && ok(id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn parse(text: &str, dir: &str) -> Result<Agent, LoadError> {
        Agent::parse(text, Path::new("agents/x/AGENT.md"), dir)
    }

    const OK: &str = "---\nname: reviewer\nmodel: claude-code/haiku\n---\nYou review diffs.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let agent = parse(OK, "reviewer").unwrap();
        assert_eq!(agent.meta.name, "reviewer");
        assert_eq!(agent.meta.model, "claude-code/haiku");
        assert_eq!(agent.body, "You review diffs.");
        assert_eq!(agent.meta.max_turns, 4);
        assert_eq!(agent.provider(), "claude-code");
        assert_eq!(agent.model_id(), "haiku");
    }

    #[test]
    fn a_name_that_does_not_match_the_directory_is_refused() {
        let error = parse(OK, "other").unwrap_err();
        assert!(matches!(error, LoadError::NameMismatch { .. }), "{error:?}");
    }

    #[test]
    fn a_model_that_could_be_a_cli_option_is_refused() {
        for model in [
            "haiku --settings=x",
            "-haiku",
            "claude-code/haiku extra",
            "noprovider",
            "claude-code/",
            "/haiku",
        ] {
            let text = format!("---\nname: r\nmodel: \"{model}\"\n---\nbody\n");
            let error = parse(&text, "r").unwrap_err();
            assert!(
                matches!(error, LoadError::BadModel { .. }),
                "{model}: {error:?}"
            );
        }
    }

    #[test]
    fn a_file_without_frontmatter_is_refused() {
        let error = parse("no frontmatter here\n", "r").unwrap_err();
        assert!(
            matches!(error, LoadError::NoFrontmatter { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn durations_parse_from_units() {
        assert_eq!(
            parse_duration("120s"),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(
            parse_duration("2m"),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(
            parse_duration("1h"),
            Some(std::time::Duration::from_secs(3600))
        );
        assert_eq!(
            parse_duration("90"),
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(parse_duration("x"), None);
    }

    #[test]
    fn a_timeout_and_max_turns_override_the_defaults() {
        let text =
            "---\nname: r\nmodel: anthropic/claude-sonnet-5\nmax_turns: 8\ntimeout: 2m\n---\nb\n";
        let agent = parse(text, "r").unwrap();
        assert_eq!(agent.meta.max_turns, 8);
        assert_eq!(agent.meta.timeout.0, std::time::Duration::from_secs(120));
    }
}
