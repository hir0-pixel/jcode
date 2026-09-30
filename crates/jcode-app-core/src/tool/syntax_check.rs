//! Cheap syntax feedback after a write/edit/patch (ported idea: Hermes
//! `file_operations` lint-after-write, reporting only NEW errors).
//!
//! Never refuses the write: a one-line note is appended so the model can fix
//! the file next turn. Only reported when the file parsed cleanly before the
//! edit. Checkers that need an external binary are skipped silently when the
//! binary is not on PATH.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_BYTES: usize = 1_000_000;
const TIMEOUT: Duration = Duration::from_millis(300);

const PY_CHECK: &str = "import ast,sys\nsrc=sys.stdin.read()\ntry:\n ast.parse(src)\nexcept SyntaxError as e:\n print(f'{e.lineno or 1}\\t{e.msg}');sys.exit(1)\n";

/// Returns `(line, message)` when the source is syntactically invalid.
async fn check(ext: &str, src: &str) -> Option<(usize, String)> {
    match ext {
        "json" => serde_json::from_str::<serde_json::Value>(src)
            .err()
            .map(|e| (e.line(), e.to_string())),
        "py" => run("python3", &["-c", PY_CHECK], src, true).await,
        "js" | "mjs" | "cjs" => run("node", &["--check", "-"], src, false).await,
        "go" => run("gofmt", &["-e"], src, false).await,
        "rs" => run("rustfmt", &["--check", "--emit", "stdout"], src, false).await,
        _ => None,
    }
}

async fn run(bin: &str, args: &[&str], src: &str, tabbed: bool) -> Option<(usize, String)> {
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?; // binary missing: skip silently
    let mut stdin = child.stdin.take()?;
    let data = src.as_bytes().to_vec();
    tokio::spawn(async move {
        let _ = stdin.write_all(&data).await;
    });
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(if tabbed { &out.stdout } else { &out.stderr });
    let first = text.lines().find(|l| !l.trim().is_empty())?;
    if tabbed {
        let (line, msg) = first.split_once('\t')?;
        return Some((line.parse().ok()?, msg.to_string()));
    }
    // "<stdin>:3:5: msg" (gofmt) / "--> <stdin>:3:5" (rustfmt) / "file:3" (node)
    let line = text
        .lines()
        .find_map(|l| {
            let rest = l.split("<stdin>:").nth(1).or_else(|| l.strip_prefix("[stdin]:"))?;
            rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
        })
        .unwrap_or(1);
    Some((line, crate::util::truncate_str(first, 160).to_string()))
}

/// A one-line note when `after` has a syntax error that `before` did not.
pub async fn new_syntax_note(path: &Path, before: Option<&str>, after: &str) -> Option<String> {
    if after.len() > MAX_BYTES {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (line, msg) = check(&ext, after).await?;
    if let Some(before) = before
        && before.len() <= MAX_BYTES
        && check(&ext, before).await.is_some()
    {
        return None;
    }
    Some(format!(
        "Syntax error at {}:{line}: {msg} (file was written; fix it)",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn json_error_reported_only_when_new() {
        let p = Path::new("a.json");
        let note = new_syntax_note(p, Some("{}"), "{\n\"a\": }").await.unwrap();
        assert!(note.contains("a.json:2"), "{note}");
        assert!(new_syntax_note(p, Some("{"), "{\n\"a\": }").await.is_none());
        assert!(new_syntax_note(p, None, "{}").await.is_none());
        assert!(new_syntax_note(Path::new("a.txt"), None, "{").await.is_none());
    }

    #[tokio::test]
    async fn python_error_has_line() {
        if std::process::Command::new("python3").arg("-V").output().is_err() {
            return;
        }
        let note = new_syntax_note(Path::new("m.py"), Some("x = 1\n"), "x = 1\ndef f(:\n")
            .await
            .unwrap();
        assert!(note.contains("m.py:2"), "{note}");
    }
}
