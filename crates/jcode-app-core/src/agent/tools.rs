use crate::message::{ContentBlock, ToolCall};
use crate::terminal_println as println;
use crate::tool::ToolOutput;

/// History cap for ordinary tool output (about 50 KB), split head 40% / tail 60%
/// as in Hermes' `tools/tool_output_truncate.py`. The full text is spilled to a file
/// (Prime's `truncate.ts` does the same) and the path goes in the note.
pub(super) const MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY: usize = 50 * 1024;
const HEAD_RATIO: f64 = 0.4;

/// Tools that page or limit their own output, and the tool the model uses to read a
/// spilled file back. They keep the old protective ceiling and never spill, so reading
/// a spill file cannot truncate and spill again.
const SELF_LIMITED_TOOLS: &[&str] = &["read", "webfetch", "agentgrep", "grep", "glob", "ls"];
const SELF_LIMITED_CEILING_CHARS: usize = 512 * 1024;

fn spill_dir() -> Option<std::path::PathBuf> {
    let dir = jcode_base::storage::jcode_dir().ok()?.join("tool-output");
    std::fs::create_dir_all(&dir).ok()?;
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| sweep_old_spills(&dir));
    Some(dir)
}

/// Delete spill files older than 7 days (once per process, on first use).
fn sweep_old_spills(dir: &std::path::Path) {
    let cutoff = std::time::Duration::from_secs(7 * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > cutoff);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Same tool + same content => same file name, so a repeated output neither writes
/// another file nor changes the capped text (which the repeat guard hashes).
fn spill_full_output(tool_name: &str, text: &str) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let safe: String = tool_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = spill_dir()?.join(format!("{safe}-{:016x}.txt", h.finish()));
    if !path.exists() {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&path).ok()?.write_all(text.as_bytes()).ok()?;
    }
    Some(path)
}

fn char_boundary_prefix(s: &str, chars: usize) -> &str {
    s.char_indices().nth(chars).map_or(s, |(i, _)| &s[..i])
}

fn char_boundary_suffix(s: &str, chars: usize) -> &str {
    let total = s.chars().count();
    if chars >= total {
        return s;
    }
    s.char_indices().nth(total - chars).map_or("", |(i, _)| &s[i..])
}

/// Returns the text unchanged when it fits, else head + note + tail.
fn cap_text_for_history(tool_name: &str, text: &str) -> Option<String> {
    let total = text.chars().count();
    if SELF_LIMITED_TOOLS.contains(&tool_name) {
        if total <= SELF_LIMITED_CEILING_CHARS {
            return None;
        }
        let kept = char_boundary_prefix(text, SELF_LIMITED_CEILING_CHARS);
        return Some(format!(
            "{kept}\n\n[Tool output truncated by jcode: tool `{tool_name}` produced {total} chars; kept first {SELF_LIMITED_CEILING_CHARS}. Use offset/limit or a narrower query.]"
        ));
    }
    if total <= MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY {
        return None;
    }
    let head = (MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY as f64 * HEAD_RATIO) as usize;
    let tail = MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY - head;
    let omitted = total - head - tail;
    let where_full = match spill_full_output(tool_name, text) {
        Some(path) => format!(
            "full output saved to {}; read it with offset/limit or grep it",
            path.display()
        ),
        None => "full output could not be saved; rerun with a narrower command".to_string(),
    };
    Some(format!(
        "{}\n\n... [TOOL OUTPUT TRUNCATED - {omitted} chars omitted out of {total} total; {where_full}] ...\n\n{}",
        char_boundary_prefix(text, head),
        char_boundary_suffix(text, tail),
    ))
}

pub(super) fn cap_tool_output_for_history(tool_name: &str, mut output: ToolOutput) -> ToolOutput {
    if let Some(capped) = cap_text_for_history(tool_name, &output.output) {
        output.output = capped;
    }
    output
}

pub(super) fn cap_sdk_tool_content_for_history(tool_name: &str, content: String) -> String {
    cap_text_for_history(tool_name, &content).unwrap_or(content)
}

/// Build rendered side-pane images from a tool output's attached images.
///
/// This mirrors how `render_messages_and_images` derives images from persisted
/// session history (source = ToolResult), so live-streamed images match what a
/// later History reload would produce. `tool_name` and `tool_input` provide the
/// label fallback (e.g. the `read` tool's `file_path`); `tool_call_id` anchors
/// the image to its tool message in the transcript.
pub(super) fn tool_output_side_pane_images(
    tool_call_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
    output: &ToolOutput,
) -> Vec<jcode_session_types::RenderedImage> {
    if output.images.is_empty() {
        return Vec::new();
    }
    let fallback_label = tool_input
        .get("file_path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    output
        .images
        .iter()
        .map(|img| jcode_session_types::RenderedImage {
            history_message_index: None,
            media_type: img.media_type.clone(),
            data: img.data.clone(),
            label: img
                .label
                .as_ref()
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty())
                .or_else(|| fallback_label.clone()),
            source: jcode_session_types::RenderedImageSource::ToolResult {
                tool_name: tool_name.to_string(),
            },
            anchor: Some(jcode_session_types::RenderedImageAnchor::ToolCall {
                id: tool_call_id.to_string(),
            }),
        })
        .collect()
}

