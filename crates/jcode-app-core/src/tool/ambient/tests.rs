use super::*;
use chrono::Utc;

#[test]
fn test_parse_priority() {
    assert_eq!(parse_priority(Some("low")), Priority::Low);
    assert_eq!(parse_priority(Some("normal")), Priority::Normal);
    assert_eq!(parse_priority(Some("high")), Priority::High);
    assert_eq!(parse_priority(None), Priority::Normal);
    assert_eq!(parse_priority(Some("unknown")), Priority::Normal);
}

#[test]
fn test_cycle_result_store_and_take() {
    let result = AmbientCycleResult {
        summary: "test".to_string(),
        memories_modified: 1,
        compactions: 0,
        proactive_work: None,
        next_schedule: None,
        started_at: Utc::now(),
        ended_at: Utc::now(),
        status: CycleStatus::Complete,
        conversation: None,
    };

    store_cycle_result(result);
    let taken = take_cycle_result();
    assert!(taken.is_some());
    assert_eq!(taken.unwrap().summary, "test");

    // Second take should be None
    assert!(take_cycle_result().is_none());
}

#[test]
fn test_end_cycle_input_deserialization() {
    let input = json!({
        "summary": "Merged 3 duplicates",
        "memories_modified": 5,
        "compactions": 1,
        "proactive_work": "Fixed typo in README",
        "next_schedule": {
            "wake_in_minutes": 20,
            "context": "Verify stale facts",
            "priority": "high"
        }
    });

    let parsed: EndCycleInput = serde_json::from_value(input).unwrap();
    assert_eq!(parsed.summary, "Merged 3 duplicates");
    assert_eq!(parsed.memories_modified, 5);
    assert_eq!(parsed.compactions, 1);
    assert_eq!(
        parsed.proactive_work.as_deref(),
        Some("Fixed typo in README")
    );
    let ns = parsed.next_schedule.unwrap();
    assert_eq!(ns.wake_in_minutes, Some(20));
    assert_eq!(ns.context.as_deref(), Some("Verify stale facts"));
    assert_eq!(ns.priority.as_deref(), Some("high"));
}

#[test]
fn test_end_cycle_input_minimal() {
    let input = json!({
        "summary": "Nothing to do",
        "memories_modified": 0,
        "compactions": 0
    });

    let parsed: EndCycleInput = serde_json::from_value(input).unwrap();
    assert_eq!(parsed.summary, "Nothing to do");
    assert!(parsed.proactive_work.is_none());
    assert!(parsed.next_schedule.is_none());
}

/// Regression for #106: Claude's tool calling emits numeric arguments as JSON
/// *strings* (e.g. `{"compactions": "0"}`). Before the fix, this failed with
/// `invalid type: string "0", expected u32`, breaking every ambient cycle.
#[test]
fn test_end_cycle_input_accepts_string_numbers() {
    let input = json!({
        "summary": "Stringified counts",
        "memories_modified": "5",
        "compactions": "0",
        "next_schedule": {
            "wake_in_minutes": "20",
            "context": "later",
            "priority": "high"
        }
    });

    let parsed: EndCycleInput = serde_json::from_value(input)
        .expect("string-encoded numbers must deserialize (regression #106)");
    assert_eq!(parsed.memories_modified, 5);
    assert_eq!(parsed.compactions, 0);
    assert_eq!(parsed.next_schedule.unwrap().wake_in_minutes, Some(20));
}

/// Ambient scheduling and permission inputs accept stringified numbers and booleans.
#[test]
fn test_ambient_inputs_accept_string_numbers_and_bools() {
    let sched: ScheduleInput = serde_json::from_value(json!({
        "wake_in_minutes": "15",
        "context": "ctx"
    }))
    .expect("schedule_ambient must accept string wake_in_minutes (#106)");
    assert_eq!(sched.wake_in_minutes, Some(15));

    let perm: RequestPermissionInput = serde_json::from_value(json!({
        "action": "delete",
        "description": "remove file",
        "rationale": "cleanup",
        "wait": "true"
    }))
    .expect("request_permission must accept string wait flag (#106)");
    assert!(perm.wait);
}

