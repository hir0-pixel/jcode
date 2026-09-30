//! Slash commands the engine does not own are answered by Hermes's Python
//! backend (`command.dispatch` needs no Python session for quick, plugin,
//! bundle and skill commands, or the prompt builders /learn /plan /init /queue).
//!
//! Commands that act on chat or session state are engine-served and never
//! forwarded: Python has no record of these sessions, so it could only act on
//! an unrelated (or missing) session.

use serde_json::{Value, json};

/// Hermes built-ins that are never forwarded: they mutate the live chat (history, model, queue,
/// checkpoints), or the engine owns the job (/refine learns, engine memory has no Python write
/// path; skill hub and curator stay reachable through Hermes's REST routes and settings).
pub(crate) const SESSION_BOUND: &[&str] = &[
    "moa", "focus", "retry", "steer", "undo", "snapshot", "snap", "compress", "compact",
    "curator", "skills", "memory",
];

/// Commands the engine's own `harness_command` serves (first word of each pair).
fn engine_key(pair: &Value) -> Option<&str> {
    pair[0].as_str()?.split_whitespace().next()
}

fn bound(key: &str) -> bool {
    SESSION_BOUND.contains(&key.trim_start_matches('/'))
}

/// Engine catalog first, then Hermes's; a Hermes entry the engine owns, or that
/// needs a Hermes session, is dropped.
pub(crate) fn merge_catalog(engine: Value, python: &Value) -> Value {
    let mut out = engine;
    let owned: Vec<String> = out["pairs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(engine_key)
        .map(str::to_string)
        .collect();
    let keep = |pair: &Value| {
        pair[0].as_str().is_some_and(|k| {
            let word = k.split_whitespace().next().unwrap_or(k);
            !bound(word) && !owned.iter().any(|o| o == word)
        })
    };
    let rows = |v: &Value| -> Vec<Value> { v.as_array().into_iter().flatten().filter(|p| keep(p)).cloned().collect() };
    let mut pairs = out["pairs"].as_array().cloned().unwrap_or_default();
    pairs.extend(rows(&python["pairs"]));
    let mut categories = out["categories"].as_array().cloned().unwrap_or_default();
    for cat in python["categories"].as_array().into_iter().flatten() {
        let cat_pairs = rows(&cat["pairs"]);
        if !cat_pairs.is_empty() {
            categories.push(json!({ "name": cat["name"], "pairs": cat_pairs }));
        }
    }
    let visible = |k: &String| !bound(k) && !owned.iter().any(|o| o == k);
    let pick = |v: &Value| -> Value {
        Value::Object(
            v.as_object().into_iter().flatten().filter(|(k, _)| visible(k)).map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
    };
    out["pairs"] = json!(pairs);
    out["categories"] = json!(categories);
    out["sub"] = pick(&python["sub"]);
    out["canon"] = pick(&python["canon"]);
    out["commands"] = pick(&python["commands"]);
    out["warning"] = python["warning"].clone();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_catalog_merges_without_engine_owned_or_session_bound_commands() {
        let engine = json!({
            "pairs": [["/goal", "engine goal"], ["/refine status", "x"]],
            "categories": [{ "name": "Harness", "pairs": [["/goal", "engine goal"]] }],
            "skills": {}, "skill_count": 3,
        });
        let python = json!({
            "pairs": [["/goal", "hermes goal"], ["/undo", "u"], ["/learn", "l"], ["/plan", "p"], ["/init", "i"], ["/tools", "t"], ["/my-skill", "s"]],
            "categories": [{ "name": "Tools", "pairs": [["/tools", "t"], ["/undo", "u"]] }, { "name": "Empty", "pairs": [["/goal", "g"]] }],
            "sub": { "/tools": ["list"], "/undo": [] },
            "canon": { "/t": "/tools", "/goal": "/goal", "/undo": "/undo" },
            "commands": { "/tools": {"argument_mode": null} },
            "skills": { "/my-skill": {"usage": 1} }, "skill_count": 9, "warning": "w",
        });
        let out = merge_catalog(engine, &python);
        let keys: Vec<&str> = out["pairs"].as_array().unwrap().iter().map(|p| p[0].as_str().unwrap()).collect();
        assert_eq!(keys, ["/goal", "/refine status", "/learn", "/plan", "/init", "/tools", "/my-skill"]);
        assert_eq!(out["pairs"][0][1], "engine goal");
        let cats: Vec<&str> = out["categories"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(cats, ["Harness", "Tools"]);
        assert_eq!(out["canon"], json!({ "/t": "/tools" }));
        assert_eq!(out["sub"], json!({ "/tools": ["list"] }));
        for k in ["learn", "plan", "init"] {
            assert!(!bound(k), "/{k} is a session-agnostic prompt builder and must stay forwardable");
        }
        assert_eq!(out["skills"], json!({}), "the slash list matches the engine's skills");
        assert_eq!(out["skill_count"], 3);
    }
}
