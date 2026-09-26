//! Prime-style recursive REPL and continual harness for the sovereign engine.
//! Rust owns provider and host calls; CPython runs in a sandboxed per-session
//! worker using the Python runtime bundled with Hermes.

pub mod agent_loop;
pub mod agent_loop_host;
pub mod entries;
pub mod harness;
pub mod host;
pub mod learning;
pub mod refine;
mod worker;

pub use host::{LlmQuery, ReplHost, RunOutput};

/// Tool description shown to the model; kept short because it is sent on every request.
pub const TOOL_DESCRIPTION: &str = "Persistent Python REPL with imports and session variables; `load(path)` reads workspace files and `llm_query(prompt)` asks a focused sub-question. Cells run in a macOS sandbox; other platforms require per-cell approval (headless sessions deny). Max 16 host calls per run. Returns print() output and the last expression's value.";
