//! Every payload the gateway builds must validate against the Hermes desktop's
//! own gateway contract (vendored in `contract/`). A drift fails here instead of
//! breaking the desktop at runtime.

use serde_json::{Value, json};
use sovereign_gateway::map::{self, Out};
use std::collections::HashMap;

fn contract() -> Value {
    let raw = include_str!("../contract/gateway-contract.openrpc.json");
    serde_json::from_str(raw).expect("contract parses")
}

/// Validate `value` against `#/components/schemas/<name>` or an inline schema.
fn check(contract: &Value, schema: &Value, value: &Value, what: &str) {
    let mut root = schema.clone();
    root["components"] = contract["components"].clone();
    let validator = jsonschema::validator_for(&root).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(value).map(|e| format!("{e} at {}", e.instance_path())).collect();
    assert!(errors.is_empty(), "{what} violates the contract: {errors:?}\nvalue: {value}");
}

fn notification_schema(contract: &Value, ty: &str) -> Value {
    contract["x-notifications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == ty)
        .unwrap_or_else(|| panic!("{ty} is not a contract notification"))["params"][0]["schema"]
        .clone()
}

fn result_schema(contract: &Value, method: &str) -> Value {
    contract["methods"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == method)
        .unwrap_or_else(|| panic!("{method} is not a contract method"))["result"]["schema"]
        .clone()
}

#[test]
fn every_mapped_event_matches_its_notification_schema() {
    let c = contract();
    let frames = [
        json!({"ev":"reasoning_delta","session_id":"s","text":"think"}),
        json!({"ev":"text_delta","session_id":"s","text":"Hi"}),
        json!({"ev":"tool_start","session_id":"s","call_id":"c","name":"bash"}),
        json!({"ev":"tool_input_delta","session_id":"s","call_id":"c","delta":"{\"command\":\"ls\"}"}),
        json!({"ev":"tool_exec","session_id":"s","call_id":"c","name":"bash"}),
        json!({"ev":"tool_done","session_id":"s","call_id":"c","name":"bash","output":"x","error":"boom"}),
        json!({"ev":"token_usage","session_id":"s","input":5,"output":3,"cache_read_input":1}),
        json!({"ev":"session_renamed","session_id":"s","display_title":"T"}),
        json!({"ev":"compacted","session_id":"s","message":"done"}),
        json!({"ev":"turn_stopped","session_id":"s","reason":"failure","message":"bad"}),
        json!({"ev":"turn_done","session_id":"s"}),
    ];
    let mut sessions = HashMap::new();
    let mut seen = Vec::new();
    for frame in &frames {
        for out in map::map_event(frame, &mut sessions) {
            if let Out::Event { ty, payload, .. } = out {
                check(&c, &notification_schema(&c, ty), &payload, ty);
                seen.push(ty);
            }
        }
    }
    for ty in [
        "message.start",
        "message.delta",
        "reasoning.delta",
        "tool.start",
        "tool.complete",
        "session.usage",
        "session.title",
        "status.update",
        "error",
        "message.complete",
    ] {
        assert!(seen.contains(&ty), "{ty} was never produced");
    }
}

#[test]
fn gateway_ready_matches() {
    let c = contract();
    let payload = json!({ "skin": {}, "change_events": false, "replay_epoch": "abc" });
    check(&c, &notification_schema(&c, "gateway.ready"), &payload, "gateway.ready");
}

#[test]
fn method_results_match() {
    let c = contract();
    let mut state = map::SessionState::default();
    state.model = Some("gpt-5".into());
    let info = map::live_info("s1", Some(&state), "/tmp", "0.1.0");
    let history = map::transcript(&json!([{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}]));

    let cases = [
        ("session.create", json!({"session_id":"s1","stored_session_id":"s1","message_count":0,"messages":[],"info":info})),
        (
            "session.resume",
            json!({"session_id":"s1","stored_session_id":"s1","message_count":2,"messages":history,"running":false,"info":info}),
        ),
        ("session.history", json!({"count":2,"messages":history})),
        (
            "session.list",
            json!({"sessions":[map::session_row(&json!({"session_id":"s1","title":"A","last_active_at_ms":1700000000000i64}))]}),
        ),
        ("prompt.submit", json!({"status":"streaming"})),
        ("session.interrupt", json!({"status":"interrupted","interrupted":true})),
        ("approval.respond", json!({"resolved":1})),
        ("gateway.capabilities", json!({"per_session_exclusive_submit":false})),
        ("client.capabilities", json!({"server_requests":["approval"]})),
        ("session.usage", state.usage_json()),
    ];
    for (method, value) in cases {
        check(&c, &result_schema(&c, method), &value, method);
    }
}

#[test]
fn approval_server_request_matches() {
    let c = contract();
    let schema = c["x-server-requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "approval")
        .unwrap()["params"][0]["schema"]
        .clone();
    let params = json!({
        "session_id": "s", "request_id": "r", "command": "rm x", "description": "rm x",
        "tool_name": "bash", "choices": ["once","session","always","deny"],
        "allow_permanent": true, "allow_session": true,
    });
    check(&c, &schema, &params, "approval");
}

#[test]
fn the_validator_rejects_contract_violations() {
    let c = contract();
    for (ty, bad) in [
        ("message.delta", json!({ "text": "x", "not_in_contract": 1 })),
        ("message.complete", json!({ "status": "finished" })),
        ("error", json!({})),
    ] {
        let mut root = notification_schema(&c, ty);
        root["components"] = c["components"].clone();
        let validator = jsonschema::validator_for(&root).unwrap();
        assert!(!validator.is_valid(&bad), "{ty} accepted an invalid payload: {bad}");
    }
}
