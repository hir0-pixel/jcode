//! Schema snapshot tests: fail if the wire shape changes accidentally.

use crate::*;

#[test]
fn create_session_system_prompt_roundtrip_and_legacy_default() {
    let legacy = serde_json::json!({"req": "create_session"});
    let decoded: ApiRequest = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
    for prompt in ["Custom system instructions\nwith unicode: 世界", ""] {
        let request = ApiRequest::CreateSession {
            working_dir: None,
            system_prompt: Some(prompt.into()),
        };
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["system_prompt"], prompt);
        assert_eq!(serde_json::from_value::<ApiRequest>(wire).unwrap(), request);
    }
}

#[test]
fn token_usage_preserves_cache_creation_and_accepts_legacy_frames() {
    let legacy = r#"{"v":1,"ev":"token_usage","session_id":"s1","input":10,"output":5,"cache_read_input":2}"#;
    let legacy_frame: ServerFrame = serde_json::from_str(legacy).unwrap();
    assert!(matches!(
        &legacy_frame.event,
        ApiEvent::TokenUsage {
            cache_creation_input: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_string(&legacy_frame).unwrap(), legacy);

    for cache_creation_input in [Some(0), Some(42)] {
        let frame = ServerFrame::event(ApiEvent::TokenUsage {
            session_id: "s1".into(),
            input: 10,
            output: 5,
            cache_read_input: Some(2),
            cache_creation_input,
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["cache_creation_input"], cache_creation_input.unwrap());
        let decoded: ServerFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.event, frame.event);
    }
}

#[test]
fn client_frame_wire_shape() {
    let frame = ClientFrame::new(
        7,
        ApiRequest::SendMessage {
            session_id: "s1".into(),
            content: "hi".into(),
            images: vec![],
            system_reminder: None,
            no_reply: false,
        },
    );
    let json = serde_json::to_string(&frame).unwrap();
    assert_eq!(
        json,
        r#"{"v":1,"id":7,"req":"send_message","session_id":"s1","content":"hi"}"#
    );
}

#[test]
fn send_message_no_reply_wire_shape_and_legacy_default() {
    let frame = ClientFrame::new(
        8,
        ApiRequest::SendMessage {
            session_id: "s1".into(),
            content: "context".into(),
            images: vec![],
            system_reminder: None,
            no_reply: true,
        },
    );
    let json = serde_json::to_string(&frame).unwrap();
    assert_eq!(
        json,
        r#"{"v":1,"id":8,"req":"send_message","session_id":"s1","content":"context","no_reply":true}"#
    );

    let legacy: ClientFrame = serde_json::from_str(
        r#"{"v":1,"id":9,"req":"send_message","session_id":"s1","content":"old","images":[]}"#,
    )
    .unwrap();
    assert!(matches!(
        legacy.request,
        ApiRequest::SendMessage {
            system_reminder: None,
            no_reply: false,
            ..
        }
    ));
}

#[test]
fn soft_interrupt_images_wire_shape_and_legacy_default() {
    let frame = ClientFrame::new(
        10,
        ApiRequest::SoftInterrupt {
            session_id: "s1".into(),
            content: "look".into(),
            images: vec![("image/png".into(), "aW1hZ2U=".into())],
            urgent: true,
        },
    );
    assert_eq!(
        serde_json::to_string(&frame).unwrap(),
        r#"{"v":1,"id":10,"req":"soft_interrupt","session_id":"s1","content":"look","images":[["image/png","aW1hZ2U="]],"urgent":true}"#
    );

    let legacy: ClientFrame = serde_json::from_str(
        r#"{"v":1,"id":11,"req":"soft_interrupt","session_id":"s1","content":"old"}"#,
    )
    .unwrap();
    assert!(matches!(
        legacy.request,
        ApiRequest::SoftInterrupt {
            images,
            urgent: false,
            ..
        } if images.is_empty()
    ));
}

#[test]
fn server_frame_wire_shape() {
    let frame = ServerFrame::reply(
        3,
        ApiEvent::HelloOk {
            version: 1,
            server: "jcode/0.55.1".into(),
            capabilities: vec![],
        },
    );
    let json = serde_json::to_string(&frame).unwrap();
    assert_eq!(
        json,
        r#"{"v":1,"reply_to":3,"ev":"hello_ok","version":1,"server":"jcode/0.55.1"}"#
    );
}

#[test]
fn unknown_event_kind_is_skippable() {
    let json = r#"{"v":1,"ev":"some_future_event","payload":123}"#;
    let frame: ServerFrame = serde_json::from_str(json).unwrap();
    assert_eq!(frame.event, ApiEvent::Unknown);
}

#[test]
fn unknown_fields_are_ignored() {
    let json = r#"{"v":1,"ev":"turn_done","session_id":"s1","future_field":true}"#;
    let frame: ServerFrame = serde_json::from_str(json).unwrap();
    assert_eq!(
        frame.event,
        ApiEvent::TurnDone {
            session_id: "s1".into()
        }
    );
}

#[test]
fn request_roundtrip() {
    let reqs = [
        ApiRequest::Hello {
            min_version: 1,
            max_version: 1,
            client: "test/0".into(),
        },
        ApiRequest::ListSessions {
            include_archived: false,
            limit: None,
        },
        ApiRequest::ArchiveSession {
            session_id: "s1".into(),
        },
        ApiRequest::RestoreSession {
            session_id: "s1".into(),
        },
        ApiRequest::SetRetentionPolicy {
            archive_after_days: Some(30),
        },
        ApiRequest::CreateSession {
            working_dir: None,
            system_prompt: None,
        },
        ApiRequest::AttachSession {
            session_id: "s1".into(),
        },
        ApiRequest::Cancel {
            session_id: "s1".into(),
        },
        ApiRequest::PermissionResponse {
            session_id: "s1".into(),
            request_id: "p1".into(),
            decision: PermissionDecision::Allow,
        },
        ApiRequest::GetRuntimeInfo {
            session_id: "s1".into(),
        },
        ApiRequest::SetApiKey {
            provider: "gemini".into(),
            api_key: "secret".into(),
        },
        ApiRequest::ClearApiKey {
            provider: "gemini".into(),
        },
        ApiRequest::ReadFile {
            session_id: "s1".into(),
            path: "src/lib.rs".into(),
            max_bytes: Some(1024),
        },
        ApiRequest::FindFiles {
            session_id: "s1".into(),
            query: "lib".into(),
            limit: Some(10),
        },
        ApiRequest::SearchText {
            session_id: "s1".into(),
            query: "needle".into(),
            path: Some("src".into()),
            limit: Some(10),
        },
        ApiRequest::FileStatus {
            session_id: "s1".into(),
            path: "src/lib.rs".into(),
        },
        ApiRequest::Ping,
    ];
    for req in reqs {
        let frame = ClientFrame::new(1, req);
        let json = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
    }
}

#[test]
fn client_handshake_over_in_memory_pipe() {
    // Server side scripted: one hello_ok line.
    let reply = serde_json::to_string(&ServerFrame::reply(
        1,
        ApiEvent::HelloOk {
            version: 1,
            server: "jcode/test".into(),
            capabilities: vec!["sessions".into()],
        },
    ))
    .unwrap()
        + "\n";
    let mut out: Vec<u8> = Vec::new();
    let mut client = HarnessClient::new(std::io::BufReader::new(reply.as_bytes()), &mut out);
    let frame = client.hello("test-client/0.1").unwrap();
    match frame.event {
        ApiEvent::HelloOk { version, .. } => assert_eq!(version, 1),
        other => panic!("unexpected event: {other:?}"),
    }
    let sent = String::from_utf8(out).unwrap();
    assert!(sent.contains(r#""req":"hello""#), "sent: {sent}");
}

#[test]
fn session_recovery_wire_shape_roundtrips_optional_notice() {
    for reconnect_notice in [None, Some("reconnected".into())] {
        let frame = ServerFrame::event(ApiEvent::SessionRecovery {
            session_id: "s1".into(),
            continuation_message: "continue task".into(),
            reconnect_notice: reconnect_notice.clone(),
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["ev"], "session_recovery");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(wire["continuation_message"], "continue task");
        assert_eq!(
            wire.get("reconnect_notice").is_some(),
            reconnect_notice.is_some()
        );
        assert!(wire.get("reply_to").is_none());
        assert_eq!(serde_json::from_value::<ServerFrame>(wire).unwrap(), frame);
    }
}

#[test]
fn hidden_system_reminder_wire_shape_and_legacy_default() {
    let frame = ClientFrame::new(
        12,
        ApiRequest::SendMessage {
            session_id: "s1".into(),
            content: String::new(),
            system_reminder: Some("continue task".into()),
            images: vec![],
            no_reply: false,
        },
    );
    let wire = serde_json::to_value(&frame).unwrap();
    assert_eq!(wire["content"], "");
    assert_eq!(wire["system_reminder"], "continue task");
    assert!(wire.get("no_reply").is_none());
    assert_eq!(serde_json::from_value::<ClientFrame>(wire).unwrap(), frame);
    let legacy: ClientFrame = serde_json::from_str(
        r#"{"v":1,"id":13,"req":"send_message","session_id":"s1","content":"hello"}"#,
    )
    .unwrap();
    assert!(matches!(
        legacy.request,
        ApiRequest::SendMessage {
            system_reminder: None,
            ..
        }
    ));
}

#[test]
fn side_panel_state_shared_types_roundtrip() {
    let snapshot = SidePanelSnapshot {
        focus_revision: 0,
        focused_page_id: Some("notes".into()),
        pages: vec![SidePanelPage {
            id: "notes".into(),
            title: "Notes".into(),
            file_path: "/notes.md".into(),
            content: "# Hello\n```mermaid\ngraph LR; A-->B\n```".into(),
            source: SidePanelPageSource::LinkedFile,
            updated_at_ms: 42,
            ..Default::default()
        }],
    };
    let mut pdf_snapshot = snapshot.clone();
    pdf_snapshot.focus_revision = 123;
    pdf_snapshot.pages[0].format = jcode_side_panel_types::SidePanelPageFormat::Pdf;
    pdf_snapshot.pages[0].pdf_data = Some("JVBERi0xLjQKJSVFT0Y=".into());
    pdf_snapshot.pages[0].content = "PDF document fallback".into();
    for snapshot in [snapshot, pdf_snapshot, SidePanelSnapshot::default()] {
        let frame = ServerFrame::event(ApiEvent::SidePanelState {
            session_id: "s1".into(),
            snapshot,
        });
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["ev"], "side_panel_state");
        assert_eq!(wire["session_id"], "s1");
        assert_eq!(serde_json::from_value::<ServerFrame>(wire).unwrap(), frame);
    }
}

#[test]
fn text_framing_is_additive_and_accepts_unframed_legacy_deltas() {
    let old = r#"{"v":1,"ev":"text_delta","session_id":"s1","text":"hello"}"#;
    let frame: ServerFrame = serde_json::from_str(old).unwrap();
    assert!(matches!(
        frame.event,
        ApiEvent::TextDelta {
            message_id: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_string(&frame).unwrap(), old);
    for event in [
        ApiEvent::TextDelta {
            session_id: "s1".into(),
            text: "hi".into(),
            message_id: Some("m1".into()),
        },
        ApiEvent::TextDone {
            session_id: "s1".into(),
            message_id: Some("m1".into()),
        },
        ApiEvent::TextReplace {
            session_id: "s1".into(),
            text: "".into(),
            message_id: Some("m1".into()),
        },
    ] {
        let frame = ServerFrame::event(event);
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["message_id"], "m1");
        assert_eq!(
            serde_json::from_value::<ServerFrame>(wire).unwrap().event,
            frame.event
        );
    }
}

#[test]
fn session_tool_control_wire_shapes_and_defaults() {
    use serde_json::json;
    for tools in [
        json!({}),
        json!({"enabled":null}),
        json!({"enabled":[]}),
        json!({"enabled":["read"],"disabled":["bash"],"custom":[{"name":"lookup","description":"Look up","parameters":{"type":"object"}}]}),
    ] {
        let wire = json!({"v":1,"id":1,"req":"configure_tools","session_id":"s1","tools":tools});
        let frame: ClientFrame = serde_json::from_value(wire).unwrap();
        let ApiRequest::ConfigureTools { tools: config, .. } = &frame.request else {
            panic!()
        };
        assert_eq!(
            config.enabled,
            tools
                .get("enabled")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()).unwrap())
        );
        assert_eq!(
            serde_json::from_value::<ClientFrame>(serde_json::to_value(&frame).unwrap()).unwrap(),
            frame
        );
    }
    for wire in [
        json!({"v":1,"id":2,"req":"list_tools","session_id":"s1"}),
        json!({"v":1,"id":3,"req":"tool_result","session_id":"s1","call_id":"c1","output":"ok"}),
        json!({"v":1,"id":4,"req":"tool_result","session_id":"s1","call_id":"c1","output":"","error":"failed"}),
    ] {
        let frame: ClientFrame = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(frame).unwrap(), wire);
    }
    for wire in [
        json!({"v":1,"reply_to":2,"ev":"tools","session_id":"s1","tools":[{"name":"read","description":"Read file","parameters":{"type":"object"}}]}),
        json!({"v":1,"ev":"tool_call","session_id":"s1","call_id":"c1","name":"lookup","input":{"key":1}}),
    ] {
        let frame: ServerFrame = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(frame).unwrap(), wire);
    }
    for parameters in [json!(null), json!([]), json!("object"), json!(42)] {
        assert!(
            serde_json::from_value::<SessionToolDefinition>(
                json!({"name":"bad","description":"bad","parameters":parameters})
            )
            .is_err()
        );
    }
    assert_eq!(
        serde_json::to_value(ToolConfiguration::default()).unwrap(),
        json!({})
    );
}

#[test]
fn turn_stopped_schema_and_future_reason_compatibility() {
    for reason in [
        "interrupted",
        "failure",
        "crash",
        "provider_guardrail",
        "limit_reached",
    ] {
        let wire = serde_json::json!({"v":1,"ev":"turn_stopped","session_id":"s1","reason":reason,"message":"Explanation"});
        let frame: ServerFrame = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(frame).unwrap(), wire);
    }
    let wire = serde_json::json!({"v":1,"ev":"turn_stopped","session_id":"s1","reason":"future_reason","message":"Explanation","provider_stop_reason":"refusal"});
    let frame: ServerFrame = serde_json::from_value(wire).unwrap();
    assert!(
        matches!(frame.event, ApiEvent::TurnStopped { reason: TurnStopReason::Unknown, provider_stop_reason: Some(reason), .. } if reason == "refusal")
    );
    let done: ServerFrame =
        serde_json::from_value(serde_json::json!({"v":1,"ev":"turn_done","session_id":"s1"}))
            .unwrap();
    assert!(matches!(done.event, ApiEvent::TurnDone { .. }));
}
