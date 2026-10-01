use super::{Tool, ToolContext, ToolOutput};
use super::webfetch_net::{fetch_resilient, wikipedia_raw_url};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// Text pages are cut to this many bytes before conversion.
const MAX_SIZE: usize = 5 * 1024 * 1024; // 5MB
/// Default window handed to the model: head + tail of long pages. The full text
/// is spilled to a file so `find`/`offset`/`grep`/`read` can reach the rest.
const HEAD_CHARS: usize = 8_000;
const TAIL_CHARS: usize = 4_000;
const WINDOW_CHARS: usize = HEAD_CHARS + TAIL_CHARS;
const FIND_MATCHES: usize = 5;
const FIND_WINDOW: usize = 600;
/// Links whose target exceeds this length are rendered as their anchor text
/// only. Long URLs are typically encoded payloads (pre-filled editors, tracking
/// parameters, data URIs) whose cost far exceeds their navigational value.
const MAX_URL_CHARS: usize = 300;
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 120;

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: crate::provider::shared_http_client(),
        }
    }
}

#[derive(Deserialize)]
struct WebFetchInput {
    url: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    find: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "intent": super::intent_schema_property(),
                "url": {
                    "type": "string",
                    "description": "URL."
                },
                "format": {
                    "type": "string",
                    "enum": ["text", "markdown", "html"],
                    "description": "Output format."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds."
                },
                "find": {
                    "type": "string",
                    "description": "Regex; show matches."
                },
                "offset": {
                    "type": "integer",
                    "description": "Char offset."
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let params: WebFetchInput = serde_json::from_value(input)?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(anyhow::anyhow!("URL must start with http:// or https://"));
        }

        let timeout = Duration::from_secs(params.timeout.unwrap_or(DEFAULT_TIMEOUT).min(MAX_TIMEOUT));
        let format = params.format.as_deref().unwrap_or("markdown");

        // Wikipedia articles: the plain wikitext is smaller and cleaner than the HTML.
        let wiki = if format == "html" { None } else { wikipedia_raw_url(&params.url) };
        let mut fetched = None;
        if let Some(raw) = wiki {
            fetched = fetch_resilient(&self.client, &raw, timeout).await.ok();
        }
        let (fetched, archived) = match fetched {
            Some(f) => f,
            None => fetch_resilient(&self.client, &params.url, timeout)
                .await
                .map_err(|e| anyhow::anyhow!("{} fetching {}", e.msg, params.url))?,
        };

        let mut notes = Vec::new();
        if archived {
            notes.push("original unavailable; archived copy (Wayback) used".to_string());
        }
        let ct = fetched.content_type.to_ascii_lowercase();
        let bytes = fetched.bytes;
        let hash = content_hash(&bytes);

        let text = if ct.contains("pdf") || bytes.starts_with(b"%PDF") {
            match pdf_text(&bytes, &hash).await {
                Ok(t) => t,
                Err(msg) => return Ok(ToolOutput::new(format!("Fetched {} (PDF, {} bytes): {msg}", params.url, bytes.len()))),
            }
        } else if let Some(ext) = binary_ext(&ct, &bytes) {
            let msg = match save_scratch(&bytes, &format!("webfetch-{hash}.{ext}")) {
                Some(p) => format!("saved to {}; use read for documents or python to parse", p.display()),
                None => "could not save to scratch dir".to_string(),
            };
            return Ok(ToolOutput::new(format!(
                "Fetched {} ({ct}, {} bytes): {msg}",
                params.url,
                bytes.len()
            )));
        } else {
            let mut bytes = bytes;
            bytes.truncate(MAX_SIZE);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let is_html = ct.contains("html") || ct.is_empty() && body.trim_start().starts_with('<');
            match format {
                "html" => body,
                "text" if is_html => html_to_text(&body),
                "text" => body,
                _ if is_html => html_to_markdown(&body),
                _ => body,
            }
        };

        let total = text.chars().count();
        let spill = (total > WINDOW_CHARS)
            .then(|| save_scratch(text.as_bytes(), &format!("webfetch-{}.txt", content_hash(text.as_bytes()))))
            .flatten();
        let path_note = spill.map(|p| format!("; full text at {}", p.display())).unwrap_or_default();

