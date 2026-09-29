//! `llm.oneshot`: a stateless model call outside any conversation (commit messages, titles,
//! project ideas). Served through the engine's own model and key so it is traced and billed
//! like every other call, instead of a second model path in Hermes's Python.

use serde_json::Value;

const COMMIT_INSTRUCTIONS: &str = "You write git commit messages. Given a diff of staged changes, write ONE concise Conventional Commits message describing what the change does and why.\nRules:\n- Subject line: type(scope): summary — imperative mood, lower-case, no trailing period, ≤ 72 characters. Types: feat, fix, refactor, perf, docs, test, build, chore, style, ci.\n- Omit the scope if it isn't obvious.\n- Add a short body (wrapped at ~72 cols) ONLY when the change needs explanation; skip it for small/obvious changes.\n- Describe the actual change, never restate the diff line-by-line.\n- Return ONLY the commit message text — no quotes, no markdown fences, no preamble.";

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}\n…(truncated)", text.chars().take(limit).collect::<String>().trim_end())
}

fn commit_message(vars: &Value) -> (String, String) {
    let text = |key: &str| vars[key].as_str().unwrap_or_default();
    let mut parts = Vec::new();
    let recent = truncate(text("recent_commits"), 1500);
    if !recent.trim().is_empty() {
        parts.push(format!("Recent commit subjects from this repo (match their style/conventions):\n{recent}"));
    }
    let diff = truncate(text("diff"), 12000);
    parts.push(format!("Diff to describe:\n{}", if diff.is_empty() { "(no textual diff available)" } else { &diff }));
    let avoid = truncate(text("avoid").trim(), 1000);
    if !avoid.is_empty() {
        parts.push(format!("You already proposed the message below and the user wants a different one. Write a NEW message with different wording (and, if reasonable, a different emphasis or scope framing) — do not repeat it:\n{avoid}"));
    }
    (COMMIT_INSTRUCTIONS.to_string(), parts.join("\n\n"))
}

/// (system, user) for the call, or (Hermes error code, message).
pub(crate) fn prompts(p: &Value) -> Result<(String, String), (i64, String)> {
    let template = p["template"].as_str().map(str::trim).filter(|t| !t.is_empty());
    let (instructions, input) = match template {
        Some("commit_message") => commit_message(&p["variables"]),
        Some(other) => return Err((4031, format!("unknown one-shot template: {other}"))),
        None => (p["instructions"].as_str().unwrap_or_default().to_string(), p["input"].as_str().unwrap_or_default().to_string()),
    };
    if instructions.trim().is_empty() && input.trim().is_empty() {
        return Err((4030, "llm.oneshot requires a template or instructions/input".into()));
    }
    Ok((instructions, input))
}

/// Drop a single wrapping ``` fence the model may have added.
pub(crate) fn strip_code_fence(text: &str) -> String {
    let text = text.trim();
    let lines: Vec<&str> = text.lines().collect();
    if text.starts_with("```") && lines.len() >= 2 && lines[lines.len() - 1].trim() == "```" {
        return lines[1..lines.len() - 1].join("\n").trim().to_string();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn oneshot_builds_the_same_prompts_hermes_did() {
        let (system, user) = prompts(&json!({"instructions": "Be brief.", "input": "Project name: x"})).unwrap();
        assert_eq!((system.as_str(), user.as_str()), ("Be brief.", "Project name: x"));
        let (system, user) = prompts(&json!({"template": "commit_message", "variables": {"diff": "+a", "avoid": "feat: old"}})).unwrap();
        assert!(system.starts_with("You write git commit messages") && user.starts_with("Diff to describe:\n+a"));
        assert!(user.contains("do not repeat it:\nfeat: old"));
        assert_eq!(prompts(&json!({"template": "nope"})).unwrap_err().0, 4031);
        assert_eq!(prompts(&json!({"input": "  "})).unwrap_err().0, 4030);
        assert_eq!(strip_code_fence("```\nfeat: x\n```"), "feat: x");
        assert_eq!(strip_code_fence(" plain "), "plain");
    }
}
