//! Deterministic pruning of old tool output, ported from Hermes'
//! `_prune_old_tool_results` (agent/context_compressor.py). Only block
//! contents are rewritten; blocks are never added or removed, so every
//! tool_use keeps its tool_result for every provider.

use crate::truncate_str_boundary;
use jcode_message_types::{ContentBlock, Message};
use std::collections::{HashMap, HashSet};

const PRUNE_MIN_CHARS: usize = 200;
const ARG_MAX_CHARS: usize = 500;
const PRESERVED_ARGS: [&str; 6] = ["old_string", "new_string", "content", "patch", "diff", "edits"];
const DUPLICATE: &str = "[Duplicate tool output - same content as a more recent call]";

fn is_placeholder(s: &str) -> bool {
    s.starts_with("[Duplicate tool output") || s.starts_with("[pruned ")
}

/// Prune old tool results in `messages`, leaving the last `protect_tail`
/// messages untouched (dedupe also spares the tail and keeps the newest copy). Returns
/// the number of edits made.
pub fn prune_old_tool_results(messages: &mut [Message], protect_tail: usize) -> usize {
    let boundary = messages.len().saturating_sub(protect_tail);
    let mut edits = 0;

    // Pass 1: dedupe byte-identical results, keeping the newest copy.
    let mut seen = HashSet::new();
    for (i, msg) in messages.iter_mut().enumerate().rev() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block
                && content.len() >= PRUNE_MIN_CHARS
                && !is_placeholder(content)
                && !seen.insert(content.clone())
                && i < boundary
            {
                *content = DUPLICATE.to_string();
                edits += 1;
            }
        }
    }

    // id -> (name, one-line args) for the gist.
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    for msg in messages.iter() {
        for block in &msg.content {
            if let ContentBlock::ToolUse {
                id, name, input, ..
            } = block
            {
                let args = input.to_string();
                calls.insert(
                    id.clone(),
                    (name.clone(), truncate_str_boundary(&args, 80).to_string()),
                );
            }
        }
    }

    for msg in messages[..boundary].iter_mut() {
        for block in msg.content.iter_mut() {
            match block {
                // Pass 2: old large results become a one-line gist.
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if content.len() > PRUNE_MIN_CHARS && !is_placeholder(content) => {
                    let (name, args) = calls.get(tool_use_id.as_str()).cloned().unwrap_or_default();
                    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                    *content = format!(
                        "[pruned {name} {args}] {} ({} chars)",
                        truncate_str_boundary(first, 80),
                        content.len()
                    );
                    edits += 1;
                }
                // Pass 3: shrink big string args inside the JSON so it stays valid.
                ContentBlock::ToolUse { input, .. } => {
                    edits += shrink_strings(input);
                }
                _ => {}
            }
        }
    }
    edits
}

fn shrink_strings(v: &mut serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) if s.len() > ARG_MAX_CHARS => {
            let cut = truncate_str_boundary(s, ARG_MAX_CHARS).to_string();
            *s = format!("{cut}...[{} chars truncated]", s.len() - cut.len());
            1
        }
        serde_json::Value::Array(a) => a.iter_mut().map(shrink_strings).sum(),
        serde_json::Value::Object(o) => o
            .iter_mut()
            // The model copies its own earlier edit bodies; keep them whole.
            .filter(|(k, _)| !PRESERVED_ARGS.contains(&k.as_str()))
            .map(|(_, v)| shrink_strings(v))
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_char_count;
    use jcode_message_types::Role;

    fn msg(role: Role, block: ContentBlock) -> Message {
        Message {
            role,
            content: vec![block],
            timestamp: None,
            tool_duration_ms: None,
        }
    }
    fn call(id: &str, input: serde_json::Value) -> Message {
        msg(
            Role::Assistant,
            ContentBlock::ToolUse {
                id: id.into(),
                name: "read".into(),
                input,
                thought_signature: None,
            },
        )
    }
    fn result(id: &str, content: &str) -> Message {
        msg(
            Role::User,
            ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: None,
            },
        )
    }
    fn res(m: &Message) -> &str {
        match &m.content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!(),
        }
    }
    fn ids(ms: &[Message]) -> Vec<(bool, String)> {
        ms.iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some((true, id.clone())),
                ContentBlock::ToolResult { tool_use_id, .. } => Some((false, tool_use_id.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn prunes_old_keeps_tail_and_pairing() {
        let big = "line one\n".to_string() + &"x".repeat(5000);
        let big2 = "other\n".to_string() + &"y".repeat(5000);
        let mut ms = vec![
            call(
                "a",
                serde_json::json!({"path":"f","other":"z".repeat(3000)}),
            ),
            result("a", &big),
            call("b", serde_json::json!({})),
            result("b", &big2),
        ];
        let before_ids = ids(&ms);
        let before = message_char_count(&ms[0]) + message_char_count(&ms[1]);
        let tail = ms[2..].to_vec();
        assert!(prune_old_tool_results(&mut ms, 2) > 0);
        assert_eq!(ids(&ms), before_ids);
        assert!(res(&ms[1]).starts_with("[pruned read"));
        assert!(message_char_count(&ms[0]) + message_char_count(&ms[1]) < before / 4);
        assert_eq!(format!("{:?}", &ms[2..]), format!("{:?}", tail));
    }

    #[test]
    fn dedupes_identical_results_keeping_newest() {
        let big = "same ".repeat(100);
        let mut ms = vec![
            call("a", serde_json::json!({})),
            result("a", &big),
            call("b", serde_json::json!({})),
            result("b", &big),
        ];
        prune_old_tool_results(&mut ms, 2);
        assert!(res(&ms[1]).starts_with("[Duplicate"));
        assert_eq!(res(&ms[3]), big);
    }

    #[test]
    fn dedupe_spares_protected_tail_and_edit_bodies_survive() {
        let big = "same ".repeat(100);
        let body = "e".repeat(3000);
        let mut ms = vec![
            call("a", serde_json::json!({"old_string": body, "new_string": body})),
            result("a", &big),
            call("b", serde_json::json!({})),
            result("b", &big),
            call("c", serde_json::json!({})),
            result("c", &big),
        ];
        prune_old_tool_results(&mut ms, 4);
        assert_eq!(res(&ms[3]), big, "tail result kept");
        assert_eq!(res(&ms[5]), big);
        assert!(res(&ms[1]).starts_with("[Duplicate"));
        let ContentBlock::ToolUse { input, .. } = &ms[0].content[0] else { panic!() };
        assert_eq!(input["old_string"].as_str().unwrap().len(), 3000);
        assert_eq!(input["new_string"].as_str().unwrap().len(), 3000);
    }
}
