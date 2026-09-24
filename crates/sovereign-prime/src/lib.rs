//! Prime-style recursive REPL for the sovereign engine.
//!
//! Follows Prime Agent / Recursive Language Models (Zhang, Kraska, Khattab):
//! large context lives in REPL variables instead of the prompt, and the model
//! works over it with code and recursive `llm_query` calls. The interpreter is
//! Pydantic Monty (a sandboxed Python subset in Rust), run in a separate,
//! memory-capped worker process so a runaway snippet cannot take the engine
//! down.

pub mod host;
pub mod worker;

pub use host::{LlmQuery, ReplHost, RunOutput};

/// Tool description shown to the model; kept short on purpose (every token
/// here is paid on every request).
pub const TOOL_DESCRIPTION: &str = "Persistent sandboxed Python REPL (Monty subset: no imports of os/sys, no network, no files). \
Use it to work over large text without pasting it into the conversation: `text = load(\"path\")` reads a \
workspace file into a variable, then slice/search it with code, and call `llm_query(prompt)` for a focused \
sub-question on a chunk (max 16 host calls per run). Variables persist across calls in this session. \
Returns print() output and the last expression's value.";
