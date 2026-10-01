//! Resilient HTTP for `webfetch`: browser UA, retry with jittered backoff,
//! Wayback fallback, Wikipedia raw-text preference. Pure helpers are unit-tested.
use futures::StreamExt;
use std::time::Duration;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// Hard cap on any downloaded body (PDFs and office files can be large).
pub const MAX_BODY: usize = 25 * 1024 * 1024;
const WAYBACK_API: &str = "https://archive.org/wayback/available";
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

pub struct Fetched {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub struct FetchFail {
    pub status: Option<u16>,
    pub msg: String,
    transient: bool,
    retry_after: Option<Duration>,
}

pub fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(d) = retry_after {
        return d.min(MAX_RETRY_AFTER);
    }
    let base = 500u64 << attempt.min(4);
    Duration::from_millis(base + rand::random_range(0..300))
}

async fn get_once(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Fetched, FetchFail> {
    let fail = |status, msg: String, transient, retry_after| FetchFail { status, msg, transient, retry_after };
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml,application/pdf,*/*;q=0.8")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            let transient = e.is_connect() || e.is_timeout();
            fail(None, format!("request failed: {}", e.without_url()), transient, None)
        })?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let transient = status.as_u16() == 429 || status.is_server_error();
        return Err(fail(Some(status.as_u16()), format!("HTTP {status}"), transient, retry_after));
    }
    if resp.content_length().is_some_and(|l| l as usize > MAX_BODY) {
        return Err(fail(None, format!("response larger than {} MB", MAX_BODY >> 20), false, None));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| fail(None, format!("read failed: {}", e.without_url()), true, None))?;
        let room = MAX_BODY - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if chunk.len() > room {
            break;
        }
    }
    Ok(Fetched { content_type, bytes })
}

/// GET with up to two retries on 429/5xx/connect/timeout errors.
pub async fn fetch_retry(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Fetched, FetchFail> {
    let mut attempt = 0;
    loop {
        match get_once(client, url, timeout).await {
            Err(e) if e.transient && attempt < 2 => {
                tokio::time::sleep(backoff(attempt, e.retry_after)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Fetch, then on 403/404/410 try the closest Wayback snapshot. The bool is
/// true when the archived copy was used.
pub async fn fetch_resilient(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<(Fetched, bool), FetchFail> {
    let mut err = match fetch_retry(client, url, timeout).await {
        Ok(f) => return Ok((f, false)),
        Err(e) => e,
    };
    if matches!(err.status, Some(403 | 404 | 410))
        && let Some(snap) = wayback_snapshot(client, WAYBACK_API, url).await
        && let Ok(f) = fetch_retry(client, &snap, timeout).await
    {
        return Ok((f, true));
    }
    if matches!(err.status, Some(403 | 404 | 410)) {
        err.msg.push_str(" (no archived copy)");
    }
    Err(err)
}

pub fn wayback_query_url(api: &str, url: &str) -> String {
    format!("{api}?url={}", urlencoding::encode(url))
}

/// Closest snapshot from a Wayback availability response, as a raw-content
/// (`id_`) https URL without the Wayback toolbar.
pub fn parse_wayback(json: &serde_json::Value) -> Option<String> {
    let closest = json.pointer("/archived_snapshots/closest")?;
    if !closest.get("available")?.as_bool()? {
        return None;
    }
    let url = closest.get("url")?.as_str()?.replacen("http://", "https://", 1);
    let (head, rest) = url.split_once("/web/")?;
    let (ts, orig) = rest.split_once('/')?;
    Some(format!("{head}/web/{ts}id_/{orig}"))
}

async fn wayback_snapshot(client: &reqwest::Client, api: &str, url: &str) -> Option<String> {
    let resp = client
        .get(wayback_query_url(api, url))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    parse_wayback(&resp.json().await.ok()?)
}

/// `https://xx.wikipedia.org/wiki/Title` -> plain wikitext URL.
pub fn wikipedia_raw_url(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let title = u.path().strip_prefix("/wiki/")?;
    if !host.ends_with(".wikipedia.org") || title.is_empty() {
        return None;
    }
    Some(format!("https://{host}/w/index.php?title={title}&action=raw"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikipedia_urls_become_raw() {
        assert_eq!(
            wikipedia_raw_url("https://en.wikipedia.org/wiki/Rust_(programming_language)#History").as_deref(),
            Some("https://en.wikipedia.org/w/index.php?title=Rust_(programming_language)&action=raw")
        );
        assert!(wikipedia_raw_url("https://example.com/wiki/X").is_none());
        assert!(wikipedia_raw_url("https://en.wikipedia.org/w/index.php?title=X").is_none());
    }

    #[test]
    fn wayback_builder_and_parser() {
        assert_eq!(
            wayback_query_url("https://archive.org/wayback/available", "https://a.b/c?d=e"),
            "https://archive.org/wayback/available?url=https%3A%2F%2Fa.b%2Fc%3Fd%3De"
        );
        let hit = serde_json::json!({"archived_snapshots":{"closest":{"available":true,
            "url":"http://web.archive.org/web/20200101000000/http://a.b/c"}}});
        assert_eq!(
            parse_wayback(&hit).as_deref(),
            Some("https://web.archive.org/web/20200101000000id_/http://a.b/c")
        );
        assert!(parse_wayback(&serde_json::json!({"archived_snapshots":{}})).is_none());
    }

    #[test]
    fn backoff_honours_retry_after_with_cap() {
        assert_eq!(backoff(0, Some(Duration::from_secs(3))), Duration::from_secs(3));
        assert_eq!(backoff(0, Some(Duration::from_secs(99))), MAX_RETRY_AFTER);
        assert!(backoff(1, None) >= Duration::from_millis(1000));
    }
}
