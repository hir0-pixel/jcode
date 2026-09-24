//! Request authentication for the gateway: bearer token, Host (DNS rebinding)
//! and Origin (cross-site WebSocket hijacking) checks. Mirrors Hermes's
//! `_ws_host_origin_reason`: packaged Electron origins (`file://`, `app://`,
//! `null`) are trusted because the token is the real boundary there.

use rand::RngCore;
use subtle::ConstantTimeEq;

/// 256-bit random token, hex encoded.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time token comparison. An empty expected token never matches.
pub fn token_matches(expected: &str, presented: Option<&str>) -> bool {
    match presented {
        Some(p) if !expected.is_empty() => {
            expected.len() == p.len() && bool::from(expected.as_bytes().ct_eq(p.as_bytes()))
        }
        _ => false,
    }
}

/// The token from `?token=` or `Authorization: Bearer`.
pub fn presented_token<'a>(query: Option<&'a str>, authorization: Option<&'a str>) -> Option<String> {
    if let Some(value) = authorization.and_then(|v| v.strip_prefix("Bearer ")) {
        return Some(value.trim().to_string());
    }
    query_param(query?, "token")
}

pub fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hosts a request may name. `bound` is the listening `ip:port`.
fn host_accepted(host: &str, bound_port: u16, bound_ip: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let (name, port) = match host.rsplit_once(':') {
        Some((n, p)) if !n.ends_with(']') || n.starts_with('[') => (n.to_string(), p.parse::<u16>().ok()),
        _ => (host.clone(), None),
    };
    if port.is_some_and(|p| p != bound_port) {
        return false;
    }
    let name = name.trim_start_matches('[').trim_end_matches(']');
    matches!(name, "127.0.0.1" | "localhost" | "::1") || name == bound_ip
}

/// `None` when allowed, otherwise the refusal reason (never includes the token).
pub fn host_origin_reason(
    host: Option<&str>,
    origin: Option<&str>,
    bound_port: u16,
    bound_ip: &str,
) -> Option<String> {
    let host = host.unwrap_or_default();
    if !host_accepted(host, bound_port, bound_ip) {
        return Some(format!("host_mismatch host={}", truncate(host)));
    }
    let Some(origin) = origin.filter(|o| !o.is_empty()) else {
        return None;
    };
    let lower = origin.to_ascii_lowercase();
    let rest = match lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://")) {
        Some(rest) => rest,
        None => return None, // file://, app://, null: packaged Electron
    };
    let netloc = rest.split('/').next().unwrap_or_default();
    if netloc.is_empty() || !host_accepted(netloc, bound_port, bound_ip) {
        return Some(format!("origin_mismatch origin={}", truncate(origin)));
    }
    None
}

fn truncate(value: &str) -> String {
    value.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_long() {
        let a = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, generate_token());
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", Some("abc")));
        assert!(!token_matches("abc", Some("abd")));
        assert!(!token_matches("abc", Some("abcd")));
        assert!(!token_matches("abc", None));
        assert!(!token_matches("", Some("")));
    }

    #[test]
    fn token_sources() {
        assert_eq!(presented_token(Some("token=a%2Bb&x=1"), None).as_deref(), Some("a+b"));
        assert_eq!(presented_token(None, Some("Bearer xyz")).as_deref(), Some("xyz"));
        assert_eq!(presented_token(Some("ticket=1"), None), None);
        assert_eq!(query_param("token=%zz", "token").as_deref(), Some("%zz"));
        assert_eq!(query_param("token=%4", "token").as_deref(), Some("%4"));
    }

    #[test]
    fn host_and_origin_rules() {
        let ok = |h, o| host_origin_reason(Some(h), o, 5000, "127.0.0.1").is_none();
        assert!(ok("127.0.0.1:5000", None));
        assert!(ok("localhost:5000", Some("file://")));
        assert!(ok("localhost:5000", Some("null")));
        assert!(ok("127.0.0.1:5000", Some("http://127.0.0.1:5000")));
        assert!(!ok("evil.com:5000", None));
        assert!(!ok("127.0.0.1:5001", None));
        assert!(!ok("127.0.0.1:5000", Some("http://evil.com")));
        assert!(!ok("127.0.0.1:5000", Some("https://127.0.0.1:9999")));
        assert!(host_origin_reason(None, None, 5000, "127.0.0.1").is_some());
    }
}