        let body = if let Some(pat) = params.find.as_deref().filter(|p| !p.is_empty()) {
            let (hits, n) = find_windows(&text, pat);
            notes.push(format!("{n} match(es) for {pat:?} in {total} chars{path_note}"));
            hits
        } else if let Some(off) = params.offset {
            let (w, next) = offset_window(&text, off);
            let more = next.map(|n| format!("; next offset={n}")).unwrap_or_default();
            notes.push(format!("chars {off}..{} of {total}{more}{path_note}", off + w.chars().count()));
            w
        } else {
            let (w, cut) = window(&text);
            if cut {
                notes.push(format!(
                    "showing head {HEAD_CHARS} + tail {TAIL_CHARS} of {total} chars{path_note}; use find or offset"
                ));
            }
            w
        };

        let note = if notes.is_empty() { String::new() } else { format!("\n({})", notes.join("; ")) };
        Ok(ToolOutput::new(format!("Fetched {} ({total} chars){note}\n\n{body}", params.url)))
    }
}

fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Same scratch dir the bash tool uses (`JCODE_SCRATCH_DIR`, else `<jcode>/scratch`).
fn scratch_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("JCODE_SCRATCH_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::storage::jcode_dir().ok().map(|d| d.join("scratch")))?;
    crate::storage::ensure_dir(&dir).ok()?;
    Some(dir)
}

/// Write a private (0600) file under the scratch dir; same name is reused.
fn save_scratch(bytes: &[u8], name: &str) -> Option<PathBuf> {
    let path = scratch_dir()?.join(name);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
        return Some(path);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&path).ok()?, bytes).ok()?;
    Some(path)
}

/// File extension when the response is a non-text payload we must not decode.
fn binary_ext(ct: &str, bytes: &[u8]) -> Option<&'static str> {
    let by_type = [
        ("spreadsheetml", "xlsx"),
        ("ms-excel", "xls"),
        ("wordprocessingml", "docx"),
        ("msword", "doc"),
        ("presentationml", "pptx"),
        ("ms-powerpoint", "ppt"),
        ("zip", "zip"),
        ("audio/", "audio"),
        ("video/", "video"),
        ("image/", "img"),
    ];
    if let Some((_, ext)) = by_type.iter().find(|(k, _)| ct.contains(k)) {
        return Some(ext);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Some("zip");
    }
    (ct.contains("octet-stream") && bytes.iter().take(1024).any(|&b| b == 0)).then_some("bin")
}

#[cfg(feature = "pdf")]
async fn pdf_text(bytes: &[u8], hash: &str) -> std::result::Result<String, String> {
    let path = save_scratch(bytes, &format!("webfetch-{hash}.pdf")).ok_or("could not save to scratch dir")?;
    let p = path.clone();
    let text = tokio::task::spawn_blocking(move || jcode_pdf::extract_text(&p))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("no text extracted ({e}); saved to {}", path.display()))?;
    let text = text.replace('\x0c', "\n\n");
    if text.trim().is_empty() {
        return Err(format!("no text layer (scanned?); saved to {}", path.display()));
    }
    Ok(text)
}

#[cfg(not(feature = "pdf"))]
async fn pdf_text(bytes: &[u8], hash: &str) -> std::result::Result<String, String> {
    let path = save_scratch(bytes, &format!("webfetch-{hash}.pdf"));
    Err(format!("PDF text extraction not built in; saved to {path:?}"))
}

fn byte_at(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map_or(text.len(), |(b, _)| b)
}

/// Short text whole; long text as head + tail. Bool is true when cut.
pub(super) fn window(text: &str) -> (String, bool) {
    let total = text.chars().count();
    if total <= WINDOW_CHARS {
        return (text.to_string(), false);
    }
    let head = &text[..byte_at(text, HEAD_CHARS)];
    let tail = &text[byte_at(text, total - TAIL_CHARS)..];
    (format!("{head}\n\n[... {} chars omitted ...]\n\n{tail}", total - WINDOW_CHARS), true)
}

/// The next window at a char offset, plus the offset after it when more remains.
fn offset_window(text: &str, offset: usize) -> (String, Option<usize>) {
    let total = text.chars().count();
    let start = offset.min(total);
    let end = (start + WINDOW_CHARS).min(total);
    let s = byte_at(text, start);
    (text[s..byte_at(text, end)].to_string(), (end < total).then_some(end))
}