#[test]
fn test_schedule_input_deserialization() {
    let input = json!({
        "wake_in_minutes": 15,
        "context": "Check CI results",
        "priority": "normal"
    });

    let parsed: ScheduleInput = serde_json::from_value(input).unwrap();
    assert_eq!(parsed.wake_in_minutes, Some(15));
    assert!(parsed.wake_at.is_none());
    assert_eq!(parsed.context, "Check CI results");
    assert_eq!(parsed.priority.as_deref(), Some("normal"));
}

#[test]
fn test_permission_input_deserialization() {
    let input = json!({
        "action": "create_pull_request",
        "description": "Create PR for test fixes",
        "rationale": "Found failing tests that need attention",
        "urgency": "high",
        "wait": true
    });

    let parsed: RequestPermissionInput = serde_json::from_value(input).unwrap();
    assert_eq!(parsed.action, "create_pull_request");
    assert_eq!(parsed.description, "Create PR for test fixes");
    assert_eq!(parsed.rationale, "Found failing tests that need attention");
    assert_eq!(parsed.urgency.as_deref(), Some("high"));
    assert!(parsed.wait);
}

#[test]
fn test_permission_input_defaults() {
    let input = json!({
        "action": "edit",
        "description": "Fix typo",
        "rationale": "Obvious error"
    });

    let parsed: RequestPermissionInput = serde_json::from_value(input).unwrap();
    assert!(parsed.urgency.is_none());
    assert!(!parsed.wait);
}

#[test]
fn test_build_permission_review_context_defaults() {
    let review =
        build_permission_review_context("edit", "Fix typo in docs", "Needs write permission", None);

    assert_eq!(
        review
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Fix typo in docs"
    );
    assert_eq!(
        review
            .get("why_permission_needed")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Needs write permission"
    );
    assert_eq!(
        review
            .get("requested_action")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "edit"
    );
}

#[test]
fn test_build_permission_review_context_uses_structured_fields() {
    let context = json!({
        "summary": "Preparing a focused refactor",
        "why_permission_needed": "Need to modify tracked files",
        "planned_steps": ["Update parser", "Run tests"],
        "files": ["src/parser.rs", "src/tests.rs"],
        "commands": ["cargo test"],
        "risks": ["Could regress parsing edge cases"],
        "rollback_plan": "Revert commit if tests fail",
        "expected_outcome": "Parser handles edge-case input",
    });
    let review =
        build_permission_review_context("edit", "fallback summary", "fallback why", Some(&context));

    assert_eq!(
        review
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Preparing a focused refactor"
    );
    assert_eq!(
        review
            .get("why_permission_needed")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Need to modify tracked files"
    );
    assert_eq!(
        review
            .get("rollback_plan")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        "Revert commit if tests fail"
    );
    assert_eq!(
        review
            .get("planned_steps")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or_default(),
        2
    );
}

#[test]
fn test_register_unregister_ambient_session() {
    let session_id = "ambient_tool_test_session";
    unregister_ambient_session(session_id);
    assert!(!is_ambient_session_registered(session_id));

    register_ambient_session(session_id.to_string());
    assert!(is_ambient_session_registered(session_id));

    unregister_ambient_session(session_id);
    assert!(!is_ambient_session_registered(session_id));
}

#[tokio::test]
async fn test_request_permission_rejects_non_ambient_session() {
    let tool = RequestPermissionTool::new();
    let input = json!({
        "action": "edit",
        "description": "Update docs",
        "rationale": "Fix typo"
    });
    let ctx = ToolContext {
        session_id: "normal_session_test".to_string(),
        message_id: "msg_1".to_string(),
        tool_call_id: "call_1".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::Direct,
    };

    let err = tool
        .execute(input, ctx)
        .await
        .expect_err("non-ambient session should be rejected");
    assert!(
        err.to_string()
            .contains("request_permission is only available to ambient sessions")
    );
}
