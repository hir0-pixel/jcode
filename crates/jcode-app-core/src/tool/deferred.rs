//! Lazy tool schemas. The schema of every tool is billed on every model call,
//! so only the core coding tools stay inline; the rest are named in
//! `load_tools`'s description and get their full schema the first time the
//! model asks for them. The loaded set is derived from the session transcript
//! (see `Agent::loaded_deferred_tools`), so it only grows, survives resume and
//! compaction, and the cached prefix breaks at most once per tool loaded.
use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub const LOAD_TOOLS: &str = "load_tools";

/// Tools withheld until loaded, with the one-line purpose shown to the model.
pub(crate) const DEFERRED: &[(&str, &str)] = &[
    ("agent_message", "message other agents, list delegated agents"),
    ("batch", "run independent tool calls in parallel"),
    ("bg", "list, wait for, or stop background tasks"),
    ("browser", "headless browser: navigate, click, read pages"),
    ("clarify", "ask the user a question when you cannot proceed without an answer"),
    ("conversation_search", "search this conversation's compacted history"),
    ("delegate", "spawn and manage child agents"),
    ("heartbeat", "re-submit a prompt when the session is idle"),
    ("hermes", "Hermes-only tools: cron jobs, image/video generation, vision, speech, Home Assistant, kanban, computer use"),
    ("invalid", "report a malformed tool call"),
    ("memory", "save and recall persistent memories"),
    ("open", "open or reveal a file or URL for the user"),
    ("refine", "keep a durable user preference or correction for future sessions"),
    ("repl", "Python for big inputs: load, slice, count, llm_query_batch"),
    ("replace", "literal or regex replace across many files, all or none"),
    ("session_goal", "set, track, complete the unattended session goal"),
    ("session_search", "search past chat sessions"),
    ("skill_manage", "list, load, edit skills"),
    ("todo", "structured todo list"),
    ("webfetch", "fetch a URL"),
    ("websearch", "search the web"),
];

pub(crate) fn is_deferred(name: &str) -> bool {
    DEFERRED.iter().any(|(n, _)| *n == name)
}

pub struct LoadToolsTool {
    catalog: String,
}

impl LoadToolsTool {
    /// `present` filters the catalog to tools this engine actually registered.
    pub fn new(present: impl Fn(&str) -> bool) -> Self {
        let catalog: Vec<String> = DEFERRED
            .iter()
            .filter(|(name, _)| present(name))
            .map(|(name, purpose)| format!("{name} ({purpose})"))
            .collect();
        // The catalog rides in the parameter description: top-level tool
        // descriptions are capped at ~20 tokens (tool::tests).
        Self { catalog: format!("Tools to load: {}.", catalog.join("; ")) }
    }
}

#[async_trait]
impl Tool for LoadToolsTool {
    fn name(&self) -> &str {
        LOAD_TOOLS
    }

    fn description(&self) -> &str {
        "Load more tools by name; callable on your next step, then stay loaded."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["names"],
            "properties": {
                "names": {"type": "array", "items": {"type": "string"}, "description": self.catalog}
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let names = requested_names(&input);
        let (known, unknown): (Vec<_>, Vec<_>) = names.into_iter().partition(|n| is_deferred(n));
        let mut out = if known.is_empty() {
            String::from("Nothing loaded.")
        } else {
            format!("Loaded: {}. Call them now.", known.join(", "))
        };
        if !unknown.is_empty() {
            out.push_str(&format!(" Not loadable: {}.", unknown.join(", ")));
        }
        Ok(ToolOutput::new(out))
    }
}

/// `names` from a `load_tools` call input (tolerates a bare string).
pub(crate) fn requested_names(input: &Value) -> Vec<String> {
    match input.get("names") {
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).map(str::to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}