/// Up to 5 case-insensitive matches, each with ~600 chars of context and its char offset.
fn find_windows(text: &str, pat: &str) -> (String, usize) {
    let re = regex::Regex::new(&format!("(?i){pat}"))
        .or_else(|_| regex::Regex::new(&format!("(?i){}", regex::escape(pat))))
        .expect("escaped pattern compiles");
    let mut out = String::new();
    let (mut n, mut last_end) = (0, 0usize);
    for m in re.find_iter(text) {
        if m.start() < last_end {
            continue;
        }
        n += 1;
        if n > FIND_MATCHES {
            break;
        }
        let ch = text[..m.start()].chars().count();
        let from = ch.saturating_sub(FIND_WINDOW / 2);
        let (s, e) = (byte_at(text, from), byte_at(text, from + FIND_WINDOW));
        last_end = e;
        out.push_str(&format!("--- match at char {ch} ---\n{}\n\n", &text[s..e]));
    }
    if n == 0 {
        out.push_str("(no matches)");
    }
    (out, n.min(FIND_MATCHES))
}

mod html_regex {
    use regex::Regex;
    use std::sync::OnceLock;

    fn compile_regex(pattern: &str, label: &str) -> Option<Regex> {
        match Regex::new(pattern) {
            Ok(regex) => Some(regex),
            Err(err) => {
                crate::logging::warn(&format!(
                    "webfetch: failed to compile static regex {label}: {}",
                    err
                ));
                None
            }
        }
    }

    macro_rules! static_regex {
        ($name:ident, $pat:expr_2021) => {
            pub fn $name() -> Option<&'static Regex> {
                static RE: OnceLock<Option<Regex>> = OnceLock::new();
                RE.get_or_init(|| compile_regex($pat, stringify!($name)))
                    .as_ref()
            }
        };
    }

    static_regex!(script, r"(?is)<script[^>]*>.*?</script>");
    static_regex!(style, r"(?is)<style[^>]*>.*?</style>");
    // Match attribute values (which may themselves contain `>`) before falling
    // back to bare `>`-terminated content, so tags carrying JSON payloads such as
    // Parsoid's `data-mw` do not leak their contents into the output.
    static_regex!(
        tag,
        r#"(?s)</?[A-Za-z!/][^\s/>]*(?:\s+[^\s=/>]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*))?)*\s*/?>"#
    );
    static_regex!(whitespace, r"\n\s*\n\s*\n");
    // Runs of empty markdown list items left behind after tag stripping.
    static_regex!(empty_bullets, r"(?m)^[ \t]*-[ \t]*$\n?");

    /// HTML elements whose content is non-prose by specification: navigation,
    /// complementary/tangential content, interactive controls, and embedded
    /// non-text resources. This is deliberately limited to elements whose *spec
    /// definition* excludes primary content, so it generalizes across sites
    /// rather than encoding any single site's markup.
    ///
    /// Notably excludes `<header>`, which commonly wraps the article `<h1>`,
    /// byline, and publication date, and `<footer>`, which can carry
    /// article-level attribution when nested inside `<article>`.
    const CHROME_TAGS: [&str; 10] = [
        "nav", "aside", "form", "noscript", "svg", "iframe", "template", "select", "dialog",
        "canvas",
    ];

    static CHROME: OnceLock<Vec<Regex>> = OnceLock::new();

    pub fn chrome() -> &'static [Regex] {
        CHROME.get_or_init(|| {
            CHROME_TAGS
                .iter()
                .filter_map(|tag| {
                    compile_regex(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>"), "chrome")
                })
                .collect()
        })
    }
    static_regex!(link, r#"(?i)<a[^>]*href=["']([^"']+)["'][^>]*>([^<]*)</a>"#);
    static_regex!(strong, r"(?i)<(?:strong|b)>([^<]*)</(?:strong|b)>");
    static_regex!(em, r"(?i)<(?:em|i)>([^<]*)</(?:em|i)>");
    static_regex!(code, r"(?i)<code>([^<]*)</code>");
    static_regex!(pre_code, r"(?is)<pre[^>]*><code[^>]*>(.+?)</code></pre>");
    static_regex!(li, r"(?i)<li[^>]*>");
    // HTML comments frequently contain build metadata, conditional markup, and
    // commented-out blocks, none of which are rendered content.
    static_regex!(comment, r"(?s)<!--.*?-->");

    static H_OPEN: OnceLock<Option<[Regex; 6]>> = OnceLock::new();
    static H_CLOSE: OnceLock<Option<[Regex; 6]>> = OnceLock::new();

    pub fn h_open() -> Option<&'static [Regex; 6]> {
        H_OPEN
            .get_or_init(|| {
                let mut compiled = Vec::with_capacity(6);
                for i in 0..6 {
                    let pattern = format!(r"(?i)<h{}[^>]*>", i + 1);
                    compiled.push(compile_regex(&pattern, "heading open")?);
                }
                compiled.try_into().ok()
            })
            .as_ref()
    }

    pub fn h_close() -> Option<&'static [Regex; 6]> {
        H_CLOSE
            .get_or_init(|| {
                let mut compiled = Vec::with_capacity(6);
                for i in 0..6 {
                    let pattern = format!(r"(?i)</h{}>", i + 1);
                    compiled.push(compile_regex(&pattern, "heading close")?);
                }
                compiled.try_into().ok()
            })
            .as_ref()
    }
}

