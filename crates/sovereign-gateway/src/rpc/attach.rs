//! Attachment RPCs (`image.*`, `file.attach`, `pdf.attach`, `clipboard.paste`,
//! `input.detect_drop`, `message.react`), answered by the engine with the shapes
//! Hermes's `methods_prompt.py` returns. State lives on disk under
//! `<home>/attachments/<session>/` so it survives across connections; images
//! staged there ride along on the session's next `prompt.submit`.

use base64::Engine as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const BYTES_MAX: usize = 25 * 1024 * 1024;
/// Raw bytes per image. jcode keeps images as base64 in the session JSON (x4/3) and Anthropic
/// rejects any image over 10 MB of base64 (`provider/image_clamp.rs`), so 7 MB raw (~9.3 MB
/// encoded) is what every provider takes. Hermes caps attaches at 25 MB and never downscales.
const IMAGE_MAX: usize = 7 * 1024 * 1024;
const PDF_MAX: u64 = 50 * 1024 * 1024;

/// The biggest decoded payload any attach RPC accepts (`file.attach` allows twice `BYTES_MAX`).
const LARGEST_PAYLOAD: usize = if BYTES_MAX * 2 > PDF_MAX as usize { BYTES_MAX * 2 } else { PDF_MAX as usize };
/// The biggest WebSocket message an attach can be: that payload as base64, plus 1 MiB for the JSON
/// around it. The socket's message cap must not be lower or a big attach closes the connection.
pub(crate) const MAX_WIRE_BYTES: usize = LARGEST_PAYLOAD.div_ceil(3) * 4 + 1024 * 1024;

pub(super) type Failure = (i64, String);
type Reply = Result<Value, Failure>;

fn fail<T>(code: i64, message: impl Into<String>) -> Result<T, Failure> {
    Err((code, message.into()))
}

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "image.attach" | "image.attach_bytes" | "image.detach" | "file.attach" | "pdf.attach"
            | "clipboard.paste" | "input.detect_drop" | "message.react"
    )
}

/// A session's staging directory. `None` for an id that is not a plain `[A-Za-z0-9_-]+` name (empty,
/// or with path characters), so no id can land in another session's directory or the root.
pub(crate) fn stage_dir(home: &str, session: &str) -> Option<PathBuf> {
    let plain = !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    plain.then(|| Path::new(home).join("attachments").join(session))
}

fn read_list(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write_json(file: &Path, value: &impl serde::Serialize) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_vec(value)?)
}

fn pending(dir: &Path) -> Vec<String> {
    read_list(&dir.join("pending.json"))
}

/// One lock per session's staged files, so a read-modify-write of `pending.json` never loses a
/// concurrent attach.
fn stage_lock(dir: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(dir.to_path_buf()).or_default().clone()
}

fn queue_image(dir: &Path, path: &Path) -> Result<usize, Failure> {
    let size = path.metadata().map_or(0, |m| m.len() as usize);
    if size > IMAGE_MAX {
        return fail(4018, format!("image too large ({size} bytes; cap is {} MB)", IMAGE_MAX / (1024 * 1024)));
    }
    let lock = stage_lock(dir);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut list = pending(dir);
    list.push(path.to_string_lossy().into_owned());
    write_json(&dir.join("pending.json"), &list).or_else(|e| fail(5027, e.to_string()))?;
    Ok(list.len())
}

fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn is_image(path: &Path) -> bool {
    IMAGE_EXTS.contains(&ext_of(path).as_str())
}

