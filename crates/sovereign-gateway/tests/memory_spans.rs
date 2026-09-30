//! Spans emitted through `jcode_base::obs_sink` reach `GET /api/sovereign/observability/memory`.
//! Own test binary: the sink is process-wide and each gateway installs its recorder.

use sovereign_gateway::{Config, Gateway};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn emitted_memory_spans_are_served_without_content() {
    let home = std::env::temp_dir().join(format!("sovereign-memory-spans-route-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let token = "test-token-memory-spans-route-32chars-min".to_string();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        token: token.clone(),
        version: "test".into(),
        legacy_socket: PathBuf::from("/dev/null"),
        default_cwd: home.to_string_lossy().into(),
        allow_non_loopback: false,
        provider: "ollama".into(),
        model: "local".into(),
        reasoning_efforts: Vec::new(),
        profile_model_applies: true,
        home: home.to_string_lossy().into(),
        complete: None,
        approval_secret: "approval-secret-memory-spans-min-32".into(),
        features: None,
        learning: None,
    };
    let gateway = Gateway::bind(config).await.unwrap();
    let port = gateway.local_addr().port();
    let server = tokio::spawn(async move {
        let _ = gateway.serve().await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    use jcode_base::obs_sink::{Span, emit};
    emit(Span::new("memory.write").session("sx").attr("action", "reinforced").attr("id", "m9"));
    emit(Span::new("memory.recall").session("sx").attr("returned", 2));

    let mut body = String::new();
    for _ in 0..50 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("GET /api/sovereign/observability/memory?session=sx HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        body = String::from_utf8_lossy(&buf).into_owned();
        if body.contains("memory.recall") { break; }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(body.contains("200 OK"), "{body}");
    assert!(body.contains("\"memory.write\"") && body.contains("\"reinforced\":1"), "{body}");
    assert!(body.contains("\"returned\":2"), "{body}");
    server.abort();
    let _ = std::fs::remove_dir_all(home);
}
