//! Repair malformed streamed tool-call arguments, ported from Hermes'
//! `_repair_tool_call_arguments` (agent/message_sanitization.py): control
//! chars in strings, trailing commas, truncated/excess/misnested closers.
//! Unrepairable input stays `Null` so the caller's "invalid arguments"
//! error path still runs.

use jcode_message_types::ToolCall;
use serde_json::Value;

pub(super) fn parse_streamed_tool_input(raw: &str) -> Value {
    let parsed = ToolCall::parse_streamed_input_to_object(raw);
    if parsed != Value::Null {
        return parsed;
    }
    if raw.trim() == "None" {
        return Value::Object(Default::default());
    }
    match repair(raw.trim()).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        Some(v @ Value::Object(_)) => v,
        _ => Value::Null,
    }
}

/// True when the arguments only parse because `repair` fixed them up
/// (closed brackets, dropped commas, ...). Such calls are untrusted when the
/// turn was cut off by the output limit: a closed-over write/edit body is
/// truncated content.
pub(super) fn needed_repair(raw: &str) -> bool {
    ToolCall::parse_streamed_input_to_object(raw) == Value::Null
        && parse_streamed_tool_input(raw) != Value::Null
}

fn repair(raw: &str) -> Option<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            match c {
                '\\' => {
                    out.push(c);
                    if let Some(n) = chars.get(i + 1) {
                        out.push(*n);
                    }
                    i += 2;
                    continue;
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                _ => out.push(c),
            }
        } else {
            match c {
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                '{' => {
                    stack.push('}');
                    out.push(c);
                }
                '[' => {
                    stack.push(']');
                    out.push(c);
                }
                ',' => {
                    let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                    if !matches!(next, Some('}' | ']') | None) {
                        out.push(c);
                    }
                }
                '}' | ']' => {
                    // Close misnested openers first; drop closers with no opener.
                    if let Some(pos) = stack.iter().rposition(|&k| k == c) {
                        while stack.len() > pos + 1 {
                            out.push(stack.pop().unwrap());
                        }
                        stack.pop();
                        out.push(c);
                    }
                }
                _ => out.push(c),
            }
        }
        i += 1;
    }
    if in_string {
        return None;
    }
    while let Some(k) = stack.pop() {
        out.push(k);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> Value {
        parse_streamed_tool_input(s)
    }

    #[test]
    fn repairs_hermes_cases() {
        assert_eq!(p(r#"{"a": [1, 2,]}"#), json!({"a": [1, 2]}));
        assert_eq!(p(r#"{"code": "}", "x": 1"#), json!({"code": "}", "x": 1}));
        assert_eq!(
            p(r#"{"items": [{"n": 1}, {"n": 2"#),
            json!({"items": [{"n": 1}, {"n": 2}]})
        );
        assert_eq!(p("{\"a\": \"x\ty\"}"), json!({"a": "x\ty"}));
        assert_eq!(p("None"), json!({}));
        assert_eq!(
            p(r#"{"a": [{"b": 1}, {"c": 2}}]}"#),
            json!({"a": [{"b": 1}, {"c": 2}]})
        );
        assert_eq!(p(r#"{"a": [1, 2}"#), json!({"a": [1, 2]}));
        assert_eq!(
            p(r#"{"tool": "edit", "args": {"items": [{"k": 1}, {"k": 2}}}}"#),
            json!({"tool": "edit", "args": {"items": [{"k": 1}, {"k": 2}]}})
        );
    }

    #[test]
    fn needed_repair_only_for_repaired_input() {
        assert!(!needed_repair(r#"{"a": 1}"#));
        assert!(!needed_repair(""));
        assert!(needed_repair(r#"{"content": "x", "p": [1"#));
        assert!(!needed_repair(r#"{"truncated": "val"#));
    }

    #[test]
    fn unrepairable_stays_null_and_valid_is_untouched() {
        assert_eq!(p(r#"{"truncated": "val"#), Value::Null);
        assert_eq!(p("garbage no json"), Value::Null);
        assert_eq!(p(r#"{"a": 1}"#), json!({"a": 1}));
        assert_eq!(p(""), json!({}));
    }
}
