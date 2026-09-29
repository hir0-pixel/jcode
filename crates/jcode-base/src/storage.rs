#![cfg_attr(test, allow(clippy::items_after_test_module))]

pub use jcode_storage::*;

use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::Path;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    jcode_storage::read_json_with_recovery_handler(path, |event| match event {
        jcode_storage::StorageRecoveryEvent::CorruptPrimary { path, error } => {
            crate::logging::warn(&format!(
                "Corrupt JSON at {}, trying backup: {}",
                path.display(),
                error
            ));
        }
        jcode_storage::StorageRecoveryEvent::RecoveredFromBackup { backup_path } => {
            crate::logging::info(&format!("Recovered from backup: {}", backup_path.display()));
        }
    })
}

#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(any(test, feature = "test-support"))]
pub fn test_env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(any(test, feature = "test-support"))]
pub fn lock_test_env() -> MutexGuard<'static, ()> {
    let guard = test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sandbox_test_home();
    guard
}

/// Tests that take the env lock but never set `JCODE_HOME` wrote their
/// sessions into the developer's real `~/.jcode`, where they showed up in the
/// desktop's session list. The first lock of the process points `JCODE_HOME`
/// at a process-lifetime temp dir when nothing set one; tests that set their
/// own still win, and restoring theirs lands back here, never on `~/.jcode`.
#[cfg(any(test, feature = "test-support"))]
fn sandbox_test_home() {
    static SANDBOX: OnceLock<std::path::PathBuf> = OnceLock::new();
    let home = SANDBOX.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("jcode-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    });
    if std::env::var_os("JCODE_HOME").is_none() {
        crate::env::set_var("JCODE_HOME", home);
    }
}

#[cfg(test)]
mod tests;
