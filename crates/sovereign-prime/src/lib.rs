//! Prime-style recursive REPL and continual harness for the sovereign engine.
//! Rust owns provider and host calls; CPython runs in a sandboxed per-session
//! worker using the Python runtime bundled with Hermes.

pub mod agent_loop;
pub mod agent_loop_host;
mod bundled_skills;
pub mod goal_ratchet;
pub mod entries;
pub mod host;
pub mod refine;
pub mod skill_files;
mod worker;

pub use host::{LlmQuery, ReplHost, RunOutput};

/// Tool description shown to the model; kept short because it is sent on every request.
pub const TOOL_DESCRIPTION: &str =
    "Python REPL; macOS sandbox, otherwise per-cell approval (headless denied).";
