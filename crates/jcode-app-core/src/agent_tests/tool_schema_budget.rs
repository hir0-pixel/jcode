use super::*;

/// Inline tool-schema budget (estimated tokens, chars/4, same meter as the
/// bench counting proxy). Measured 7,618 across 26 tools before lazy loading, 2,027 across 9 after.
const INLINE_SCHEMA_BUDGET: usize = 2_200;

/// An agent with the sovereign-engine tool set. The env gates are only read
/// while the registry is built, so restore them for neighbouring tests.
async fn sovereign_agent() -> Agent {
    let keys = ["SOVEREIGN_REPL_WORKER", "SOVEREIGN_HERMES_PYTHON"];
    let saved = keys.map(|k| (k, std::env::var_os(k)));
    crate::env::set_var("SOVEREIGN_REPL_WORKER", "/bin/sovereign");
    crate::env::set_var("SOVEREIGN_HERMES_PYTHON", "/bin/sh");
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    for (key, value) in saved {
        match value {
            Some(value) => crate::env::set_var(key, value),
            None => crate::env::remove_var(key),
        }
    }
    Agent::new(provider, registry)
}

fn names(defs: &[ToolDefinition]) -> Vec<&str> {
    defs.iter().map(|d| d.name.as_str()).collect()
}

fn model_calls(agent: &mut Agent, tool: &str, input: serde_json::Value) {
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: format!("call-{tool}"),
            name: tool.to_string(),
            input,
            thought_signature: None,
        }],
    );
}

#[tokio::test]
async fn inline_tool_schemas_stay_within_budget() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = sovereign_agent().await;
    let defs = agent.tool_definitions().await;
    let total = ToolDefinition::aggregate_prompt_token_estimate(&defs);
    eprintln!("inline schema tokens: {total} across {} tools", defs.len());
    assert!(total <= INLINE_SCHEMA_BUDGET, "{total} tokens > {INLINE_SCHEMA_BUDGET}: {:?}", names(&defs));
    for core in ["read", "write", "edit", "apply_patch", "bash", "agentgrep", "load_tools"] {
        assert!(names(&defs).contains(&core), "{core} must stay inline");
    }
    assert!(!names(&defs).contains(&"todo"));
}

#[tokio::test]
async fn load_tools_output_names_loaded_and_rejects_unknown() {
    let _guard = crate::storage::lock_test_env();
    let agent = sovereign_agent().await;
    let out = agent
        .execute_tool("load_tools", serde_json::json!({"names": ["todo", "bash", "nope"]}))
        .await
        .unwrap();
    assert!(out.output.contains("Loaded: todo.") && out.output.contains("Not loadable: bash, nope."), "{}", out.output);
}

#[tokio::test]
async fn loaded_tools_become_callable_and_persist_across_turns() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = sovereign_agent().await;
    let first = agent.tool_definitions().await;
    assert!(!names(&first).contains(&"todo") && !names(&first).contains(&"bg"));
    // No new load: the locked list is reused untouched.
    assert_eq!(names(&first), names(&agent.tool_definitions().await));

    model_calls(&mut agent, "load_tools", serde_json::json!({"names": ["todo"]}));
    let second = agent.tool_definitions().await;
    assert!(names(&second).contains(&"todo") && !names(&second).contains(&"bg"));
    assert!(names(&second).contains(&"load_tools"), "bg still deferred");

    // Later turns (more history, nothing new loaded) keep it, and it is stable.
    agent.add_message(Role::User, vec![ContentBlock::Text { text: "next".into(), cache_control: None }]);
    assert_eq!(names(&second), names(&agent.tool_definitions().await));

    // Calling a deferred tool directly also loads it from then on.
    model_calls(&mut agent, "bg", serde_json::json!({"action": "list"}));
    let third = agent.tool_definitions().await;
    assert!(names(&third).contains(&"todo") && names(&third).contains(&"bg"));
}

#[tokio::test]
async fn loaded_set_is_rebuilt_from_the_transcript() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = sovereign_agent().await;
    model_calls(&mut agent, "load_tools", serde_json::json!({"names": ["browser", "webfetch"]}));
    let defs = agent.tool_definitions().await;
    assert!(names(&defs).contains(&"browser") && names(&defs).contains(&"webfetch"));
    agent.locked_tools = None;
    agent.locked_deferred.clear();
    assert!(names(&agent.tool_definitions().await).contains(&"browser"));
}

#[tokio::test]
async fn explicit_allowlist_is_never_deferred() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = sovereign_agent().await;
    agent.allowed_tools = Some(["todo".to_string(), "read".to_string()].into_iter().collect());
    let defs = agent.tool_definitions().await;
    assert_eq!(names(&defs), ["read", "todo"]);
}