fn media_type(path: &Path) -> &'static str {
    match ext_of(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

/// `~`, `file://`, quotes and backslash-escaped spaces, as a terminal drop produces them.
fn clean_token(raw: &str) -> PathBuf {
    let t = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim_start_matches("file://").replace("\\ ", " ");
    match (t.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(t),
    }
}

/// `(existing file, remainder)` for a path at the start of `raw`, or the whole of it.
fn split_path(raw: &str) -> Option<(PathBuf, String)> {
    let raw = raw.trim();
    let whole = clean_token(raw);
    if whole.is_file() {
        return Some((whole, String::new()));
    }
    let (token, rest) = match raw.chars().next()? {
        q @ ('"' | '\'') => raw[1..].split_once(q).map(|(t, r)| (t.to_string(), r))?,
        _ => {
            // Escaped spaces belong to the path; the first bare space ends it.
            let bytes = raw.as_bytes();
            let end = (0..bytes.len()).find(|&i| bytes[i] == b' ' && (i == 0 || bytes[i - 1] != b'\\')).unwrap_or(raw.len());
            (raw[..end].to_string(), &raw[end..])
        }
    };
    let path = clean_token(&token);
    path.is_file().then(|| (path, rest.trim().to_string()))
}

fn image_meta(path: &Path) -> Value {
    json!({ "name": path.file_name().and_then(|n| n.to_str()).unwrap_or("") })
}

fn attached_image(path: &Path, count: usize, extra: Value) -> Value {
    let mut out = json!({ "attached": true, "path": path.to_string_lossy(), "count": count });
    for (k, v) in image_meta(path).as_object().into_iter().flatten().chain(extra.as_object().into_iter().flatten()) {
        out[k] = v.clone();
    }
    out
}

fn decode(raw: &str, max: usize, label: &str) -> Result<Vec<u8>, Failure> {
    let cleaned: String = raw.trim().split_once(";base64,").map_or(raw.trim(), |(_, b)| b).split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .or_else(|_| fail(4017, "data is not valid base64"))?;
    if bytes.is_empty() {
        return fail(4017, format!("{label} is empty"));
    }
    if bytes.len() > max {
        return fail(4018, format!("{label} too large ({} bytes; cap is {} MB)", bytes.len(), max / (1024 * 1024)));
    }
    Ok(bytes)
}

fn sniff_ext(bytes: &[u8], filename: &str) -> String {
    let named = ext_of(Path::new(filename));
    if !named.is_empty() {
        return named;
    }
    let ext = match bytes {
        b if b.starts_with(b"\x89PNG\r\n\x1a\n") => "png",
        b if b.starts_with(b"\xff\xd8\xff") => "jpg",
        b if b.starts_with(b"GIF8") => "gif",
        b if b.starts_with(b"BM") => "bmp",
        b if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" => "webp",
        _ => "png",
    };
    ext.into()
}

fn unique(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
    let n = pending(dir).len() + 1;
    dir.join("images").join(format!("{prefix}_{}_{n}.{ext}", chrono::Local::now().format("%Y%m%d_%H%M%S%3f")))
}

fn write_staged(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).and_then(|_| std::fs::write(path, bytes)).or_else(|e| fail(5027, format!("write failed: {e}")))
}

fn ref_for(path: &Path, cwd: &str) -> (String, String) {
    let rel = path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().into_owned();
    let quoted = if rel.contains(|c: char| c.is_whitespace() || "()[]{}<>\"'`".contains(c)) {
        ["`", "\"", "'"].iter().find(|q| !rel.contains(**q)).map_or(rel.clone(), |q| format!("{q}{rel}{q}"))
    } else {
        rel.clone()
    };
    (rel, format!("@file:{quoted}"))
}

fn safe_name(name: &str, fallback: &str) -> String {
    let base = Path::new(name.trim()).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let clean: String = base.chars().map(|c| if c.is_control() { '_' } else { c }).collect();
    let clean = clean.trim().trim_matches('.').to_string();
    if clean.is_empty() { fallback.into() } else { clean }
}

fn text_of<'a>(p: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| p[*k].as_str().filter(|s| !s.trim().is_empty())).unwrap_or("").trim()
}

/// AppleScript that writes the clipboard PNG to `to`, with `\` and `"` in the path escaped.
fn clipboard_script(to: &Path) -> String {
    let out = to.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "set f to open for access POSIX file \"{out}\" with write permission\nset eof f to 0\nwrite (the clipboard as «class PNGf») to f\nclose access f"
    )
}

fn clipboard_png(to: &Path) -> bool {
    let script = clipboard_script(to);
    let attempts: [(&str, Vec<&str>); 3] = [
        ("osascript", vec!["-e", &script]),
        ("wl-paste", vec!["--type", "image/png"]),
        ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
    ];
    for (bin, args) in attempts {
        let Ok(res) = std::process::Command::new(bin).args(&args).stdin(std::process::Stdio::null()).output() else { continue };
        if bin != "osascript" && res.status.success() && !res.stdout.is_empty() {
            let _ = std::fs::write(to, &res.stdout);
        }
        if to.metadata().is_ok_and(|m| m.len() > 0) {
            return true;
        }
        let _ = std::fs::remove_file(to);
    }
    false
}

