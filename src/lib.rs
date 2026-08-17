//! kersh: a declarative agent runner.
//!
//! An agent is one file, `agents/<name>/AGENT.md`. The frontmatter names
//! a model, an almanac skill, and a gaff profile; the body is the system
//! prompt. kersh resolves the references, composes the prompt, and runs
//! the agent through rig. The profile carries the agent's context, its
//! guards, and its stop rule, so kersh declares none of that itself.
//!
//! The agent gets structured read tools, never a bash tool, because a
//! shell command string cannot be guarded. See [`tools`].

#![forbid(unsafe_code)]

pub mod agent;
pub mod cli;
pub mod compose;
pub mod store;
pub mod tools;
pub mod util;
