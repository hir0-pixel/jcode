//! One credential owner. Hermes's Keys screen and `model.save_key` write API
//! keys to `$HERMES_HOME/.env`; the engine reads that same file FIRST for every
//! key it defines, so a key rotated there is never shadowed by a stale process
//! env var or a jcode-stored key. The process env and jcode's own env files
//! (`sovereign login`) only fill keys the Hermes `.env` does not define.

use std::path::Path;
use std::sync::Once;

/// Every binding in dotenv text, parsed like python-dotenv (which Hermes loads `.env` with):
/// `export ` prefix, `'quoted'` / `"quoted"` values (multi-line, with dotenv's escapes), a ` # comment`
/// stripped from unquoted values, a key alone is unset. Later duplicates come later in the list.
/// Not done: `${VAR}` interpolation.
fn parse_all(content: &str) -> Vec<(String, Option<String>)> {
    let c: Vec<char> = content.strip_prefix('\u{feff}').unwrap_or(content).chars().collect();
    let (mut i, mut out) = (0, Vec::new());
    let blank = |c: char| c.is_whitespace() && c != '\n' && c != '\r';
    let skip = |i: &mut usize, f: &dyn Fn(char) -> bool| while *i < c.len() && f(c[*i]) { *i += 1 };
    let eol = |i: &mut usize| {
        while *i < c.len() && !matches!(c[*i], '\n' | '\r') { *i += 1 }
        if c.get(*i) == Some(&'\r') && c.get(*i + 1) == Some(&'\n') { *i += 1 }
        *i = (*i + 1).min(c.len());
    };
    // The text between quotes starting at c[i] (a quote), with its escapes decoded; i ends past the quote.
    let quoted = |i: &mut usize| -> Option<String> {
        let q = c[*i];
        let close = |literal: bool| {
            let mut j = *i + 1;
            while j < c.len() {
                if c[j] == '\\' && c.get(j + 1) == Some(&q) && !literal { j += 2 } else if c[j] == q { return Some(j) } else { j += 1 }
            }
            None
        };
        let end = close(false).or_else(|| close(true))?;
        let raw: Vec<char> = c[*i + 1..end].to_vec();
        *i = end + 1;
        let (mut text, mut k) = (String::new(), 0);
        while k < raw.len() {
            let esc = raw.get(k + 1).and_then(|n| match (q, *n) {
                (_, '\\') | (_, '\'') => Some(*n),
                ('"', '"') => Some('"'),
                ('"', 'a') => Some('\x07'), ('"', 'b') => Some('\x08'), ('"', 'f') => Some('\x0c'),
                ('"', 'n') => Some('\n'), ('"', 'r') => Some('\r'), ('"', 't') => Some('\t'), ('"', 'v') => Some('\x0b'),
                _ => None,
            });
            match (raw[k], esc) {
                ('\\', Some(e)) => { text.push(e); k += 2 }
                (ch, _) => { text.push(ch); k += 1 }
            }
        }
        Some(text)
    };
    loop {
        skip(&mut i, &|ch| ch.is_whitespace());
        if i >= c.len() { break }
        let word: String = c[i..].iter().take(7).collect();
        if word.strip_prefix("export").is_some_and(|r| r.chars().next().is_some_and(blank)) {
            i += 6;
            skip(&mut i, &blank);
        }
        let key = match c[i] {
            '#' => None,
            '\'' => quoted(&mut i),
            _ => {
                let start = i;
                skip(&mut i, &|ch| !ch.is_whitespace() && ch != '=' && ch != '#');
                (i > start).then(|| c[start..i].iter().collect::<String>())
            }
        };
        skip(&mut i, &blank);
        let mut value = None;
        let mut ok = key.is_some();
        if ok && c.get(i) == Some(&'=') {
            i += 1;
            skip(&mut i, &blank);
            value = Some(match c.get(i) {
                Some('\'' | '"') => match quoted(&mut i) { Some(v) => v, None => { ok = false; String::new() } },
                None | Some('\n' | '\r') => String::new(),
                _ => {
                    let start = i;
                    skip(&mut i, &|ch| !matches!(ch, '\n' | '\r'));
                    let part: String = c[start..i].iter().collect();
                    // python-dotenv: re.sub(r"\s+#.*", "", part).rstrip()
                    let cut = part.char_indices().find(|&(at, ch)| ch == '#' && part[..at].ends_with(char::is_whitespace)).map_or(part.len(), |(at, _)| at);
                    part[..cut].trim_end().to_string()
                }
            });
        }
        skip(&mut i, &blank);
        if c.get(i) == Some(&'#') {
            skip(&mut i, &|ch| !matches!(ch, '\n' | '\r'));
        }
        skip(&mut i, &blank);
        let clean = i >= c.len() || matches!(c[i], '\n' | '\r');
        eol(&mut i);
        if let (Some(key), true, true) = (key, ok, clean) {
            out.push((key, value));
        }
    }
    out
}

