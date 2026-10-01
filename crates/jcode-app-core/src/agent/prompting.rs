use super::Agent;
use crate::logging;
use crate::message::{Message, ToolDefinition};

impl Agent {
    /// Explicitly prepare/freeze the same tool surface used by provider turns.
    /// Unlike `debug_context`, this may update the tool cache. It never calls a provider.
    pub async fn prepare_debug_context(&mut self) -> serde_json::Value {
        let prepared_tools = self.tool_definitions().await;
        let mut context = self.debug_context().await;
        context["prepared_tools"] = serde_json::json!(prepared_tools);
        context
    }

    /// Inspect the next request's static context without inference, prewarming,
    /// or locking a new tool snapshot. Pending memory is deliberately not consumed.
    pub async fn debug_context(&self) -> serde_json::Value {
        let prompt = self.build_system_prompt_split(None);
        let current_tools = self.tool_definitions_for_debug().await;
        let effective_tools = self.locked_tools.as_ref().unwrap_or(&current_tools);
        let locked_tool_names = self.locked_tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
        });
        serde_json::json!({
            "session_id": self.session.id,
            "working_dir": self.session.working_dir,
            "mode": if self.session.is_canary { "cli" } else { "regular" },
            "is_canary": self.session.is_canary,
            "system_prompt": {
                "static": prompt.static_part,
                "dynamic": prompt.dynamic_part,
                "pending_memory_included": false,
            },
            "tools_locked": self.locked_tools.is_some(),
            "locked_tool_names": locked_tool_names,
            "effective_tools": effective_tools,
            "current_tools": current_tools,
        })
    }

    pub(super) fn log_prompt_prefix_accounting(
        &self,
        split: &crate::prompt::SplitSystemPrompt,
        tools: &[ToolDefinition],
    ) {
        let system_tokens = split.estimated_tokens();
        let tool_tokens = ToolDefinition::aggregate_prompt_token_estimate(tools);
        let prefix_tokens = system_tokens + tool_tokens;
        logging::info(&format!(
            "Prompt prefix estimate: total={} tokens (system={} tools={})",
            prefix_tokens, system_tokens, tool_tokens
        ));
    }

    pub(super) fn build_memory_prompt_nonblocking_shared(
        &self,
        messages: std::sync::Arc<[Message]>,
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        if !self.memory_enabled {
            return None;
        }

        let session_id = &self.session.id;

        let fresh_user_turn = crate::message::ends_with_fresh_user_turn(&messages);
        if fresh_user_turn && crate::memory_extract::note_user_turn(session_id) {
            self.extract_memories(crate::memory_extract::Trigger::Periodic);
        }
        if fresh_user_turn {
            crate::memory_agent::recall_local_now(
                session_id,
                &messages,
                self.session.working_dir.as_deref(),
            );
        }
        let pending = if fresh_user_turn {
            crate::memory::take_pending_memory_for_project(
                session_id,
                self.session.working_dir.as_deref(),
            )
        } else {
            None
        };

        pending
    }

    /// Extract new memories from this session's messages that were not yet covered (spawned,
    /// never blocks). Session end, the 12-turn periodic run and compaction all come through here
    /// or through the same `memory_extract::spawn`.
    pub(crate) fn extract_memories(&self, trigger: crate::memory_extract::Trigger) {
        if !self.memory_enabled {
            return;
        }
        let messages = &self.session.messages;
        crate::memory_extract::spawn(
            trigger,
            &self.session.id,
            self.session.working_dir.as_deref(),
            messages.len(),
            |from| messages[from..].iter().map(|m| m.to_message()).collect(),
        );
    }

    fn append_current_turn_system_reminder(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        let Some(reminder) = self
            .current_turn_system_reminder
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        if !split.dynamic_part.is_empty() {
            split.dynamic_part.push_str("\n\n");
        }
        split.dynamic_part.push_str("# System Reminder\n\n");
        split.dynamic_part.push_str(reminder);
    }

    /// Build split system prompt for better caching
    /// Returns static (cacheable) and dynamic (not cached) parts separately
    pub(super) fn build_system_prompt_split(
        &self,
        memory_prompt: Option<&str>,
    ) -> crate::prompt::SplitSystemPrompt {
        if let Some(ref override_prompt) = self.session.system_prompt {
            return crate::prompt::SplitSystemPrompt {
                static_part: override_prompt.clone(),
                dynamic_part: String::new(),
            };
        }

        let skills = self.current_skills_snapshot();
        let skill_prompt = self
            .active_skill
            .as_ref()
            .and_then(|name| skills.get(name).map(|skill| skill.get_prompt().to_string()));

        let disabled = jcode_base::skill::disabled_skill_names();
        let available_skills: Vec<crate::prompt::SkillInfo> = self
            .current_skills_snapshot()
            .list()
            .iter()
            .filter(|skill| !disabled.contains(&skill.name))
            .map(|skill| crate::prompt::SkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect();

        let working_dir = self
            .session
            .working_dir
            .as_ref()
            .map(std::path::PathBuf::from);

        let (mut split, _context_info) = crate::prompt::build_system_prompt_split_with_agents_md(
            skill_prompt.as_deref(),
            &available_skills,
            self.session.is_canary,
            memory_prompt,
            working_dir.as_deref(),
            self.agents_md_snapshot.clone(),
        );

        self.append_continual_harness_addenda(&mut split);
        self.append_repl_guidance(&mut split);
        self.append_tool_use_enforcement(&mut split);
        self.append_current_turn_system_reminder(&mut split);
        crate::prompt::append_swarm_effort_directive(
            &mut split,
            self.provider.reasoning_effort().as_deref(),
        );

        split
    }

    /// Prime's Continual Harness `prompt` notes (memories of category `prompt`), rendered into the
    /// *static* (cached) part so provider prompt caching still applies: a
    /// running session's cache stays valid because this only changes when a
    /// brand-new session builds its first prompt, matching M9's "applied to
    /// new sessions" contract for `/refine`. Sovereign engine only (the same
    /// engine gate as the `refine` tool; no Python needed), and best-effort:
    /// any storage error here must never break prompt building.
    fn append_continual_harness_addenda(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        if std::env::var_os("SOVEREIGN_REPL_WORKER").is_none() {
            return;
        }
        // Snapshot once per session: a note learned elsewhere must not change a
        // running session's static prefix (it would break its prompt cache).
        let cached = addenda_snapshots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.session.id)
            .cloned();
        if let Some(addenda) = cached {
            push_addenda(split, &addenda);
            return;
        }
        let Ok(home) = jcode_base::storage::jcode_dir() else {
            return;
        };
        let Ok(store) = sovereign_prime::entries::EntryStore::open_cached(&home) else {
            return;
        };
        if let Some(dir) = self.session.working_dir.as_deref() {
            let _ = store.set_session_dir(&self.session.id, dir);
        }
        let Ok((addenda, note_ids)) = store.render_prompt_with_ids(&self.session.id) else {
            return;
        };
        // Behaviour rules are always here, so recall must never show them again.
        if note_ids.iter().any(|id| !crate::memory::is_memory_injected(&self.session.id, id)) {
            crate::memory::mark_memories_known(&self.session.id, &note_ids, "static prompt prefix");
        }
        let addenda = addenda.trim().to_string();
        addenda_snapshots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.session.id.clone(), addenda.clone());
        push_addenda(split, &addenda);
    }

    /// Hermes `TOOL_USE_ENFORCEMENT` for the model families it lists (never
    /// Claude). Static part, so the prompt cache prefix stays stable.
    fn append_tool_use_enforcement(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        if !needs_tool_use_enforcement(&self.provider.model()) {
            return;
        }
        split.static_part.push_str(
            "\n\nWhen you say you will act, make the tool call in the same response. Every reply either calls a tool or gives the final result.\n",
        );
    }

    fn append_repl_guidance(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        if !crate::tool::repl_available() {
            return;
        }
        if !split.static_part.is_empty() {
            split.static_part.push_str("\n\n");
        }
        split.static_part.push_str(
            "# Recursive REPL\nThe `repl` tool (load it with `load_tools`) keeps data in Python variables: `await load(path, start, length)` reads a file slice, `await llm_query(chunk)` asks a sub-model, `await llm_query_batch(list_of_chunks)` runs up to 64 at once. Variables persist between calls; also `refine`, `goal`, `heartbeat`, `spawn_subagent`, `agent_message`. Print only what you need.\n",
        );
    }

    /// Non-blocking memory prompt - takes pending result and spawns check for next turn
    #[cfg(test)]
    pub(super) fn build_memory_prompt_nonblocking(
        &self,
        messages: &[Message],
        _memory_event_tx: Option<crate::memory::MemoryEventSink>,
    ) -> Option<crate::memory::PendingMemory> {
        self.build_memory_prompt_nonblocking_shared(messages.to_vec().into(), _memory_event_tx)
    }
}