fn html_to_text(html: &str) -> String {
    let mut text = html.to_string();

    let (Some(script), Some(style), Some(tag), Some(whitespace)) = (
        html_regex::script(),
        html_regex::style(),
        html_regex::tag(),
        html_regex::whitespace(),
    ) else {
        return html.trim().to_string();
    };

    text = script.replace_all(&text, "").to_string();
    text = style.replace_all(&text, "").to_string();
    if let Some(comment) = html_regex::comment() {
        text = comment.replace_all(&text, "").to_string();
    }
    for re in html_regex::chrome() {
        text = re.replace_all(&text, "").to_string();
    }

    text = text.replace("<br>", "\n");
    text = text.replace("<br/>", "\n");
    text = text.replace("<br />", "\n");
    text = text.replace("</p>", "\n\n");
    text = text.replace("</div>", "\n");
    text = text.replace("</li>", "\n");
    text = text.replace("</tr>", "\n");

    text = tag.replace_all(&text, "").to_string();

    text = text.replace("&nbsp;", " ");
    text = text.replace("&lt;", "<");
    text = text.replace("&gt;", ">");
    text = text.replace("&amp;", "&");
    text = text.replace("&quot;", "\"");
    text = text.replace("&#39;", "'");

    text = whitespace.replace_all(&text, "\n\n").to_string();

    text.trim().to_string()
}

/// Render one anchor as markdown, dropping targets that cost more context than
/// they convey.
///
/// Three general cases, none specific to any site:
/// - Empty anchor text means the link is a bare icon or control. Emitting
///   `[](url)` conveys nothing, so the whole link is dropped.
/// - Overlong targets are encoded payloads rather than addresses; the anchor
///   text is kept and the target dropped.
/// - Pure in-page fragments (`#foo`) are navigation aids with no destination
///   content, so the text is kept and the target dropped.
fn render_link(href: &str, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.chars().count() > MAX_URL_CHARS {
        return text.to_string();
    }
    format!("[{text}]({href})")
}

