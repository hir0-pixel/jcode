//! One credential store. Hermes's Keys screen and `model.save_key` write API
//! keys to `$HERMES_HOME/.env`; instead of copying them into jcode's own env
//! files, the engine reads that same file as its last-resort key source
//! (after the process env and jcode's files, so `sovereign login` still wins).

use std::path::Path;
use std::sync::Once;

/// `KEY=value` from dotenv text (optional `export `, one layer of quotes); empty means unset.
fn parse(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let rest = line.trim().trim_start_matches("export ").trim_start();
        let value = rest.strip_prefix(key)?.trim_start().strip_prefix('=')?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn resolve_in(home: &Path, key: &str) -> Option<String> {
    parse(&std::fs::read_to_string(home.join(".env")).ok()?, key)
}

fn resolve(key: &str) -> Option<String> {
    resolve_in(Path::new(&std::env::var_os("HERMES_HOME")?), key)
}

pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| jcode_base::provider_catalog::register_api_key_fallback_resolver(resolve));
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
}
