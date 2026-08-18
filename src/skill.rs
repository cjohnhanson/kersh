//! Resolving an almanac skill body for a run.
//!
//! An agent may name an almanac skill. kersh runs `almanac show <name>`,
//! strips the frontmatter, and appends the body to the system prompt as
//! the agent's method. The binary is `KERSH_ALMANAC` or `almanac` on
//! `PATH`, so almanac resolves the library by its own rules.
//!
//! Resolution fails closed. An agent that names a skill is defined to use
//! it, so a run aborts when almanac cannot resolve it rather than running
//! a different agent without its method.

use std::process::Command;

/// The body of the almanac skill `name`, or the reason it could not
/// resolve.
///
/// # Errors
/// Returns the reason when almanac is absent or does not know the skill.
pub fn resolve(name: &str) -> Result<String, String> {
    let binary = std::env::var("KERSH_ALMANAC").unwrap_or_else(|_| "almanac".to_owned());
    let output = Command::new(&binary)
        .arg("show")
        .arg(name)
        .output()
        .map_err(|error| format!("cannot run `{binary} show {name}`: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "almanac could not resolve the skill `{name}`: {reason}"
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(body_of(&text).to_owned())
}

/// The body after a leading `---` frontmatter block, or the whole text
/// when there is none. The frontmatter is metadata; the body is the
/// method the agent applies.
fn body_of(skill_md: &str) -> &str {
    let Some(rest) = skill_md
        .strip_prefix("---\n")
        .or_else(|| skill_md.strip_prefix("---\r\n"))
    else {
        return skill_md;
    };
    let Some(end) = rest.find("\n---") else {
        return skill_md;
    };
    let after = &rest[end + 4..];
    after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_is_the_text_after_the_frontmatter() {
        let md = "---\nname: code-review\ndescription: Reviews code.\n---\nCite file and line.\n";
        assert_eq!(body_of(md), "Cite file and line.\n");
    }

    #[test]
    fn a_skill_without_frontmatter_is_its_whole_text() {
        let md = "Cite file and line.";
        assert_eq!(body_of(md), "Cite file and line.");
    }

    #[test]
    fn an_unterminated_frontmatter_is_left_whole() {
        // A document that opens a frontmatter but never closes it is not
        // split; the whole text is the body rather than an empty one.
        let md = "---\nname: x\nno closing fence";
        assert_eq!(body_of(md), md);
    }
}
