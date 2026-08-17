//! The read tools an agent gets: `read_file`, `grep`, and `list`.
//!
//! Each is a pure function over a confined root, implemented with
//! ripgrep's libraries, so there is no shell and no honored ignore or
//! config file an attacker could plant. A path outside the root, or a
//! symlink that escapes it, is refused. The agent never gets a bash tool,
//! because a shell command string cannot be guarded.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

/// The default byte cap on one `read_file`, with the marker included.
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024;

/// The most files `grep` and `list` visit before they stop and say so.
const MAX_FILES: usize = 20_000;

/// A confined view of one directory. Every tool resolves a caller path
/// against this root and refuses anything that escapes it.
#[derive(Debug, Clone)]
pub struct Root {
    canonical: PathBuf,
}

/// Why a tool call failed. The text is what the model reads.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("the path `{0}` is outside the agent's root")]
    Outside(String),
    #[error("the pattern is not a valid regular expression: {0}")]
    BadPattern(String),
    #[error("the glob `{glob}` is not valid: {message}")]
    BadGlob { glob: String, message: String },
    #[error("cannot read `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

impl Root {
    /// Confine tools to `dir`. The directory must exist, so an escaping
    /// symlink is caught by comparing canonical paths.
    pub fn new(dir: &Path) -> Result<Self, ToolError> {
        let canonical = dir.canonicalize().map_err(|source| ToolError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        Ok(Self { canonical })
    }

    /// The root directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.canonical
    }

    /// Resolve a caller-supplied path against the root, refusing an
    /// escape. `canonicalize` resolves symlinks, so a link pointing out
    /// of the root fails the prefix check.
    fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        let joined = self.canonical.join(rel);
        let resolved = joined.canonicalize().map_err(|source| ToolError::Io {
            path: rel.to_owned(),
            source,
        })?;
        if resolved.starts_with(&self.canonical) {
            Ok(resolved)
        } else {
            Err(ToolError::Outside(rel.to_owned()))
        }
    }

    /// Read a file under the root, capped at `max_bytes`. Over the cap,
    /// the head and the tail are kept with a marker between them, so
    /// neither the imports nor the end is lost.
    pub fn read_file(&self, rel: &str, max_bytes: usize) -> Result<String, ToolError> {
        let path = self.resolve(rel)?;
        let bytes = std::fs::read(&path).map_err(|source| ToolError::Io {
            path: rel.to_owned(),
            source,
        })?;
        Ok(cap_text(&String::from_utf8_lossy(&bytes), max_bytes))
    }

    /// List the paths under the root that match `glob`, relative to the
    /// root. Ignore and config files are not honored, so a planted
    /// `.gitignore` cannot hide a file.
    pub fn list(&self, glob: &str) -> Result<Vec<String>, ToolError> {
        let matcher = globset::Glob::new(glob)
            .map_err(|error| ToolError::BadGlob {
                glob: glob.to_owned(),
                message: error.to_string(),
            })?
            .compile_matcher();
        let mut names = Vec::new();
        let mut walker = ignore::WalkBuilder::new(&self.canonical);
        walker.standard_filters(false).follow_links(false);
        for entry in walker.build().take(MAX_FILES).flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(&self.canonical)
                && matcher.is_match(rel)
            {
                names.push(rel.display().to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Search files under `path_glob` for `pattern`, returning
    /// `path:line:text` lines. No shell, no config, binary files skipped.
    pub fn grep(&self, pattern: &str, path_glob: &str) -> Result<Vec<String>, ToolError> {
        let matcher =
            RegexMatcher::new(pattern).map_err(|error| ToolError::BadPattern(error.to_string()))?;
        let glob = globset::Glob::new(path_glob)
            .map_err(|error| ToolError::BadGlob {
                glob: path_glob.to_owned(),
                message: error.to_string(),
            })?
            .compile_matcher();
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build();

        let mut hits = Vec::new();
        let mut walker = ignore::WalkBuilder::new(&self.canonical);
        walker.standard_filters(false).follow_links(false);
        for entry in walker.build().take(MAX_FILES).flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.canonical) else {
                continue;
            };
            if !glob.is_match(rel) {
                continue;
            }
            let display = rel.display().to_string();
            let _ = searcher.search_path(
                &matcher,
                entry.path(),
                UTF8(|lnum, line| {
                    hits.push(format!("{display}:{lnum}:{}", line.trim_end()));
                    Ok(true)
                }),
            );
        }
        Ok(hits)
    }
}

