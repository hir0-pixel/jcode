//! Materializing a Continual Harness `skill` entry as an importable
//! `SKILL.md` under `~/.jcode/skills/<slug>/`, so a skill created or updated
//! through `/refine` is executable by the existing Python worker (the same
//! bundled-skill layout `bundled_skills.rs` installs and the worker adds to
//! `sys.path`) instead of staying inert text in `harness_entries`.
//!
//! `/refine` edits are capped and tracked by changesets (see
//! [`crate::refine::apply`]), so this module only validates Prime's
//! executable contract and writes the file.

use serde_json::Value;
use std::path::{Path, PathBuf};

pub const MAX_NAME_CHARS: usize = 64;

/// `kebab-case` a proposed skill name into a filesystem- and import-safe slug.
pub fn slugify(name: &str) -> String {
    let raw: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    raw.split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// No absolute per-user paths and no obvious credential material in a
/// proposed skill body, description, or reference.
pub fn looks_unsafe(text: &str) -> bool {
    let lower = text.to_lowercase();
    let has_home_path =
        text.contains("/Users/") || text.contains("/home/") || lower.contains(r"c:\users");
    let secret_markers = [
        "api_key",
        "apikey",
        "secret",
        "password",
        "-----begin",
        "bearer ",
        "sk-ant-",
        "sk-proj-",
    ];
    has_home_path || secret_markers.iter().any(|m| lower.contains(m))
}

pub fn skills_dir() -> Option<PathBuf> {
    jcode_storage::jcode_dir().ok().map(|d| d.join("skills"))
}

/// Prime's mandatory executable contract for a `skill` entry
/// (`refinement.ts` `validateEdit`): a Python reference with an import and a
/// callable (or call pattern), plus an `arguments` object (`{}` when the
/// callable truly takes none). Returns the rejection reason, or `None` if
/// the contract is satisfied.
pub fn validate_reference(reference: &Value, arguments: &Value) -> Option<String> {
    if !arguments.is_object() {
        return Some("skill edit requires an arguments object".into());
    }
    let Some(reference) = reference.as_object() else {
        return Some("skill edit requires a reference object".into());
    };
    if reference.get("type").and_then(Value::as_str) != Some("python") {
        return Some("skill edit requires reference.type to be \"python\"".into());
    }
    let has_import = reference
        .get("import")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        || reference
            .get("python_import")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    if !has_import {
        return Some("skill edit requires reference.import (the Python module)".into());
    }
    let has_callable = reference
        .get("callable")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        || reference
            .get("call_pattern")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
    if !has_callable {
        return Some("skill edit requires reference.callable or reference.call_pattern".into());
    }
    None
}

/// Write (or overwrite) `~/.jcode/skills/<slug>/SKILL.md` for a gated skill
/// create/update. The Python import/callable contract is recorded in the
/// body so the model sees exactly how to invoke it (`await <import>(...)`),
/// alongside the harness row's own `reference`/`arguments` columns.
pub fn write(dir: &Path, slug: &str, name: &str, description: &str, body: &str, reference: &Value) -> std::io::Result<()> {
    let skill_dir = dir.join(slug);
    std::fs::create_dir_all(&skill_dir)?;
    let import = reference["import"]
        .as_str()
        .or_else(|| reference["python_import"].as_str())
        .unwrap_or_default();
    let callable = reference["callable"]
        .as_str()
        .or_else(|| reference["call_pattern"].as_str())
        .unwrap_or_default();
    let call_form = if reference["callable"].as_str().is_some() {
        format!("await {import}.{callable}(...)")
    } else {
        callable.to_string()
    };
    let content = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n{}\n\nRLM call: `{call_form}`\n",
        body.trim()
    );
    std::fs::write(skill_dir.join("SKILL.md"), content)
}
