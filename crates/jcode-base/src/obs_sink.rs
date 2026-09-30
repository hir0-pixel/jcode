//! Span hook for memory and learning steps.
//!
//! The lower crates (memory store, extraction, recall) cannot depend on the gateway, so they call
//! [`emit`] and the runtime installs the recorder (EveStack's `Observer`) once at startup. With no
//! recorder installed (tests, CLI tools) [`emit`] does nothing. Attributes carry ids, counts and
//! reasons; memory text never goes in.

use serde_json::{Value, json};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct Span {
    /// `memory.write`, `memory.recall`, `memory.inject`, `memory.extract`, `memory.skip`,
    /// `learning.gate`, `learning.refine`, `learning.apply`, `learning.skip`, `loop.guard`.
    pub kind: &'static str,
    pub session_id: Option<String>,
    pub error: Option<String>,
    pub attributes: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

impl Span {
    pub fn new(kind: &'static str) -> Self {
        Self { kind, session_id: None, error: None, attributes: json!({}), input_tokens: 0, output_tokens: 0, duration_ms: 0 }
    }
    pub fn session(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    pub fn attr(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.attributes[key] = value.into();
        self
    }
    pub fn error(mut self, message: impl Into<String>) -> Self {
        self.error = Some(message.into());
        self
    }
    pub fn tokens(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
    pub fn took_ms(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }
}

type Recorder = Arc<dyn Fn(Span) + Send + Sync>;
static RECORDER: RwLock<Option<Recorder>> = RwLock::new(None);

/// Install the process-wide recorder, replacing any earlier one.
pub fn install(recorder: impl Fn(Span) + Send + Sync + 'static) {
    if let Ok(mut slot) = RECORDER.write() {
        *slot = Some(Arc::new(recorder));
    }
}

pub fn emit(span: Span) {
    let recorder = RECORDER.read().ok().and_then(|slot| slot.clone());
    if let Some(recorder) = recorder {
        recorder(span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emit_reaches_the_installed_recorder_and_is_silent_without_one() {
        emit(Span::new("memory.write"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        install(move |s| sink.lock().unwrap().push(s));
        emit(Span::new("memory.write").session("s1").attr("outcome", "merged"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].attributes["outcome"], "merged");
        assert_eq!(seen[0].session_id.as_deref(), Some("s1"));
    }
}