/// Cap `text` at `max_bytes`, keeping the head and the tail with a marker
/// between them when it overflows.
fn cap_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker = "\n... [truncated by kersh; narrow the request] ...\n";
    let room = max_bytes.saturating_sub(marker.len());
    let head_len = room / 2;
    let tail_len = room - head_len;
    let head_end = floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len() - tail_len);
    format!("{}{marker}{}", &text[..head_end], &text[tail_start..])
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\nsecond line\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.rs"), "fn main() {}\n").unwrap();
        // A planted ignore file must not hide a.txt from list or grep.
        std::fs::write(dir.path().join(".gitignore"), "a.txt\n").unwrap();
        dir
    }

    #[test]
    fn reads_a_file_under_the_root() {
        let dir = fixture();
        let root = Root::new(dir.path()).unwrap();
        assert_eq!(
            root.read_file("a.txt", DEFAULT_MAX_BYTES).unwrap(),
            "hello world\nsecond line\n"
        );
    }

    #[test]
    fn refuses_a_path_outside_the_root() {
        let dir = fixture();
        let root = Root::new(dir.path()).unwrap();
        let error = root
            .read_file("../etc/hosts", DEFAULT_MAX_BYTES)
            .unwrap_err();
        assert!(
            matches!(error, ToolError::Outside(_) | ToolError::Io { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn refuses_an_escaping_symlink() {
        let dir = fixture();
        let secret = dir.path().parent().unwrap().join("kersh-secret.txt");
        std::fs::write(&secret, "SECRET\n").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("leak")).unwrap();
        let root = Root::new(dir.path()).unwrap();
        let error = root.read_file("leak", DEFAULT_MAX_BYTES).unwrap_err();
        assert!(matches!(error, ToolError::Outside(_)), "{error:?}");
        let _ = std::fs::remove_file(secret);
    }

    #[test]
    fn list_ignores_a_planted_gitignore() {
        let dir = fixture();
        let root = Root::new(dir.path()).unwrap();
        let names = root.list("**/*").unwrap();
        assert!(names.contains(&"a.txt".to_string()), "{names:?}");
        assert!(names.contains(&"sub/b.rs".to_string()), "{names:?}");
    }

    #[test]
    fn grep_finds_a_line_and_ignores_a_planted_gitignore() {
        let dir = fixture();
        let root = Root::new(dir.path()).unwrap();
        let hits = root.grep("world", "**/*.txt").unwrap();
        assert_eq!(hits, vec!["a.txt:1:hello world".to_string()]);
    }

    #[test]
    fn grep_refuses_a_bad_pattern() {
        let dir = fixture();
        let root = Root::new(dir.path()).unwrap();
        assert!(matches!(
            root.grep("(unclosed", "**/*").unwrap_err(),
            ToolError::BadPattern(_)
        ));
    }

    #[test]
    fn a_long_file_keeps_its_head_and_tail() {
        let mut text = String::new();
        for n in 0..1000 {
            use std::fmt::Write as _;
            let _ = writeln!(text, "line {n}");
        }
        let capped = cap_text(&text, 200);
        assert!(capped.len() <= 200 + 60);
        assert!(capped.starts_with("line 0\n"), "{capped}");
        assert!(capped.trim_end().ends_with("line 999"), "{capped}");
        assert!(capped.contains("truncated"));
    }
}
