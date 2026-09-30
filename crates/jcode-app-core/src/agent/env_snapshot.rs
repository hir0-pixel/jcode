//! One-time environment line for a session's first user message: cwd, which
//! common tools are on PATH (no subprocess per tool), python package probe
//! (one `python3 -c`, 1.5 s cap), and the first cwd entries. Hard cap 600 chars.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const TOOLS: [&str; 15] = [
    "python3", "pip", "node", "npm", "cargo", "go", "java", "gcc", "g++", "cmake", "git", "uv", "jq", "curl",
    "systemctl",
];
const MAX_CHARS: usize = 600;

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// "pytest 8.1, numpy 1.26" / "no pytest, no numpy", cached for the process.
fn python_probe() -> &'static str {
    static PROBE: OnceLock<String> = OnceLock::new();
    PROBE.get_or_init(|| {
        let code = "import importlib\nfor m in ('pytest','numpy'):\n try: print(m, importlib.import_module(m).__version__)\n except Exception: print('no', m)";
        let Ok(mut child) = std::process::Command::new("python3")
            .args(["-c", code])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return String::new();
        };
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < Duration::from_millis(1500) => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return String::new();
                }
            }
        }
        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut so, &mut out);
        }
        out.lines().collect::<Vec<_>>().join(", ")
    })
}

pub(super) fn enabled() -> bool {
    std::env::var("JCODE_ENV_SNAPSHOT").map_or(true, |v| v != "0")
        && crate::config::config().agents.environment_snapshot
}

pub(super) fn snapshot(cwd: &Path) -> String {
    let (have, missing): (Vec<&str>, Vec<&str>) = TOOLS.iter().partition(|t| on_path(t));
    let mut have: Vec<String> = have.iter().map(|t| t.to_string()).collect();
    if let Some(py) = have.iter_mut().find(|t| *t == "python3") {
        let probe = python_probe();
        if !probe.is_empty() {
            *py = format!("python3 ({probe})");
        }
    }
    let mut names: Vec<String> = std::fs::read_dir(cwd)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names.truncate(20);
    let text = format!(
        "<environment>cwd {}; have: {}; missing: {}; files: {}</environment>",
        cwd.display(),
        have.join(", "),
        missing.join(", "),
        names.join(" ")
    );
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_CHARS - 17).collect();
    cut.push_str("...</environment>");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_capped_and_lists_files() {
        let d = std::env::temp_dir().join(format!("envsnap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..40 {
            std::fs::write(d.join(format!("file_with_a_long_name_{i:02}.txt")), "").unwrap();
        }
        let s = snapshot(&d);
        assert!(s.chars().count() <= MAX_CHARS, "{}", s.chars().count());
        assert!(s.starts_with("<environment>cwd ") && s.ends_with("</environment>"));
        let d2 = d.join("sub");
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d2.join("a.txt"), "").unwrap();
        assert!(snapshot(&d2).contains("files: a.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
