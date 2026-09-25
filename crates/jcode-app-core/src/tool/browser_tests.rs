use super::*;
use crate::tool::ToolExecutionMode;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Captured {
    head: String,
    body: Vec<u8>,
}

/// A one-shot mock HTTP server: accepts a single request, records its head +
/// body, and replies with `response_body`. Mirrors the hand-rolled mock used
/// by `compile_remote/tests.rs` (no mocking crate is a jcode-app-core dep).
struct MockServer {
    base_url: String,
    captured: Arc<Mutex<Option<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(status_line: &'static str, response_body: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let captured: Arc<Mutex<Option<Captured>>> = Arc::default();
        let store = captured.clone();
        let task = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let end = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < end + length {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            *store.lock().unwrap() = Some(Captured {
                head,
                body: bytes[end..end + length].to_vec(),
            });
            let out = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(out.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        Self { base_url, captured, task }
    }

    fn take_request(self) -> (String, Value) {
        self.task.abort();
        let captured = self.captured.lock().unwrap().take().expect("no request captured");
        let body: Value = serde_json::from_slice(&captured.body).unwrap_or(Value::Null);
        (captured.head, body)
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        session_id: "sess-1".into(),
        message_id: "msg-1".into(),
        tool_call_id: "call-1".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::AgentTurn,
    }
}

#[test]
fn schema_lists_every_hermes_action_and_requires_action_only() {
    let tool = BrowserTool::new();
    let schema = tool.parameters_schema();
    let actions: Vec<&str> = schema["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for expected in ACTIONS {
        assert!(actions.contains(expected), "missing action {expected}");
    }
    assert_eq!(schema["required"], json!(["action"]));
}

#[test]
fn schema_definition_stays_under_the_token_budget() {
    let tool = BrowserTool::new();
    let def = tool.to_definition();
    let estimate = def.prompt_token_estimate();
    assert!(
        estimate < 400,
        "browser tool schema is ~{estimate} tokens (budget: 400); definition: {def:?}"
    );
}

#[tokio::test]
async fn execute_forwards_action_and_params_with_the_session_token() {
    let server = MockServer::start("200 OK", r#"{"success": true, "url": "https://example.com"}"#).await;
    let tool = BrowserTool::with_endpoint(format!("{}/api/browser/act", server.base_url), "tok-abc");

    let out = tool
        .execute(
            json!({"action": "navigate", "url": "https://example.com", "intent": "load the page"}),
            ctx(),
        )
        .await
        .unwrap();
    assert_eq!(out.output, r#"{"success": true, "url": "https://example.com"}"#);

    let (head, body) = server.take_request();
    assert!(head.starts_with("POST /api/browser/act"), "{head}");
    assert!(head.contains("x-hermes-session-token: tok-abc"), "{head}");
    assert_eq!(body["action"], "navigate");
    assert_eq!(body["task_id"], "sess-1");
    assert_eq!(body["params"]["url"], "https://example.com");
    // 'action' and 'intent' must not leak into params (the route dispatches on
    // the top-level 'action' field and Hermes's handlers take neither key).
    assert!(body["params"].get("action").is_none());
    assert!(body["params"].get("intent").is_none());
}

#[tokio::test]
async fn a_non_success_status_becomes_an_error() {
    let server = MockServer::start("500 Internal Server Error", r#"{"detail": "boom"}"#).await;
    let tool = BrowserTool::with_endpoint(format!("{}/api/browser/act", server.base_url), "tok");

    let err = tool
        .execute(json!({"action": "snapshot"}), ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    server.take_request();
}

#[tokio::test]
async fn unknown_action_never_reaches_the_network() {
    let tool = BrowserTool::with_endpoint("http://127.0.0.1:1", "tok");
    let err = tool
        .execute(json!({"action": "teleport"}), ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown action"), "{err}");
}

#[tokio::test]
async fn missing_bridge_is_a_clean_error_not_a_panic() {
    let tool = BrowserTool::new();
    let err = tool
        .execute(json!({"action": "navigate", "url": "https://example.com"}), ctx())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not configured"), "{err}");
}
