//! Auto-verify gate helpers: project test-command detection and small probes.
//! Detection idea from Hermes `agent/coding_context.py` (marker files).

use std::path::Path;

/// First marker wins. Returns (short command name for spans, shell command).
pub(super) fn detect_test_command(dir: &Path) -> Option<(&'static str, String)> {
    let has = |f: &str| dir.join(f).is_file();
    if has("Cargo.toml") {
        return Some(("cargo", "cargo test".into()));
    }
    if has("go.mod") {
        return Some(("go", "go test ./...".into()));
    }
    if has("package.json") {
        let pkg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
        if pkg["scripts"]["test"].is_string() {
            return Some(("npm", "npm test --silent".into()));
        }
    }
    if has("build.gradle") || has("build.gradle.kts") {
        return Some(if has("gradlew") {
            ("gradle", "./gradlew test".into())
        } else {
            ("gradle", "gradle test".into())
        });
    }
    if has("pom.xml") {
        return Some(("mvn", "mvn -q test".into()));
    }
    if has("CMakeLists.txt") {
        return Some((
            "cmake",
            "cmake -B build && cmake --build build && ctest --test-dir build --output-on-failure"
                .into(),
        ));
    }
    if let Some(sub) = ["", "tests", "test"].into_iter().find(|sub| has_py_tests(&dir.join(sub))) {
        let pytest = std::process::Command::new("python3")
            .args(["-c", "import pytest"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        return Some(if pytest {
            ("pytest", "python3 -m pytest -x -q".into())
        } else {
            // Bare `unittest` only finds `test*.py` (not Exercism's `x_test.py`) and
            // reports "Ran 0 tests ... OK", a false pass.
            ("unittest", format!("python3 -m unittest discover -s {} -p '*test*.py'", if sub.is_empty() { "." } else { sub }))
        });
    }
    None
}

fn has_py_tests(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".py") && (n.ends_with("_test.py") || n.starts_with("test_"))
        })
    })
}

/// `*_test*`, `test_*`, `.test.`/`.spec.` names, or a `tests/` (`test/`) directory.
pub(super) fn is_test_path(path: &str) -> bool {
    let p = path.trim().trim_matches(['"', '\'']).replace('\\', "/");
    let mut parts = p.rsplit('/');
    let name = parts.next().unwrap_or("");
    name.contains("_test") || name.starts_with("test_") || name.contains(".test.")
        || name.contains(".spec.") || parts.any(|d| d == "tests" || d == "test")
}

/// Changed paths (`git status --porcelain`) in `dir`; empty outside a repo.
pub(super) fn git_changed(dir: &Path) -> Vec<String> {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.get(3..))
                .map(|p| p.rsplit(" -> ").next().unwrap_or(p).to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Exit code of a bash tool result: the tool appends `Exit code: N` on failure.
pub(super) fn exit_code_of(output: &str, is_error: bool) -> i64 {
    if is_error {
        return 1;
    }
    output
        .trim_end()
        .rsplit_once("Exit code: ")
        .and_then(|(_, n)| n.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir()
            .join(format!("av-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        for (f, c) in files {
            std::fs::write(d.join(f), c).unwrap();
        }
        d
    }

    fn cmd(files: &[(&str, &str)]) -> Option<String> {
        detect_test_command(&dir_with(files)).map(|(_, c)| c)
    }

    #[test]
    fn marker_detection_per_language() {
        assert_eq!(cmd(&[("Cargo.toml", "")]).unwrap(), "cargo test");
        assert_eq!(cmd(&[("go.mod", ""), ("Cargo.toml", "")]).unwrap(), "cargo test");
        assert_eq!(cmd(&[("go.mod", "")]).unwrap(), "go test ./...");
        assert_eq!(cmd(&[("package.json", r#"{"scripts":{"test":"jest"}}"#)]).unwrap(), "npm test --silent");
        assert!(cmd(&[("package.json", r#"{"scripts":{}}"#)]).is_none());
        assert_eq!(cmd(&[("build.gradle", "")]).unwrap(), "gradle test");
        assert_eq!(cmd(&[("build.gradle.kts", ""), ("gradlew", "")]).unwrap(), "./gradlew test");
        assert_eq!(cmd(&[("pom.xml", "")]).unwrap(), "mvn -q test");
        assert!(cmd(&[("CMakeLists.txt", "")]).unwrap().ends_with("--output-on-failure"));
        let py = cmd(&[("x_test.py", "")]).unwrap();
        assert!(py == "python3 -m pytest -x -q" || py.starts_with("python3 -m unittest discover -s . -p"));
        assert!(cmd(&[("test_x.py", "")]).is_some());
        assert!(cmd(&[("main.py", "")]).is_none());
        assert!(cmd(&[]).is_none());
    }

    #[test]
    fn test_paths_and_exit_codes() {
        assert!(is_test_path("src/foo_test.go"));
        assert!(is_test_path("a/tests/x.rs"));
        assert!(is_test_path("test_x.py"));
        assert!(is_test_path("a.spec.ts"));
        assert!(!is_test_path("src/contest.rs"));
        assert_eq!(exit_code_of("ok", false), 0);
        assert_eq!(exit_code_of("boom\n\nExit code: 2", false), 2);
        assert_eq!(exit_code_of("x", true), 1);
    }
}
