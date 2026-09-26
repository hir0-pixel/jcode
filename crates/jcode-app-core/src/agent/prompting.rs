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
            "mode": if self.is_desktop_selfdev() { "desktop" }
                else if self.session.is_canary { "cli" } else { "regular" },
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

        let available_skills: Vec<crate::prompt::SkillInfo> = self
            .current_skills_snapshot()
            .list()
            .iter()
            .filter(|skill| skill.enabled)
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
        self.append_current_turn_system_reminder(&mut split);
        crate::prompt::append_swarm_effort_directive(
            &mut split,
            self.provider.reasoning_effort().as_deref(),
        );

        split
    }

    /// Prime's Continual Harness `prompt`-kind entries, rendered into the
    /// *static* (cached) part so provider prompt caching still applies: a
    /// running session's cache stays valid because this only changes when a
    /// brand-new session builds its first prompt, matching M9's "applied to
    /// new sessions" contract for `/refine`. Sovereign engine only (gated on
    /// the same env var as the REPL and `refine` tool), and best-effort: any
    /// storage error here must never break prompt building.
    fn append_continual_harness_addenda(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        if std::env::var_os("SOVEREIGN_HERMES_PYTHON").is_none() {
            return;
        }
        let Ok(home) = jcode_base::storage::jcode_dir() else {
            return;
        };
        let Ok(store) = sovereign_prime::entries::EntryStore::open_cached(&home) else {
            return;
        };
        let Ok(addenda) = store.render_prompt(&self.session.id) else {
            return;
        };
        let addenda = addenda.trim();
        if addenda.is_empty() {
            return;
        }
        if !split.static_part.is_empty() {
            split.static_part.push_str("\n\n");
        }
        split.static_part.push_str("# Continual Harness\n\n");
        split.static_part.push_str(addenda);
    }

    fn append_repl_guidance(&self, split: &mut crate::prompt::SplitSystemPrompt) {
        if std::env::var_os("SOVEREIGN_HERMES_PYTHON").is_none() {
            return;
        }
        if !split.static_part.is_empty() {
            split.static_part.push_str("\n\n");
        }
        split.static_part.push_str(
            "# Recursive REPL\nTreat large context as a variable: load it, inspect it programmatically, and call llm_query only on focused chunks. Keep useful state in variables and use host functions for goals, refinement, subagents, messages, and heartbeat.\n",
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
