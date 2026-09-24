//! Drives real worker processes: state, recursion, confinement, limits.

use sovereign_prime::{LlmQuery, ReplHost};
use std::path::PathBuf;
use std::sync::Arc;

fn host() -> Arc<ReplHost> {
    ReplHost::new(PathBuf::from(env!("CARGO_BIN_EXE_prime-worker")))
}

fn upper() -> LlmQuery {
    Arc::new(|prompt: String| Box::pin(async move { Ok(prompt.to_uppercase()) }))
}

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("prime-test-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    dir
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

#[tokio::test(flavor = "multi_thread")]
async fn variables_persist_between_runs() {
    let h = host();
    let first = h.run("s", "x = 41", None, upper()).await.unwrap();
    assert!(first.fresh_state && first.error.is_none());
    let second = h.run("s", "x + 1", None, upper()).await.unwrap();
    assert_eq!(second.value.as_deref(), Some("42"));
    assert!(!second.fresh_state);
    // Sessions are isolated from each other.
    let other = h.run("other", "x", None, upper()).await.unwrap();
    assert!(other.error.unwrap().contains("NameError"));
}

#[tokio::test(flavor = "multi_thread")]
async fn llm_query_is_a_recursive_host_call() {
    let h = host();
    let out = h.run("s", "parts = ['a', 'b']\nr = [llm_query(p) for p in parts]\nprint(r)\n''.join(r)", None, upper()).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("'AB'"));
    assert_eq!(out.stdout.trim(), "['A', 'B']");
    assert_eq!(out.host_calls, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn load_reads_workspace_files_into_variables() {
    let h = host();
    let dir = workdir();
    let out = h.run("s", "t = load('src/notes.txt')\nlen(t.splitlines())", Some(&dir), upper()).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("3"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn load_is_confined_to_the_working_directory() {
    let h = host();
    let dir = workdir();
    let outside = std::env::temp_dir().join(format!("prime-outside-{}", rand_suffix()));
    std::fs::write(&outside, "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, dir.join("link.txt")).unwrap();
    for path in ["../x", "/etc/passwd", "link.txt", outside.to_str().unwrap()] {
        let code = format!("load({path:?})");
        let out = h.run("s", &code, Some(&dir), upper()).await.unwrap();
        let err = out.error.unwrap_or_default();
        assert!(err.contains("outside the working directory") || err.contains("not found"), "{path}: {err}");
    }
    let out = h.run("s", "load('src/notes.txt')", None, upper()).await.unwrap();
    assert!(out.error.unwrap().contains("working directory"));
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_file(outside).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_has_no_host_access() {
    let h = host();
    for (code, expect) in [
        ("open('/etc/passwd').read()", "PermissionError"),
        ("import os\nos.listdir('/')", "PermissionError"),
        ("subprocess_run('ls')", "NameError"),
        ("__import__('os')", "NameError"),
    ] {
        let out = h.run("s", code, None, upper()).await.unwrap();
        let err = out.error.unwrap_or_default();
        assert!(err.contains(expect), "{code}: {err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_call_budget_is_enforced() {
    let h = host();
    let out = h.run("s", "for i in range(40):\n    llm_query(str(i))", None, upper()).await.unwrap();
    assert!(out.error.unwrap().contains("budget"));
    assert_eq!(out.host_calls, 16);
}

#[tokio::test(flavor = "multi_thread")]
async fn runaway_code_hits_limits_and_the_session_survives() {
    let h = host();
    h.run("s", "keep = 7", None, upper()).await.unwrap();
    let out = h.run("s", "def f(n):\n    return f(n + 1)\nf(0)", None, upper()).await.unwrap();
    assert!(out.error.unwrap().contains("RecursionError"));
    let out = h.run("s", "keep", None, upper()).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("7"), "state survives a caught error");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_blowup_is_contained_in_the_worker() {
    let h = host();
    let result = h.run("s", "z = [0] * (10 ** 9)", None, upper()).await;
    match result {
        Ok(out) => assert!(out.error.unwrap_or_default().contains("MemoryError"), "expected MemoryError"),
        Err(err) => assert!(format!("{err:#}").contains("memory"), "{err:#}"),
    }
    // The host is still usable afterwards.
    let out = h.run("s", "1 + 1", None, upper()).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("2"));
}
