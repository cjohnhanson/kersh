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
mod cli_sessions;
pub mod client;
pub mod compose;
pub mod discover;
pub mod events;
#[cfg(feature = "fake-model")]
pub mod fake_model;
pub mod gaff;
pub mod guard;
pub mod harness;
pub mod hook;
pub mod host;
pub mod login_env;
pub mod memories;
pub mod model;
pub mod mux;
pub mod paths;
pub mod proto;
pub mod runtime;
pub mod service;
pub mod session;
pub mod sessions;
pub mod setup;
pub mod skill;
pub mod store;
pub mod tools;
pub mod util;
pub mod wrapper;

pub use client::Client;