pub(super) fn tool_output_to_content_blocks(
    tool_use_id: String,
    output: ToolOutput,
) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::ToolResult {
        tool_use_id,
        content: output.output,
        is_error: None,
    }];
    for img in output.images {
        blocks.push(ContentBlock::Image {
            media_type: img.media_type,
            data: img.data,
        });
        if let Some(label) = img.label.filter(|label| !label.trim().is_empty()) {
            blocks.push(ContentBlock::Text {
                text: format!(
                    "[Attached image associated with the preceding tool result: {}]",
                    label
                ),
                cache_control: None,
            });
        }
    }
    blocks
}

pub(super) fn print_tool_summary(tool: &ToolCall) {
    match tool.name.as_str() {
        "bash" => {
            if let Some(cmd) = tool.input.get("command").and_then(|v| v.as_str()) {
                let short = if cmd.len() > 60 {
                    format!("{}...", crate::util::truncate_str(cmd, 60))
                } else {
                    cmd.to_string()
                };
                println!("$ {}", short);
            }
        }
        "read" | "write" | "edit" => {
            if let Some(path) = tool.input.get("file_path").and_then(|v| v.as_str()) {
                println!("{}", path);
            }
        }
        "glob" | "grep" => {
            if let Some(pattern) = tool.input.get("pattern").and_then(|v| v.as_str()) {
                println!("'{}'", pattern);
            }
        }
        "ls" => {
            let path = tool
                .input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            println!("{}", path);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_diff_text_survives_history_conversion_and_serialization() {
        let text =
            "Edited f\n\nFile diff:\n```diff\n--- f\n+++ f\n@@ -39,1 +39,1 @@\n-old\n+new\n```\n";
        let blocks = tool_output_to_content_blocks(
            "edit-call".into(),
            cap_tool_output_for_history("edit", ToolOutput::new(text)),
        );
        let serialized = serde_json::to_string(&blocks).unwrap();
        let restored: Vec<ContentBlock> = serde_json::from_str(&serialized).unwrap();
        assert!(
            matches!(&restored[0], ContentBlock::ToolResult { content, tool_use_id, .. }
            if content == text && tool_use_id == "edit-call")
        );
    }

    #[test]
    fn cap_tool_output_leaves_small_output_unchanged() {
        let output = ToolOutput::new("short output");
        let capped = cap_tool_output_for_history("bash", output.clone());
        assert_eq!(capped.output, output.output);
    }
}

#[cfg(test)]
mod image_anchor_tests {
    use super::*;

    #[test]
    fn live_batch_images_anchor_to_parent_and_have_no_history_boundary() {
        let output = ToolOutput::new("batch results")
            .with_labeled_image("image/png", "one", "first.png")
            .with_labeled_image("image/png", "two", "second.png");
        let images =
            tool_output_side_pane_images("parent-batch", "batch", &serde_json::json!({}), &output);
        assert_eq!(images.len(), 2);
        for image in &images {
            assert_eq!(
                image.anchor,
                Some(jcode_session_types::RenderedImageAnchor::ToolCall {
                    id: "parent-batch".into()
                })
            );
            assert_eq!(image.history_message_index, None);
        }
        assert_eq!(images[0].data, "one");
        assert_eq!(images[1].data, "two");
        assert_eq!(images[0].label.as_deref(), Some("first.png"));
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    #[test]
    fn oversized_output_keeps_head_and_tail_and_spills_full_text() {
        let _env = jcode_base::storage::lock_test_env();
        let text = format!("HEAD{}TAIL", "x".repeat(200_000));
        let capped = cap_tool_output_for_history("bash", ToolOutput::new(text.clone())).output;
        assert!(capped.starts_with("HEAD"));
        assert!(capped.ends_with("TAIL"));
        assert!(capped.chars().count() < MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY + 400);
        let path = capped
            .split("saved to ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .expect("spill path in note");
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[test]
    fn small_output_and_self_limited_tools_are_untouched() {
        let _env = jcode_base::storage::lock_test_env();
        let mid = "y".repeat(100_000);
        assert_eq!(cap_tool_output_for_history("bash", ToolOutput::new("ok")).output, "ok");
        assert_eq!(cap_tool_output_for_history("read", ToolOutput::new(mid.clone())).output, mid);
    }

    #[test]
    fn identical_oversized_output_reuses_one_private_spill_file() {
        let _env = jcode_base::storage::lock_test_env();
        let text = "z".repeat(120_000);
        let a = cap_tool_output_for_history("bash", ToolOutput::new(text.clone())).output;
        let b = cap_tool_output_for_history("bash", ToolOutput::new(text)).output;
        assert_eq!(a, b, "repeat guard hashes the capped text; it must be stable");
        let path = a.split("saved to ").nth(1).and_then(|r| r.split(';').next()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn multibyte_text_splits_on_char_boundaries() {
        let _env = jcode_base::storage::lock_test_env();
        let text = "é".repeat(70_000);
        let capped = cap_sdk_tool_content_for_history("bash", text);
        assert!(capped.contains("TRUNCATED"));
    }
}