fn html_to_markdown(html: &str) -> String {
    let mut md = html.to_string();

    let (
        Some(script),
        Some(style),
        Some(link),
        Some(strong),
        Some(em),
        Some(code),
        Some(pre_code),
        Some(li),
        Some(tag),
        Some(whitespace),
    ) = (
        html_regex::script(),
        html_regex::style(),
        html_regex::link(),
        html_regex::strong(),
        html_regex::em(),
        html_regex::code(),
        html_regex::pre_code(),
        html_regex::li(),
        html_regex::tag(),
        html_regex::whitespace(),
    )
    else {
        return html.trim().to_string();
    };

    md = script.replace_all(&md, "").to_string();
    md = style.replace_all(&md, "").to_string();
    if let Some(comment) = html_regex::comment() {
        md = comment.replace_all(&md, "").to_string();
    }
    for re in html_regex::chrome() {
        md = re.replace_all(&md, "").to_string();
    }

    if let (Some(h_open), Some(h_close)) = (html_regex::h_open(), html_regex::h_close()) {
        for i in 0..6 {
            let prefix = "#".repeat(i + 1);
            md = h_open[i]
                .replace_all(&md, &format!("\n{} ", prefix))
                .to_string();
            md = h_close[i].replace_all(&md, "\n").to_string();
        }
    }

    md = link
        .replace_all(&md, |caps: &regex::Captures<'_>| {
            render_link(
                caps.get(1).map_or("", |m| m.as_str()),
                caps.get(2).map_or("", |m| m.as_str()),
            )
        })
        .to_string();
    md = strong.replace_all(&md, "**$1**").to_string();
    md = em.replace_all(&md, "*$1*").to_string();
    md = code.replace_all(&md, "`$1`").to_string();
    md = pre_code.replace_all(&md, "\n```\n$1\n```\n").to_string();
    md = li.replace_all(&md, "\n- ").to_string();

    md = md.replace("<br>", "\n");
    md = md.replace("<br/>", "\n");
    md = md.replace("<br />", "\n");
    md = md.replace("</p>", "\n\n");

    md = tag.replace_all(&md, "").to_string();

    md = md.replace("&nbsp;", " ");
    md = md.replace("&lt;", "<");
    md = md.replace("&gt;", ">");
    md = md.replace("&amp;", "&");
    md = md.replace("&quot;", "\"");
    md = md.replace("&#39;", "'");

    if let Some(empty_bullets) = html_regex::empty_bullets() {
        md = empty_bullets.replace_all(&md, "").to_string();
    }
    md = whitespace.replace_all(&md, "\n\n").to_string();

    md.trim().to_string()
}

