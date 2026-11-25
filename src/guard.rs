//! What kersh checks before a profiled start: that gaff knows the
//! profile, that the harness and mode run gaff's hooks, and that gaff's
//! hooks are registered where the harness reads its settings.
//!
//! kersh never writes a harness's settings or hook files; registration is
//! gaff's (`gaff init --host <harness>`, which `kersh setup` runs). These
//! checks only read.

use std::path::{Path, PathBuf};

use crate::session::{Harness, Mode};

/// The text a registered gaff hook carries in a harness's settings.
const GAFF_HOOK: &str = "gaff hook";

fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn home(env: &[(String, String)]) -> PathBuf {
    env_value(env, "HOME").map_or_else(
        || crate::store::user_home().unwrap_or_else(|| PathBuf::from("/")),
        PathBuf::from,
    )
}

/// The files where a harness reads its user-level settings and hooks.
#[must_use]
pub fn user_hook_files(harness: Harness, env: &[(String, String)]) -> Vec<PathBuf> {
    match harness {
        Harness::ClaudeCode => {
            let dir = env_value(env, "CLAUDE_CONFIG_DIR")
                .map_or_else(|| home(env).join(".claude"), PathBuf::from);
            vec![dir.join("settings.json")]
        }
        Harness::Codex => {
            let dir = env_value(env, "CODEX_HOME")
                .map_or_else(|| home(env).join(".codex"), PathBuf::from);
            vec![dir.join("hooks.json"), dir.join("config.toml")]
        }
        Harness::Rig => Vec::new(),
    }
}

/// The project files a harness also reads in `cwd`.
fn project_hook_files(harness: Harness, cwd: &Path) -> Vec<PathBuf> {
    match harness {
        Harness::ClaudeCode => vec![
            cwd.join(".claude").join("settings.json"),
            cwd.join(".claude").join("settings.local.json"),
        ],
        Harness::Codex | Harness::Rig => Vec::new(),
    }
}

fn names_gaff(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.contains(GAFF_HOOK))
}

/// Whether gaff's hooks are registered at the harness's user level.
#[must_use]
pub fn registered_for_user(harness: Harness, env: &[(String, String)]) -> bool {
    user_hook_files(harness, env)
        .iter()
        .any(|path| names_gaff(path))
}

/// The profiles gaff lists, by running `gaff profile list` with the login
/// environment in `cwd`.
pub fn gaff_profiles(cwd: &Path, env: &[(String, String)]) -> Result<Vec<String>, String> {
    let path_var = env_value(env, "PATH").unwrap_or_default();
    let gaff = crate::harness::find_on_path("gaff", path_var)
        .ok_or_else(|| "gaff is not on the login PATH".to_owned())?;
    let output = std::process::Command::new(&gaff)
        .args(["profile", "list"])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run gaff: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`gaff profile list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// Refuse a profiled start that would run believing it has a guard it
/// does not.
pub fn check_profile(
    harness: Harness,
    mode: Mode,
    profile: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(), String> {
    if (harness, mode) == (Harness::Codex, Mode::Interactive) {
        return Err(format!(
            "gaff profile `{profile}`: kersh has not verified that an interactive Codex session \
             runs gaff's hooks; start it headless or without a profile"
        ));
    }
    let profiles =
        gaff_profiles(cwd, env).map_err(|error| format!("gaff profile `{profile}`: {error}"))?;
    if !profiles.iter().any(|name| name == profile) {
        return Err(format!(
            "gaff has no profile `{profile}` (it lists: {})",
            if profiles.is_empty() {
                "none".to_owned()
            } else {
                profiles.join(", ")
            }
        ));
    }
    let registered = registered_for_user(harness, env)
        || project_hook_files(harness, cwd)
            .iter()
            .any(|path| names_gaff(path));
    if !registered {
        return Err(format!(
            "gaff profile `{profile}`: gaff's hooks are not registered for {harness} (checked {}); \
             run `kersh setup {harness}`",
            user_hook_files(harness, env)
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_user_files_follow_the_config_dir_variables() {
        let env = vec![
            ("HOME".to_owned(), "/h".to_owned()),
            ("CLAUDE_CONFIG_DIR".to_owned(), "/c".to_owned()),
        ];
        assert_eq!(
            user_hook_files(Harness::ClaudeCode, &env),
            [PathBuf::from("/c/settings.json")]
        );
        assert_eq!(
            user_hook_files(Harness::Codex, &env).first().unwrap(),
            &PathBuf::from("/h/.codex/hooks.json")
        );
    }

    #[test]
    fn interactive_codex_with_a_profile_is_refused() {
        let error =
            check_profile(Harness::Codex, Mode::Interactive, "w", Path::new("/"), &[]).unwrap_err();
        assert!(error.contains("interactive Codex"), "{error}");
    }

    #[test]
    fn a_missing_gaff_refuses_a_profiled_start() {
        let env = vec![("PATH".to_owned(), "/nonexistent".to_owned())];
        let error = check_profile(
            Harness::ClaudeCode,
            Mode::Headless,
            "w",
            Path::new("/"),
            &env,
        )
        .unwrap_err();
        assert!(error.contains("not on the login PATH"), "{error}");
    }
}
