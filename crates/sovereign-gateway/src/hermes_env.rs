//! One credential owner. Hermes's Keys screen and `model.save_key` write API
//! keys to `$HERMES_HOME/.env`; the engine reads that same file FIRST for every
//! key it defines, so a key rotated there is never shadowed by a stale process
//! env var or a jcode-stored key. The process env and jcode's own env files
//! (`sovereign login`) only fill keys the Hermes `.env` does not define.

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
}