#[cfg(test)]
#[path = "webfetch_corpus_tests.rs"]
mod corpus_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_non_prose_elements() {
        let html = "<nav><a href='/x'>Menu</a></nav><p>Body text</p>\
                    <aside>Related</aside><form><select><option>Pick</option></select></form>";
        let md = html_to_markdown(html);
        assert!(md.contains("Body text"));
        assert!(!md.contains("Menu"), "nav should be dropped: {md}");
        assert!(!md.contains("Related"), "aside should be dropped: {md}");
        assert!(
            !md.contains("Pick"),
            "form controls should be dropped: {md}"
        );
    }

    #[test]
    fn keeps_article_header_and_footer_content() {
        // <header> usually holds the title/byline and <footer> can hold
        // article attribution, so neither is treated as chrome.
        let html = "<article><header><h1>Real Title</h1><p>By Author</p></header>\
                    <p>Body</p><footer>Published 2026</footer></article>";
        let md = html_to_markdown(html);
        for needle in ["Real Title", "By Author", "Body", "Published 2026"] {
            assert!(md.contains(needle), "{needle} missing from {md}");
        }
    }

    #[test]
    fn drops_empty_links_and_overlong_targets() {
        assert_eq!(render_link("https://example.com", ""), "");
        assert_eq!(render_link("#section", "Jump"), "Jump");
        let long = format!("https://example.com/?code={}", "a".repeat(MAX_URL_CHARS));
        assert_eq!(render_link(&long, "Run"), "Run");
        assert_eq!(
            render_link("https://example.com", "Home"),
            "[Home](https://example.com)"
        );
    }

    #[test]
    fn strips_html_comments() {
        let md = html_to_markdown("<p>Keep</p><!-- build:12345 drop me -->");
        assert!(md.contains("Keep"));
        assert!(!md.contains("drop me"), "comment retained: {md}");
    }

    #[test]
    fn does_not_leak_attributes_containing_angle_brackets() {
        // Parsoid-style tags embed JSON in attributes; a naive `<[^>]+>` regex
        // stops at the first `>` inside the value and dumps the rest as text.
        let html = r#"<span data-mw='{"wt":"[[a]] > [[b]]"}'>Visible</span>"#;
        let text = html_to_text(html);
        assert_eq!(text, "Visible");
    }

    // ---- network behaviour, against a local one-shot-per-connection server ----

    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    fn http(status: &str, ct: &str, body: &[u8], extra: &str) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    /// Serves `responses` in order, one per connection; returns base URL and raw requests seen.
    fn serve(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for resp in responses {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = s.write_all(&resp);
            }
        });
        (url, seen)
    }

    fn tool() -> WebFetchTool {
        WebFetchTool { client: reqwest::Client::builder().no_proxy().build().unwrap() }
    }

    async fn fetch(url: &str, extra: Value) -> String {
        let mut input = json!({"url": url});
        input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "c".into(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: super::super::ToolExecutionMode::AgentTurn,
        };
        tool().execute(input, ctx).await.unwrap().output
    }

    fn scratch() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let guard = crate::storage::lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        crate::env::set_var("JCODE_SCRATCH_DIR", dir.path());
        (guard, dir)
    }

    fn long_page() -> String {
        let mut p = String::from("<html><body>");
        for i in 0..2000 {
            p.push_str(&format!("<p>paragraph {i:04} filler text here</p>"));
        }
        p.push_str("<p>the NEEDLE sits here</p></body></html>");
        p
    }

    #[tokio::test]
    async fn long_page_is_windowed_spilled_and_searchable() {
        let (_g, dir) = scratch();
        let page = long_page();
        let (url, seen) = serve(vec![http("200 OK", "text/html", page.as_bytes(), ""); 3]);
        let out = fetch(&url, json!({})).await;
        assert!(out.len() < 13_500, "{}", out.len());
        assert!(out.contains("paragraph 0000") && out.contains("NEEDLE") && out.contains("chars omitted"));
        assert!(!out.contains("paragraph 1000"));
        let path = out.split("full text at ").nth(1).unwrap().split(';').next().unwrap();
        let spilled = std::fs::read_to_string(path).unwrap();
        assert!(spilled.contains("paragraph 1000"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(path.starts_with(dir.path().to_str().unwrap()));

        let found = fetch(&url, json!({"find": "paragraph 1000"})).await;
        assert!(found.contains("--- match at char") && found.contains("paragraph 1000") && !found.contains("paragraph 0000"));

        let paged = fetch(&url, json!({"offset": 12_000})).await;
        assert!(paged.contains("chars 12000..24000") && paged.contains("next offset=24000"));
        let ua = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(ua.contains("user-agent: mozilla/5.0 (macintosh") && !ua.contains("jcode"), "{ua}");
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let (_g, _d) = scratch();
        let (url, seen) = serve(vec![
            http("429 Too Many Requests", "text/plain", b"slow down", "Retry-After: 0\r\n"),
            http("200 OK", "text/plain", b"hello world", ""),
        ]);
        let out = fetch(&url, json!({})).await;
        assert!(out.contains("hello world"), "{out}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn xlsx_is_saved_not_printed() {
        let (_g, dir) = scratch();
        let body = b"PK\x03\x04\x00binary\xff\xfe";
        let ct = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        let (url, _) = serve(vec![http("200 OK", ct, body, "")]);
        let out = fetch(&url, json!({})).await;
        assert!(out.contains("saved to") && out.contains(".xlsx") && !out.contains("binary"), "{out}");
        let saved = std::fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read(saved).unwrap(), body);
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn pdf_text_is_extracted() {
        let (_g, _d) = scratch();
        let stream = "BT /F1 18 Tf 20 100 Td (Hello PDF Marker) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for o in offs {
            pdf.push_str(&format!("{o:010} 00000 n \n"));
        }
        pdf.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
        let (url, _) = serve(vec![http("200 OK", "application/pdf", pdf.as_bytes(), "")]);
        let out = fetch(&url, json!({})).await;
        assert!(out.contains("Hello PDF Marker"), "{out}");
    }

    #[test]
    fn windows_handle_multibyte_and_short_text() {
        assert_eq!(window("hi"), ("hi".to_string(), false));
        let long = "é".repeat(WINDOW_CHARS + 50);
        let (w, cut) = window(&long);
        assert!(cut && w.contains("50 chars omitted"));
        let (hits, n) = find_windows(&format!("{long} Zed"), "zed");
        assert_eq!(n, 1);
        assert!(hits.contains(&format!("char {}", long.chars().count() + 1)));
    }
}