pub(super) fn handle(method: &str, home: &str, cwd: &str, p: &Value) -> Reply {
    let session = p["session_id"].as_str().filter(|s| !s.is_empty()).ok_or((-32602, "session_id is required".to_string()))?;
    let dir = stage_dir(home, session).ok_or((-32602, "invalid session_id".to_string()))?;
    match method {
        "image.attach" => {
            let raw = text_of(p, &["path"]);
            if raw.is_empty() {
                return fail(4015, "path required");
            }
            let Some((path, rest)) = split_path(raw) else { return fail(4016, format!("image not found: {raw}")) };
            if !is_image(&path) {
                return fail(4016, format!("unsupported image: {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("")));
            }
            let count = queue_image(&dir, &path)?;
            let text = if rest.is_empty() { format!("[User attached image: {}]", path.file_name().and_then(|n| n.to_str()).unwrap_or("")) } else { rest.clone() };
            Ok(attached_image(&path, count, json!({ "remainder": rest, "text": text })))
        }
        "image.attach_bytes" => {
            let b64 = text_of(p, &["content_base64", "data"]);
            if b64.is_empty() {
                return fail(4015, "content_base64 required");
            }
            let bytes = decode(b64, IMAGE_MAX, "image")?;
            let hint = match (text_of(p, &["filename"]), text_of(p, &["ext"]).trim_start_matches('.')) {
                ("", "") => String::new(),
                ("", ext) => format!("x.{ext}"),
                (name, _) => name.to_string(),
            };
            let ext = sniff_ext(&bytes, &hint);
            if !IMAGE_EXTS.contains(&ext.as_str()) {
                return fail(4016, format!("unsupported image extension: .{ext}"));
            }
            let path = unique(&dir, "upload", &ext);
            write_staged(&path, &bytes)?;
            let count = queue_image(&dir, &path)?;
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            Ok(attached_image(&path, count, json!({ "remainder": "", "text": format!("[User attached image: {name}]"), "bytes": bytes.len() })))
        }
        "image.detach" => {
            let raw = text_of(p, &["path"]);
            if raw.is_empty() {
                return fail(4015, "path required");
            }
            let lock = stage_lock(&dir);
            let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
            let before = pending(&dir);
            let after: Vec<String> = before.iter().filter(|x| x.as_str() != raw).cloned().collect();
            write_json(&dir.join("pending.json"), &after).or_else(|e| fail(5027, e.to_string()))?;
            let detached = after.len() != before.len();
            // The staged copy goes too (never a file outside this session's directory).
            if detached && Path::new(raw).starts_with(&dir) {
                let _ = std::fs::remove_file(raw);
            }
            Ok(json!({ "detached": detached, "count": after.len() }))
        }
        "file.attach" => {
            let (raw, data_url) = (text_of(p, &["path"]), text_of(p, &["data_url"]));
            if raw.is_empty() && data_url.is_empty() {
                return fail(4015, "path or data_url required");
            }
            let found = split_path(raw).map(|(path, _)| path);
            let (stored, uploaded) = match found {
                Some(path) => (path, false),
                None if data_url.is_empty() => return fail(5028, "file not found on gateway and no data_url provided"),
                None => {
                    let bytes = decode(data_url, BYTES_MAX * 2, "file").map_err(|(_, m)| (5028, m))?;
                    let target = dir.join("files").join(safe_name(text_of(p, &["name"]), "attachment"));
                    write_staged(&target, &bytes).map_err(|(_, m)| (5028, m))?;
                    (target, true)
                }
            };
            let (ref_path, ref_text) = ref_for(&stored, cwd);
            Ok(json!({
                "attached": true,
                "name": stored.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "path": stored.to_string_lossy(), "ref_path": ref_path, "ref_text": ref_text, "uploaded": uploaded,
            }))
        }
        "pdf.attach" => {
            let (raw, b64) = (text_of(p, &["path"]), text_of(p, &["content_base64", "data"]));
            if raw.is_empty() && b64.is_empty() {
                return fail(4015, "path or content_base64 required");
            }
            let (pdf, name) = if b64.is_empty() {
                let path = split_path(raw).map(|(path, _)| path).filter(|x| x.is_file()).ok_or((4016, format!("PDF not found: {raw}")))?;
                if ext_of(&path) != "pdf" {
                    return fail(4016, format!("not a PDF: {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("")));
                }
                if path.metadata().is_ok_and(|m| m.len() > PDF_MAX) {
                    return fail(4018, "PDF too large; cap is 50 MB");
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("document.pdf").to_string();
                (path, name)
            } else {
                let bytes = decode(b64, PDF_MAX as usize, "PDF")?;
                if !bytes.starts_with(b"%PDF-") {
                    return fail(4017, "payload is not a PDF (missing %PDF- magic bytes)");
                }
                let name = safe_name(text_of(p, &["filename"]), "uploaded.pdf");
                let path = dir.join("files").join(&name);
                write_staged(&path, &bytes)?;
                (path, name)
            };
            let text = jcode_pdf::extract_text(&pdf).or_else(|e| fail(5028, e.to_string()))?;
            let pages = text.split('\u{c}').filter(|s| !s.trim().is_empty()).count().max(1);
            let target = dir.join("files").join(format!("{name}.txt"));
            write_staged(&target, text.as_bytes()).map_err(|(_, m)| (5028, m))?;
            let (_, ref_text) = ref_for(&target, cwd);
            Ok(json!({
                "attached": true, "filename": name, "pages_attached": pages, "pages": [], "count": pending(&dir).len(),
                "path": target.to_string_lossy(), "ref_text": ref_text,
                "text": format!("[User attached PDF: {name} ({pages} page(s))]\n{ref_text}"),
            }))
        }
        "clipboard.paste" => {
            let path = unique(&dir, "clip", "png");
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&dir));
            if !clipboard_png(&path) {
                return Ok(json!({ "attached": false, "message": "No image found in clipboard" }));
            }
            let count = queue_image(&dir, &path)?;
            Ok(attached_image(&path, count, json!({})))
        }
        "input.detect_drop" => {
            let Some((path, rest)) = split_path(text_of(p, &["text"])) else { return Ok(json!({ "matched": false })) };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if is_image(&path) {
                let count = queue_image(&dir, &path)?;
                let text = if rest.is_empty() { format!("[User attached image: {name}]") } else { rest };
                return Ok(json!({ "matched": true, "is_image": true, "path": path.to_string_lossy(), "count": count, "text": text, "name": name }));
            }
            let text = format!("[User attached file: {}]", path.display()) + &if rest.is_empty() { String::new() } else { format!("\n{rest}") };
            Ok(json!({ "matched": true, "is_image": false, "path": path.to_string_lossy(), "name": name, "text": text }))
        }
        _ => react(&dir, p),
    }
}

/// Tapback semantics like Hermes: one reaction per author, the same emoji retracts, null clears.
fn react(dir: &Path, p: &Value) -> Reply {
    let role = p["newest_role"].as_str().unwrap_or("");
    let row_id = p["row_id"].as_i64();
    if row_id.is_none() && !matches!(role, "user" | "assistant") {
        return fail(4023, "row_id or newest_role required");
    }
    let emoji = match &p["emoji"] {
        Value::Null => None,
        e => match e.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(e) => Some(e.to_string()),
            None => return fail(4024, "emoji must be a non-empty string or null"),
        },
    };
    let author = p["author"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("user");
    if !matches!(author, "user" | "agent") {
        return fail(4025, "author must be 'user' or 'agent'");
    }
    let file = dir.join("reactions.json");
    let mut all: serde_json::Map<String, Value> =
        std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let key = row_id.map_or_else(|| format!("newest:{role}"), |r| r.to_string());
    let existing: Vec<Value> = all.get(&key).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let previous = existing.iter().find(|r| r["author"] == author).cloned();
    let mut reactions: Vec<Value> = existing.into_iter().filter(|r| r["author"] != author).collect();
    if let Some(emoji) = emoji.filter(|e| previous.as_ref().is_none_or(|p| p["emoji"] != *e)) {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        reactions.push(json!({ "emoji": emoji, "author": author, "at": at }));
    }
    all.insert(key, json!(reactions));
    write_json(&file, &all).or_else(|e| fail(5007, e.to_string()))?;
    Ok(json!({ "row_id": row_id.unwrap_or(0), "reactions": reactions }))
}

/// Staged image paths, the same images as `(media_type, base64)` for `send_message`, and the paths
/// that could not be read (they are dropped from the turn; the caller reports them). Nothing is
/// cleared here (see `clear_staged`). Blocking file reads and encoding: call off the async workers.
pub(super) fn staged_images(home: &str, session: &str) -> (Vec<String>, Vec<(String, String)>, Vec<String>) {
    let paths = stage_dir(home, session).map(|dir| pending(&dir)).unwrap_or_default();
    let (mut images, mut unreadable) = (Vec::new(), Vec::new());
    for p in &paths {
        match std::fs::read(p) {
            Ok(bytes) => images.push((media_type(Path::new(p)).to_string(), base64::engine::general_purpose::STANDARD.encode(bytes))),
            Err(_) => unreadable.push(p.clone()),
        }
    }
    (paths, images, unreadable)
}

/// Drop only the `sent` paths from the queue: an image attached while the submit was in flight
/// stays for the next turn.
pub(super) fn clear_staged(home: &str, session: &str, sent: &[String]) {
    let Some(dir) = stage_dir(home, session) else { return };
    let lock = stage_lock(&dir);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let left: Vec<String> = pending(&dir).into_iter().filter(|p| !sent.contains(p)).collect();
    if left.is_empty() {
        let _ = std::fs::remove_file(dir.join("pending.json"));
    } else {
        let _ = write_json(&dir.join("pending.json"), &left);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_harness_api_server::translate::{BridgeState, Outbound};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    fn setup(name: &str) -> (String, String) {
        let home = std::env::temp_dir().join(format!("attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        (home.to_string_lossy().into(), home.to_string_lossy().into())
    }

    fn call(method: &str, home: &str, p: Value) -> Reply {
        let mut p = p;
        p["session_id"] = json!("s1");
        handle(method, home, home, &p)
    }

    #[test]
    fn a_staged_image_reaches_the_provider_request_once() {
        let (home, _) = setup("image");
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        let r = call("image.attach_bytes", &home, json!({ "content_base64": format!("data:image/png;base64,{b64}") })).unwrap();
        assert_eq!((r["attached"].clone(), r["count"].clone(), r["bytes"].clone()), (json!(true), json!(1), json!(PNG.len())));
        assert!(r["text"].as_str().unwrap().starts_with("[User attached image: upload_"));
        assert!(Path::new(r["path"].as_str().unwrap()).is_file());

        let (sent, images, _) = staged_images(&home, "s1");
        assert_eq!(images, vec![("image/png".to_string(), b64.clone())]);
        // The harness bridge carries them into the legacy `message` jcode turns into an image content part.
        let mut request = super::super::send_message_request("s1", "look", None, images);
        request["id"] = json!(2);
        let mut bridge = BridgeState::default();
        bridge.session_id = Some("s1".into());
        let out = bridge.api_request_to_legacy(&request);
        let Some(Outbound::Legacy(message)) = out.first() else { panic!("no legacy message") };
        assert_eq!(message["images"], json!([["image/png", b64]]));

        // An image attached while the submit was in flight survives clearing what was sent.
        let late = call("image.attach_bytes", &home, json!({ "content_base64": format!("data:image/png;base64,{b64}") })).unwrap();
        clear_staged(&home, "s1", &sent);
        let (left, _, _) = staged_images(&home, "s1");
        assert_eq!(left, vec![late["path"].as_str().unwrap().to_string()]);
        clear_staged(&home, "s1", &left);
        assert!(staged_images(&home, "s1").0.is_empty(), "consumed by the submit");
    }

    #[test]
    fn oversized_images_are_refused_and_concurrent_attaches_are_all_kept() {
        let (home, _) = setup("limits");
        let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; IMAGE_MAX + 1]);
        assert_eq!(call("image.attach_bytes", &home, json!({ "content_base64": big, "ext": "png" })).unwrap_err().0, 4018);
        let file = Path::new(&home).join("big.png");
        std::fs::write(&file, vec![0u8; IMAGE_MAX + 1]).unwrap();
        assert_eq!(call("image.attach", &home, json!({ "path": file.to_string_lossy() })).unwrap_err().0, 4018);

        let dir = stage_dir(&home, "s1").unwrap();
        std::thread::scope(|s| {
            for i in 0..16 {
                let dir = &dir;
                s.spawn(move || queue_image(dir, Path::new(&format!("/x/{i}.png"))).unwrap());
            }
        });
        assert_eq!(pending(&dir).len(), 16);
    }

    #[test]
    fn image_attach_detach_and_drop_use_hermes_shapes() {
        let (home, _) = setup("path");
        let img = Path::new(&home).join("my pic.png");
        std::fs::write(&img, PNG).unwrap();
        let r = call("image.attach", &home, json!({ "path": format!("'{}' what is this", img.display()) })).unwrap();
        assert_eq!((r["remainder"].as_str(), r["text"].as_str(), r["count"].as_i64()), (Some("what is this"), Some("what is this"), Some(1)));
        assert_eq!(call("image.attach", &home, json!({})).unwrap_err().0, 4015);
        assert_eq!(call("image.attach", &home, json!({ "path": "/nope.png" })).unwrap_err().0, 4016);

        let d = call("input.detect_drop", &home, json!({ "text": img.to_string_lossy().replace(' ', "\\ ") })).unwrap();
        assert_eq!((d["matched"].clone(), d["is_image"].clone(), d["count"].clone()), (json!(true), json!(true), json!(2)));
        assert_eq!(call("input.detect_drop", &home, json!({ "text": "hello there" })).unwrap(), json!({ "matched": false }));

        let gone = call("image.detach", &home, json!({ "path": img.to_string_lossy() })).unwrap();
        assert_eq!(gone, json!({ "detached": true, "count": 0 }));
    }

    #[test]
    fn file_and_pdf_attach_return_refs_and_react_toggles() {
        let (home, _) = setup("file");
        let data = format!("data:text/plain;base64,{}", base64::engine::general_purpose::STANDARD.encode("hi"));
        let r = call("file.attach", &home, json!({ "name": "../notes.txt", "data_url": data })).unwrap();
        assert_eq!((r["attached"].clone(), r["uploaded"].clone(), r["name"].clone()), (json!(true), json!(true), json!("notes.txt")));
        assert!(r["ref_text"].as_str().unwrap().starts_with("@file:"));
        assert_eq!(std::fs::read_to_string(r["path"].as_str().unwrap()).unwrap(), "hi");
        assert_eq!(call("file.attach", &home, json!({})).unwrap_err().0, 4015);
        assert_eq!(call("pdf.attach", &home, json!({ "content_base64": base64::engine::general_purpose::STANDARD.encode("nope") })).unwrap_err().0, 4017);

        let react = |emoji: Value| call("message.react", &home, json!({ "row_id": 7, "emoji": emoji })).unwrap();
        assert_eq!(react(json!("👍"))["reactions"].as_array().unwrap().len(), 1);
        assert!(react(json!("👍"))["reactions"].as_array().unwrap().is_empty(), "same emoji retracts");
        assert_eq!(react(json!("🎉"))["row_id"], 7);
        assert!(react(Value::Null)["reactions"].as_array().unwrap().is_empty());
        assert_eq!(call("message.react", &home, json!({})).unwrap_err().0, 4023);
    }

    #[test]
    fn session_ids_that_are_not_plain_names_never_reach_the_attachments_root() {
        let (home, _) = setup("ids");
        for bad in ["", "///", "a/b", "..", "a b", "../x"] {
            assert!(stage_dir(&home, bad).is_none(), "{bad:?}");
        }
        assert_ne!(stage_dir(&home, "ab"), stage_dir(&home, "a-b"));
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        for bad in ["///", "a/b"] {
            let p = json!({ "content_base64": b64, "session_id": bad });
            assert_eq!(handle("image.attach_bytes", &home, &home, &p).unwrap_err().0, -32602);
        }
        assert!(!Path::new(&home).join("attachments").exists(), "nothing was written anywhere");
        assert!(staged_images(&home, "///").0.is_empty());
    }

    #[test]
    fn detach_deletes_the_staged_file_and_unreadable_staged_images_are_reported() {
        let (home, _) = setup("detach");
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
        let a = call("image.attach_bytes", &home, json!({ "content_base64": b64 })).unwrap();
        let b = call("image.attach_bytes", &home, json!({ "content_base64": b64 })).unwrap();
        let (a, b) = (a["path"].as_str().unwrap().to_string(), b["path"].as_str().unwrap().to_string());
        std::fs::remove_file(&b).unwrap(); // vanished before the turn was sent
        let (paths, images, unreadable) = staged_images(&home, "s1");
        assert_eq!((paths.len(), images.len(), unreadable), (2, 1, vec![b]));
        assert_eq!(call("image.detach", &home, json!({ "path": a })).unwrap()["detached"], true);
        assert!(!Path::new(&a).exists(), "detach removes the staged file");
    }

    #[test]
    fn the_clipboard_script_escapes_quotes_and_backslashes_in_the_path() {
        let script = clipboard_script(Path::new("/tmp/a\"b\\c/x.png"));
        assert!(script.contains(r#"POSIX file "/tmp/a\"b\\c/x.png" with"#), "{script}");
    }

    #[test]
    fn the_socket_cap_covers_the_largest_attach() {
        let b64_len = |n: usize| n.div_ceil(3) * 4;
        assert!(MAX_WIRE_BYTES > b64_len(IMAGE_MAX) && MAX_WIRE_BYTES > b64_len(BYTES_MAX * 2) && MAX_WIRE_BYTES > b64_len(PDF_MAX as usize));
        assert!(MAX_WIRE_BYTES > 6_500_000 * 4 / 3, "a 6.5 MB image fits");
    }
}
