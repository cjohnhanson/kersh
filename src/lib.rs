//! kersh: a declarative agent runner.
//!
//! An agent is one file, `agents/<name>/AGENT.md`. The frontmatter names
//! a model, an almanac skill, and a gaff profile; the body is the system
//! prompt. kersh resolves the references, composes the prompt, and runs
//! the agent through rig. The profile carries the agent's context, its
//! guards, and its stop rule, so kersh declares none of that itself.
//!
//! The agent gets one tool, a shell. The gaff profile decides what it may
//! run, because gaff guards a tool call. kersh confines the command to the
//! root and caps its output; the profile does the rest. See [`model`].

#![forbid(unsafe_code)]

pub mod agent;
pub mod cli;
pub mod compose;
#[cfg(feature = "fake-model")]
pub mod fake_model;
pub mod gaff;
pub mod hook;
pub mod model;
pub mod skill;
pub mod store;
pub mod tools;
pub mod util;