fn needs_tool_use_enforcement(model: &str) -> bool {
    const FAMILIES: [&str; 9] = [
        "gpt", "codex", "gemini", "gemma", "grok", "glm", "qwen", "deepseek", "muse",
    ];
    let model = model.to_ascii_lowercase();
    FAMILIES.iter().any(|f| model.contains(f))
}

fn addenda_snapshots() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn push_addenda(split: &mut crate::prompt::SplitSystemPrompt, addenda: &str) {
    if addenda.is_empty() {
        return;
    }
    if !split.static_part.is_empty() {
        split.static_part.push_str("\n\n");
    }
    split.static_part.push_str("# Continual Harness\n\n");
    split.static_part.push_str(addenda);
}

#[cfg(test)]
mod addenda_snapshot_tests {
    use super::*;

    #[test]
    fn tool_use_enforcement_skips_claude() {
        assert!(needs_tool_use_enforcement("gpt-5.1-codex"));
        assert!(needs_tool_use_enforcement("Qwen3-Coder"));
        assert!(!needs_tool_use_enforcement("claude-sonnet-4-5"));
    }

    #[test]
    fn snapshot_is_reused_and_not_refreshed() {
        addenda_snapshots().lock().unwrap().insert("s-snap".into(), "rule A".into());
        let mut split = crate::prompt::SplitSystemPrompt::default();
        push_addenda(&mut split, "rule A");
        assert!(split.static_part.ends_with("rule A"));
        // a later insert for another session leaves this one untouched
        addenda_snapshots().lock().unwrap().insert("other".into(), "rule B".into());
        assert_eq!(addenda_snapshots().lock().unwrap()["s-snap"], "rule A");
    }
}
