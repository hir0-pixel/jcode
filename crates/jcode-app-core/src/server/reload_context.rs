use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use crate::protocol::ReloadRecoverySnapshot as ReloadRecoveryDirective;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadContext {
    pub task_context: Option<String>,
    pub version_before: String,
    pub version_after: String,
    pub session_id: String,
    pub timestamp: String,
}

impl ReloadContext {
    fn path_for_session(session_id: &str) -> Result<std::path::PathBuf> {
        let sanitized: String = session_id
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') { ch } else { '_' })
            .collect();
        Ok(crate::storage::jcode_dir()?.join(format!("reload-context-{sanitized}.json")))
    }

    fn legacy_path() -> Result<std::path::PathBuf> {
        Ok(crate::storage::jcode_dir()?.join("reload-context.json"))
    }

    #[cfg(test)]
    pub fn save(&self) -> Result<()> {
        let path = Self::path_for_session(&self.session_id)?;
        crate::storage::write_json(&path, self)
    }

    pub fn load_for_session(session_id: &str) -> Result<Option<Self>> {
        let path = Self::path_for_session(session_id)?;
        if path.exists() {
            let context = crate::storage::read_json(&path)?;
            let _ = std::fs::remove_file(path);
            return Ok(Some(context));
        }
        let legacy = Self::legacy_path()?;
        if !legacy.exists() {
            return Ok(None);
        }
        let context: Self = crate::storage::read_json(&legacy)?;
        if context.session_id == session_id {
            let _ = std::fs::remove_file(legacy);
            Ok(Some(context))
        } else {
            Ok(None)
        }
    }

    pub fn peek_for_session(session_id: &str) -> Result<Option<Self>> {
        let path = Self::path_for_session(session_id)?;
        if path.exists() {
            return crate::storage::read_json(&path).map(Some);
        }
        let legacy = Self::legacy_path()?;
        if !legacy.exists() {
            return Ok(None);
        }
        let context: Self = crate::storage::read_json(&legacy)?;
        Ok((context.session_id == session_id).then_some(context))
    }

    fn continuation_message(&self, note: &str, restored_turns: Option<usize>) -> String {
        let task = self.task_context.as_ref().map(|task| format!("\nTask context: {task}")).unwrap_or_default();
        let turns = restored_turns.map(|count| format!(" Session restored with {count} turns.")).unwrap_or_default();
        format!("Reload succeeded ({} → {}).{}{}{} Continue immediately from where you left off. Do not ask the user what to do next. Do not summarize the reload.", self.version_before, self.version_after, task, note, turns)
    }

    pub fn interrupted_session_continuation_message() -> String {
        "Your session was interrupted by a server reload while a tool was running. The tool was aborted and results may be incomplete. Continue exactly where you left off and do not ask the user what to do next.".to_string()
    }

    pub fn recovery_directive(
        context: Option<&Self>,
        was_interrupted: bool,
        background_note: &str,
        restored_turns: Option<usize>,
    ) -> Option<ReloadRecoveryDirective> {
        context.map(|context| ReloadRecoveryDirective {
            reconnect_notice: Some(format!("Reloaded with build {}", context.version_after)),
            continuation_message: context.continuation_message(background_note, restored_turns),
        }).or_else(|| was_interrupted.then(|| ReloadRecoveryDirective {
            reconnect_notice: None,
            continuation_message: Self::interrupted_session_continuation_message(),
        }))
    }

    pub fn recovery_directive_for_session(
        session_id: &str,
        context: Option<&Self>,
        was_interrupted: bool,
        restored_turns: Option<usize>,
    ) -> Option<ReloadRecoveryDirective> {
        Self::recovery_directive(context, was_interrupted, &persisted_background_tasks_note(session_id), restored_turns)
    }

    pub fn log_recovery_outcome(flow: &str, session_id: &str, outcome: &str, detail: &str) {
        crate::logging::info(&format!("reload recovery flow={flow} session_id={session_id} outcome={outcome} detail={detail}"));
    }
}

fn persisted_background_tasks_note(session_id: &str) -> String {
    let tasks = crate::background::global().persisted_detached_running_tasks_for_session(session_id);
    let mut notes = if tasks.is_empty() { String::new() } else {
        let tasks = tasks.iter().map(|task| format!("{} ({})", task.task_id, task.tool_name)).collect::<Vec<_>>().join(", ");
        format!("\nPersisted background task(s) for this session are still running: {tasks}. Do not rerun those commands. Check them first with the `bg` tool.")
    };
    let pending = crate::server::pending_await_members_for_session(session_id).into_iter().filter(|state| !state.background).collect::<Vec<_>>();
    if !pending.is_empty() {
        let pending = pending.iter().map(|state| {
            let watch = if state.requested_ids.is_empty() { "entire swarm".to_string() } else { state.requested_ids.join(", ") };
            format!("{} -> [{}], {}s remaining", watch, state.target_status.join(", "), state.remaining_timeout().as_secs())
        }).collect::<Vec<_>>().join("; ");
        notes.push_str(&format!("\nPersisted blocking `swarm await_members` wait(s) are still pending: {pending}. Rerun the same `swarm` call with action `await_members` to resume them with the remaining timeout.") );
    }
    notes
}