/// `KEY`'s value in dotenv text (the last definition wins, as in Hermes); empty means unset.
fn parse(content: &str, key: &str) -> Option<String> {
    parse_all(content).into_iter().rev().find(|(k, _)| k == key).and_then(|(_, v)| v).filter(|v| !v.is_empty())
}

fn resolve_in(home: &Path, key: &str) -> Option<String> {
    parse(&std::fs::read_to_string(home.join(".env")).ok()?, key)
}

fn resolve(key: &str) -> Option<String> {
    resolve_in(Path::new(&std::env::var_os("HERMES_HOME")?), key)
}

/// Tests that set `HERMES_HOME` hold this so they do not race each other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| jcode_base::provider_catalog::register_api_key_override_resolver(resolve));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_reads_the_key_hermes_saved() {
        let dir = std::env::temp_dir().join(format!("hermes-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_in(&dir, "FAKE_PROVIDER_API_KEY"), None);
        std::fs::write(
            dir.join(".env"),
            "# c\nOTHER=1\nexport FAKE_PROVIDER_API_KEY=\"fake-key-123\"\nEMPTY_API_KEY=\nFAKE_PROVIDER_API_KEY_2=x\n",
        )
        .unwrap();
        assert_eq!(resolve_in(&dir, "FAKE_PROVIDER_API_KEY").as_deref(), Some("fake-key-123"));
        assert_eq!(resolve_in(&dir, "FAKE_PROVIDER_API_KEY_2").as_deref(), Some("x"));
        assert_eq!(resolve_in(&dir, "EMPTY_API_KEY"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The fixture as both parsers read it: python-dotenv (Hermes's own venv) and `parse_all`.
    #[test]
    fn dotenv_parsing_agrees_with_python_dotenv_on_a_fixture() {
        let fixture = concat!(
            "\u{feff}# comment\n\nPLAIN=abc\nSPACED = spaced value  \nINLINE=value # trailing comment\nHASH=a#b\nHASHFIRST=#notacomment\n",
            "export EXPORTED=yes\nexport  TWO=2\nexporter=notexport\nDUP=first\nDUP=last\nEMPTY=\nBARE\n",
            "SQ='single # kept' # comment\nDQ=\"dq # kept\" # comment\nESC=\"tab\\there \\\"q\\\" back\\\\slash \\$\"\nSESC='it\\'s \\n raw'\n",
            "MULTI=\"line1\nline2\"\nCRLF=windows\r\nAFTER=1\nBAD=\"x\" junk\nUNCLOSED=\"never\nLAST=end",
        );
        let ours: std::collections::BTreeMap<String, Option<String>> = parse_all(fixture).into_iter().collect();
        let dir = std::env::temp_dir().join(format!("hermes-env-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(".env");
        std::fs::write(&file, fixture).unwrap();
        let python = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../hermes-agent/.venv/bin/python");
        if !python.exists() {
            eprintln!("skipped: no Hermes venv at {}", python.display());
            assert_eq!(ours["DUP"].as_deref(), Some("last"));
            return;
        }
        let out = std::process::Command::new(python)
            .args(["-c", "import sys,json\nfrom dotenv import dotenv_values\nprint(json.dumps(dotenv_values(sys.argv[1], encoding='utf-8-sig')))", file.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let theirs: std::collections::BTreeMap<String, Option<String>> = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(ours, theirs);
        assert_eq!(ours["DUP"].as_deref(), Some("last"));
        assert_eq!(parse(fixture, "INLINE").as_deref(), Some("value"));
        assert_eq!(parse(fixture, "EMPTY"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rotated_hermes_key_beats_stale_jcode_and_env_keys_which_only_fill_gaps() {
        use jcode_base::provider_catalog::load_api_key_from_env_or_config as load;
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hermes-env-order-{}", std::process::id()));
        let (jcode, hermes) = (dir.join("jcode"), dir.join("hermes"));
        std::fs::create_dir_all(&hermes).unwrap();
        // SAFETY: env is only touched under LOCK, with names unique to this test.
        unsafe {
            std::env::set_var("JCODE_HOME", &jcode);
            std::env::set_var("HERMES_HOME", &hermes);
            std::env::set_var("FAKE_ROTATED_API_KEY", "stale-env-key");
        }
        let config = jcode_base::storage::app_config_dir().unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("fake.env"), "FAKE_ROTATED_API_KEY=stale-jcode-key\nFAKE_GAP_API_KEY=jcode-gap-key\n").unwrap();
        std::fs::write(hermes.join(".env"), "FAKE_ROTATED_API_KEY=rotated-hermes-key\n").unwrap();
        register();
        assert_eq!(load("FAKE_ROTATED_API_KEY", "fake.env").as_deref(), Some("rotated-hermes-key"));
        assert_eq!(load("FAKE_GAP_API_KEY", "fake.env").as_deref(), Some("jcode-gap-key"));
        // Direct reads and auth probes (`env_secret`) see the same rotated key, and a key only Hermes has.
        use jcode_base::provider_catalog::env_secret;
        assert_eq!(env_secret("FAKE_ROTATED_API_KEY").as_deref(), Some("rotated-hermes-key"));
        std::fs::write(hermes.join(".env"), "FAKE_ROTATED_API_KEY=rotated-hermes-key\nFAKE_ONLY_API_KEY=hermes-only\n").unwrap();
        assert_eq!(env_secret("FAKE_ONLY_API_KEY").as_deref(), Some("hermes-only"));
        std::fs::write(hermes.join(".env"), "FAKE_ROTATED_API_KEY=rotated-hermes-key\n").unwrap();
        // Removed from Hermes: the jcode/env keys are the fallback again.
        std::fs::write(hermes.join(".env"), "").unwrap();
        assert_eq!(load("FAKE_ROTATED_API_KEY", "fake.env").as_deref(), Some("stale-env-key"));
        assert_eq!(env_secret("FAKE_ROTATED_API_KEY").as_deref(), Some("stale-env-key"));
        unsafe {
            std::env::remove_var("FAKE_ROTATED_API_KEY");
            std::env::remove_var("HERMES_HOME");
            std::env::remove_var("JCODE_HOME");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    const OPENAI_SSE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
    const CLAUDE_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"x\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// A fake OpenRouter-compatible / Anthropic server: records the key of every model request and streams "ok".
    fn fake_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (mut stream, log) = (stream.unwrap(), log.clone());
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())).unwrap_or(0);
                    let mut body = vec![0u8; length];
                    let _ = stream.read_exact(&mut body);
                    let claude = head.starts_with("POST") && head.contains("/messages");
                    if claude || (head.starts_with("POST") && head.contains("chat/completions")) {
                        let auth = head.lines().find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            match name.to_ascii_lowercase().as_str() {
                                "x-api-key" => Some(value.trim().to_string()),
                                "authorization" => value.trim().strip_prefix("Bearer ").map(str::to_string),
                                _ => None,
                            }
                        });
                        log.lock().unwrap().push(auth.unwrap_or_default());
                        let sse = if claude { CLAUDE_SSE } else { OPENAI_SSE };
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len());
                    } else {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
                    }
                });
            }
        });
        (base, seen)
    }

    /// The engine's model provider (the same `MultiProvider` the gateway serves), pointed at a fake
    /// OpenRouter-compatible endpoint, with Hermes's `.env` as the only key source.
    #[tokio::test]
    async fn a_hermes_key_saved_or_rotated_reaches_the_next_model_request_through_multi_provider() {
        use jcode_base::provider::{MultiProvider, Provider, external};
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hermes-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (jcode, hermes) = (dir.join("jcode"), dir.join("hermes"));
        std::fs::create_dir_all(&hermes).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let (base, seen) = fake_server();
        let vars = [
            ("HOME", dir.join("home").to_string_lossy().into_owned()), ("JCODE_HOME", jcode.to_string_lossy().into_owned()),
            ("HERMES_HOME", hermes.to_string_lossy().into_owned()), ("JCODE_OPENROUTER_API_BASE", base),
            ("JCODE_OPENROUTER_MODEL_CATALOG", "false".into()), ("JCODE_INITIAL_PROVIDER_EXPLICIT", "1".into()),
            ("JCODE_ACTIVE_PROVIDER", "openrouter".into()), ("JCODE_NON_INTERACTIVE", "1".into()),
        ];
        let scrub = ["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "JCODE_OPENROUTER_API_KEY_NAME"];
        // SAFETY: env is only touched under ENV_LOCK.
        unsafe {
            for (k, v) in &vars { std::env::set_var(k, v); }
            for k in scrub { std::env::remove_var(k); }
        }
        register();
        external::register_openrouter_factory(|spec| {
            use external::OpenRouterRuntimeSpec;
            use jcode_provider_openrouter_runtime::OpenRouterProvider;
            let provider: std::sync::Arc<dyn Provider> = match spec {
                OpenRouterRuntimeSpec::Default => std::sync::Arc::new(OpenRouterProvider::new()?),
                OpenRouterRuntimeSpec::OpenRouterApiKey => std::sync::Arc::new(OpenRouterProvider::new_openrouter_api_key_runtime()?),
                OpenRouterRuntimeSpec::CompatibleProfile(p) => std::sync::Arc::new(OpenRouterProvider::new_openai_compatible_profile_runtime(p)?),
                OpenRouterRuntimeSpec::NamedProfile { name, config } => std::sync::Arc::new(OpenRouterProvider::new_named_openai_compatible(&name, &config)?),
            };
            Ok(provider)
        });
        let env_file = hermes.join(".env");
        let mut asked = 0;
        macro_rules! ask { ($p:expr) => {{
            let outcome = $p.complete_simple("hi", "be brief").await;
            let saw = seen.lock().unwrap().get(asked).cloned();
            if saw.is_some() { asked += 1; }
            let _ = asked;
            (outcome.is_ok(), saw)
        }}; }

        // 1. The key is saved before the engine starts; then rotated: the next request carries key-two.
        std::fs::write(&env_file, "OPENROUTER_API_KEY=key-one\n").unwrap();
        let provider = MultiProvider::new();
        assert_eq!(ask!(provider), (true, Some("key-one".to_string())));
        std::fs::write(&env_file, "OPENROUTER_API_KEY=key-two\n").unwrap();
        assert_eq!(ask!(provider), (true, Some("key-two".to_string())), "a rotated key applies with no restart");

        // 2. Disconnect (key removed from `.env`): the key the provider was built with must not keep
        //    authenticating - not even before the auth-changed signal arrives, nor after it (on the path a
        //    live session takes, or the plain one).
        std::fs::write(&env_file, "").unwrap();
        let (ok, saw) = ask!(provider);
        assert!(!ok && saw.is_none(), "a disconnected key is not used: {saw:?}");
        provider.on_auth_changed_preserve_current_provider();
        assert!(!ask!(provider).0);
        std::fs::write(&env_file, "OPENROUTER_API_KEY=key-again\n").unwrap();
        provider.on_auth_changed();
        assert_eq!(ask!(provider), (true, Some("key-again".to_string())));
        std::fs::write(&env_file, "").unwrap();
        provider.on_auth_changed();
        assert!(!ask!(provider).0);

        // 3. First save after the engine started with no key: nothing serves until the auth-changed signal.
        drop(provider);
        let cold = MultiProvider::new();
        assert!(!ask!(cold).0, "no key yet");
        std::fs::write(&env_file, "OPENROUTER_API_KEY=key-three\n").unwrap();
        cold.on_auth_changed();
        assert_eq!(ask!(cold), (true, Some("key-three".to_string())), "notify_auth_changed hot-initialises the provider");

        // SAFETY: as above.
        unsafe {
            for (k, _) in &vars { std::env::remove_var(k); }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The Anthropic runtime (its API-base mode) and the OpenAI API-key loader read Hermes's `.env`
    /// afresh, so a rotated key reaches the next request there too.
    #[tokio::test]
    async fn a_rotated_hermes_key_reaches_the_anthropic_runtime_and_the_openai_key_loader() {
        use jcode_base::provider::Provider;
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hermes-rotate-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (jcode, hermes) = (dir.join("jcode"), dir.join("hermes"));
        std::fs::create_dir_all(&hermes).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let (base, seen) = fake_server();
        let vars = [
            ("HOME", dir.join("home").to_string_lossy().into_owned()), ("JCODE_HOME", jcode.to_string_lossy().into_owned()),
            ("HERMES_HOME", hermes.to_string_lossy().into_owned()), ("JCODE_ANTHROPIC_API_BASE", base),
            ("JCODE_NON_INTERACTIVE", "1".into()),
        ];
        // SAFETY: env is only touched under ENV_LOCK.
        unsafe {
            for (k, v) in &vars { std::env::set_var(k, v); }
            for k in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY"] { std::env::remove_var(k); }
        }
        register();
        let env_file = hermes.join(".env");
        std::fs::write(&env_file, "ANTHROPIC_API_KEY=claude-one\nOPENAI_API_KEY=sk-one\n").unwrap();
        let claude = jcode_provider_anthropic_runtime::AnthropicProvider::new();
        let key = |n: usize| seen.lock().unwrap().get(n).cloned();
        claude.complete_simple("hi", "be brief").await.unwrap();
        assert_eq!(key(0).as_deref(), Some("claude-one"));
        let openai = || jcode_base::auth::codex::load_api_key_credentials().unwrap().access_token;
        assert_eq!(openai(), "sk-one");
        std::fs::write(&env_file, "ANTHROPIC_API_KEY=claude-two\nOPENAI_API_KEY=sk-two\n").unwrap();
        claude.complete_simple("hi", "be brief").await.unwrap();
        assert_eq!(key(1).as_deref(), Some("claude-two"), "the rotated Anthropic key is on the next request");
        assert_eq!(openai(), "sk-two");
        // SAFETY: as above.
        unsafe {
            for (k, _) in &vars { std::env::remove_var(k); }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
