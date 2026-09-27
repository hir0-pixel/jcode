//! Drives real worker processes: state, recursion, confinement, limits.

use sovereign_prime::host::Refine;
use sovereign_prime::{LlmQuery, ReplHost};
use std::path::PathBuf;
use std::sync::Arc;

fn host() -> Arc<ReplHost> {
    ReplHost::new(PathBuf::from(
        std::env::var("SOVEREIGN_HERMES_PYTHON").expect("staged Hermes CPython"),
    ))
}

fn upper() -> LlmQuery {
    Arc::new(|prompt: String| Box::pin(async move { Ok(prompt.to_uppercase()) }))
}

fn no_refine() -> Refine {
    Arc::new(|_op: String| Box::pin(async move { Ok(r#"{"scheduled":false}"#.to_string()) }))
}

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "prime-test-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    dir
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[tokio::test(flavor = "multi_thread")]
async fn variables_persist_between_runs() {
    let h = host();
    let first = h
        .run(
            "s",
            "x = 41",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert!(first.fresh_state && first.error.is_none());
    let second = h
        .run(
            "s",
            "x + 1",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(second.value.as_deref(), Some("42"));
    assert!(!second.fresh_state);
    // Sessions are isolated from each other.
    let other = h
        .run(
            "other",
            "x",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert!(other.error.unwrap().contains("NameError"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stdlib_imports_and_python_package_skills_work() {
    let skills = PathBuf::from(std::env::var("JCODE_HOME").unwrap()).join("skills");
    let package = skills.join("parity_probe/src/parity_probe");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("__init__.py"),
        "def twice(value):\n    return value * 2\n",
    )
    .unwrap();
    let h = host();
    let out = h.run(
        "imports",
        "import json, re\nimport parity_probe\nmatch = re.search(r'(\\d+)', 'value=21')\njson.dumps({'result': parity_probe.twice(int(match.group(1)))})",
        None, upper(), no_refine(), sovereign_prime::host::ExtraHostFns::default(), true,
    ).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("'{\"result\": 42}'"));
    drop(h);
    std::fs::remove_dir_all(skills.join("parity_probe")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn llm_query_is_a_recursive_host_call() {
    let h = host();
    let out = h.run("s", "parts = ['a', 'b']\nr = []\nfor p in parts:\n    r.append(await llm_query(p))\nprint(r)\n''.join(r)", None, upper(), no_refine(), sovereign_prime::host::ExtraHostFns::default(), true).await.unwrap();
    assert_eq!(out.value.as_deref(), Some("'AB'"));
    assert_eq!(out.stdout.trim(), "['A', 'B']");
    assert_eq!(out.host_calls, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn prime_skill_host_request_bridges_goal_and_refine() {
    let h = host();
    let extra = sovereign_prime::host::ExtraHostFns {
        goal: Arc::new(|op| {
            Box::pin(async move {
                let op: serde_json::Value = serde_json::from_str(&op).unwrap();
                match op["op"].as_str().unwrap() {
                    "get" => Ok(r#"{"goal":"ship parity"}"#.to_string()),
                    "create" => Ok(format!(r#"{{"created":{}}}"#, op["text"])),
                    other => anyhow::bail!("unexpected goal operation: {other}"),
                }
            })
        }),
        websearch: Arc::new(|op| {
            Box::pin(async move {
                let op: serde_json::Value = serde_json::from_str(&op).unwrap();
                anyhow::ensure!(op["query"] == "local", "query was not forwarded");
                anyhow::ensure!(op["num_results"] == 2, "result limit was not forwarded");
                Ok("safe local results".to_string())
            })
        }),
        ..sovereign_prime::host::ExtraHostFns::default()
    };
    let out = h.run(
        "prime-api",
        "import rlm\ngoal = await rlm.host_request('goal.get')\ncreated = await rlm.host_request('goal.create', {'objective': 'ship parity'})\nrefine = await rlm.host_request('refine.status')\nsearch = await rlm.host_request('websearch.run', {'query': 'local', 'num_results': 2})\n(goal['goal'], created['created'], refine['scheduled'], search['results'])",
        None,
        upper(),
        no_refine(),
        extra,
        true,
    ).await.unwrap();
    assert_eq!(
        out.value.as_deref(),
        Some("('ship parity', 'ship parity', False, 'safe local results')")
    );
    assert_eq!(out.host_calls, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn load_reads_workspace_files_into_variables() {
    let h = host();
    let dir = workdir();
    let out = h
        .run(
            "s",
            "t = await load('src/notes.txt')\nlen(t.splitlines())",
            Some(&dir),
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
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
        let out = h
            .run(
                "s",
                &code,
                Some(&dir),
                upper(),
                no_refine(),
                sovereign_prime::host::ExtraHostFns::default(),
                true,
            )
            .await
            .unwrap();
        let err = out.error.unwrap_or_default();
        assert!(
            err.contains("outside the working directory") || err.contains("not found"),
            "{path}: {err}"
        );
    }
    let out = h
        .run(
            "s",
            "load('src/notes.txt')",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert!(out.error.unwrap().contains("working directory"));
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::remove_file(outside).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_has_no_host_access() {
    let h = host();
    for (code, expect) in [
        ("open('/etc/passwd').read()", "PermissionError"),
        ("import os\nos.listdir('/Users')", "PermissionError"),
        (
            "import socket\nsocket.create_connection(('127.0.0.1', 9), .1)",
            "PermissionError",
        ),
        (
            "open('/private/tmp/sovereign-repl-outside-write', 'w').write('x')",
            "PermissionError",
        ),
    ] {
        let out = h
            .run(
                "s",
                code,
                None,
                upper(),
                no_refine(),
                sovereign_prime::host::ExtraHostFns::default(),
                true,
            )
            .await
            .unwrap();
        let err = out.error.unwrap_or_default();
        assert!(err.contains(expect), "{code}: {err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_call_budget_is_enforced() {
    let h = host();
    let out = h
        .run(
            "s",
            "for i in range(40):\n    await llm_query(str(i))",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert!(out.error.unwrap().contains("budget"));
    assert_eq!(out.host_calls, 16);
}

#[tokio::test(flavor = "multi_thread")]
async fn runaway_code_hits_limits_and_the_session_survives() {
    let h = host();
    h.run(
        "s",
        "keep = 7",
        None,
        upper(),
        no_refine(),
        sovereign_prime::host::ExtraHostFns::default(),
        true,
    )
    .await
    .unwrap();
    let out = h
        .run(
            "s",
            "def f(n):\n    return f(n + 1)\nf(0)",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert!(out.error.unwrap().contains("RecursionError"));
    let out = h
        .run(
            "s",
            "keep",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        out.value.as_deref(),
        Some("7"),
        "state survives a caught error"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_blowup_is_contained_in_the_worker() {
    let h = host();
    let started = std::time::Instant::now();
    let result = h
        .run(
            "s",
            "z = b'x' * (400 * 1024 * 1024)\nimport time\ntime.sleep(3)",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await;
    match result {
        Ok(out) => assert!(
            out.error.unwrap_or_default().contains("MemoryError"),
            "expected MemoryError"
        ),
        Err(err) => assert!(format!("{err:#}").contains("memory"), "{err:#}"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "RSS watchdog did not stop the worker promptly"
    );
    // The host is still usable afterwards.
    let out = h
        .run(
            "s",
            "1 + 1",
            None,
            upper(),
            no_refine(),
            sovereign_prime::host::ExtraHostFns::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(out.value.as_deref(), Some("2"));
}
