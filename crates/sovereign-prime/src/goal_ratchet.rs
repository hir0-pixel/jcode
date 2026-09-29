//! AVO-style ratchet for goals (NVIDIA arXiv 2603.24517 sec. 3): a per-command
//! score vector from real execution, hidden-ref checkpoints only when the
//! score matches or improves, regression notes, and the supervisor request.
//! No model call here; the one rare supervisor call lives in the gateway.

use crate::agent_loop::SessionGoal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

/// test command -> (passed, failed)
pub type Score = BTreeMap<String, (u32, u32)>;
pub const MAX_LINEAGE: usize = 20;
/// Continuation turns between two supervisor calls.
pub const SUPERVISOR_GAP: i64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub n: u32,
    /// Hidden git ref, or "no-vcs".
    pub git_ref: String,
    pub score: String,
    pub line: String,
}

impl Checkpoint {
    pub fn to_json(&self) -> Value {
        json!({ "n": self.n, "ref": self.git_ref, "score": self.score, "line": self.line })
    }
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            n: v["n"].as_u64()? as u32,
            git_ref: v["ref"].as_str()?.into(),
            score: v["score"].as_str().unwrap_or("").into(),
            line: v["line"].as_str().unwrap_or("").into(),
        })
    }
}

pub fn score_to_json(s: &Score) -> Value {
    s.iter().map(|(k, (p, f))| (k.clone(), json!([p, f]))).collect::<serde_json::Map<_, _>>().into()
}

pub fn score_from_json(v: &Value) -> Score {
    v.as_object()
        .map(|o| o.iter().filter_map(|(k, x)| Some((k.clone(), (x[0].as_u64()? as u32, x[1].as_u64()? as u32)))).collect())
        .unwrap_or_default()
}

/// Split a shell line into simple commands at unquoted `&&`, `||`, `;`, `|`, `&` and newlines.
fn simple_commands(line: &str) -> Vec<Vec<String>> {
    let (mut cmds, mut words, mut word) = (Vec::new(), Vec::new(), String::new());
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    let flush = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '\\') => word.extend(chars.next()),
            (None, ';' | '|' | '&' | '\n') => {
                flush(&mut word, &mut words);
                while chars.next_if(|n| matches!(n, '|' | '&')).is_some() {}
                cmds.push(std::mem::take(&mut words));
            }
            (None, c) if c.is_whitespace() => flush(&mut word, &mut words),
            (None, c) => word.push(c),
        }
    }
    flush(&mut word, &mut words);
    cmds.push(words);
    cmds.retain(|w| !w.is_empty());
    cmds
}

/// Whether one simple command (program and arguments) runs a known test/build/check/lint runner.
fn runs_verifier(words: &[String]) -> bool {
    let mut i = 0;
    // Leading `VAR=value` assignments and transparent wrappers.
    while i < words.len() && ((words[i].contains('=') && !words[i].starts_with('-')) || matches!(words[i].as_str(), "sudo" | "time" | "env" | "command" | "exec" | "nice")) {
        let wrapper = !words[i].contains('=');
        i += 1;
        while wrapper && words.get(i).is_some_and(|w| w.starts_with('-')) {
            i += 1;
        }
    }
    let Some(program) = words.get(i) else { return false };
    let program = program.rsplit('/').next().unwrap_or(program);
    let args: Vec<&str> = words[i + 1..].iter().map(String::as_str).collect();
    let plain: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with('-') && !a.starts_with('+')).collect();
    let script = |name: &str| ["test", "build", "lint"].iter().any(|k| name == *k || name.strip_prefix(k).is_some_and(|rest| rest.starts_with(':')));
    let rest = |n: usize| words[(i + n).min(words.len())..].to_vec();
    match program {
        "cargo" => plain.first().is_some_and(|s| matches!(*s, "test" | "build" | "check" | "clippy")),
        "npm" | "pnpm" | "yarn" | "bun" => match plain.as_slice() {
            ["test" | "build" | "lint", ..] => true,
            ["run" | "run-script", name, ..] => script(name),
            ["exec" | "dlx" | "x", ..] => runs_verifier(&rest(args.iter().position(|a| !a.starts_with('-')).map_or(1, |p| p + 2))),
            [name] if program == "yarn" => script(name),
            _ => false,
        },
        "npx" | "bunx" | "pnpx" => runs_verifier(&rest(1 + args.iter().take_while(|a| a.starts_with('-')).count())),
        "pytest" | "py.test" | "jest" | "vitest" | "tsc" => true,
        "python" | "python3" => args.windows(2).any(|w| w == ["-m", "pytest"]),
        "go" | "swift" | "deno" => plain.first() == Some(&"test"),
        "make" | "gmake" => plain.iter().any(|t| matches!(*t, "test" | "check")),
        "gradle" | "gradlew" | "mvn" | "mvnw" => plain.iter().any(|t| *t == "test"),
        _ => false,
    }
}

/// Whether a bash command line runs a real test/build/check/lint runner (by program and
/// subcommand, not by a substring: `echo test`, `git checkout` and `ls tests` are not verification).
pub fn is_verify_command(line: &str) -> bool {
    simple_commands(line).iter().any(|words| runs_verifier(words))
}

pub fn command_key(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect()
}

/// Sum "N passed" / "M failed" over runner summary lines (cargo test, pytest,
/// jest, vitest). None when the output has no such summary.
pub fn parse_counts(out: &str) -> Option<(u32, u32)> {
    let (mut p, mut f, mut seen) = (0u32, 0u32, false);
    for line in out.lines() {
        let l = line.trim();
        if !(l.starts_with("test result:") || l.contains("Tests:") || l.starts_with("Tests ") || l.starts_with('=') || l.contains(" in ")) {
            continue;
        }
        let words: Vec<&str> = l.split(|c: char| c.is_whitespace() || ",;|=".contains(c)).filter(|w| !w.is_empty()).collect();
        for pair in words.windows(2) {
            let Ok(n) = pair[0].parse::<u32>() else { continue };
            match pair[1] {
                "passed" => (p, seen) = (p + n, true),
                "failed" => (f, seen) = (f + n, true),
                _ => {}
            }
        }
    }
    seen.then_some((p, f))
}

/// Correctness first: a non-zero exit never scores as passing.
pub fn score_of(ok: bool, out: &str) -> (u32, u32) {
    match (parse_counts(out), ok) {
        (Some((p, 0)), false) => (p, 1),
        (Some(c), _) => c,
        (None, true) => (1, 0),
        (None, false) => (0, 1),
    }
}

/// Matches or improves every re-run command, no new failures.
pub fn accepts(best: &Score, turn: &Score) -> bool {
    !turn.is_empty()
        && turn.iter().all(|(k, &(p, f))| match best.get(k) {
            // Fewer passes with no failures is removed tests, not a regression: it must not
            // freeze every later checkpoint. `regression()` still catches new failures.
            Some(&(bp, bf)) => f <= bf && (p >= bp || f == 0),
            None => p > 0,
        })
}

/// A command that was tracked as better now fails more.
pub fn regression(best: &Score, turn: &Score) -> Option<String> {
    turn.iter().find_map(|(k, &(p, f))| {
        let &(bp, bf) = best.get(k)?;
        (f > bf).then(|| format!("`{k}` was {bp}p/{bf}f, now {p}p/{f}f"))
    })
}

pub fn fmt_score(s: &Score) -> String {
    let mut out: Vec<String> = s.iter().take(3).map(|(k, (p, f))| format!("{}: {p}p/{f}f", k.chars().take(30).collect::<String>())).collect();
    if s.len() > 3 {
        out.push(format!("+{} more", s.len() - 3));
    }
    out.join("; ")
}

/// Options for every git call in a user's repo: its config must not run anything (fsmonitor, hooks),
/// and no global attributes file may attach a clean/smudge filter. Callers also set `GIT_ATTR_NOSYSTEM=1`
/// (see [`safe_git_env`]) for the system-wide attributes file.
pub const SAFE_GIT: [&str; 6] = ["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "core.attributesFile=/dev/null"];

/// The environment half of [`SAFE_GIT`].
pub fn safe_git_env(c: &mut Command) -> &mut Command {
    c.env("GIT_ATTR_NOSYSTEM", "1")
}

/// A git call in the repo may not take longer than this (a huge tree, a hung filter).
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run `c`, killing it after `limit`; its stdout when it succeeded in time.
fn bounded(c: &mut Command, limit: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().unwrap_or_default();
    match status {
        Some(status) if status.success() => Some(String::from_utf8_lossy(&out).trim().to_string()),
        Some(_) => None,
        None => {
            eprintln!("akira: a git call ran over {}s and was stopped; the goal checkpoint is skipped", limit.as_secs());
            None
        }
    }
}

fn git(cwd: &Path, envs: &[(&str, &Path)], args: &[&str]) -> Option<String> {
    let mut c = Command::new("git");
    c.current_dir(cwd).args(SAFE_GIT).args(args).stdin(Stdio::null());
    safe_git_env(&mut c);
    for (k, v) in envs {
        c.env(k, v);
    }
    bounded(&mut c, GIT_TIMEOUT)
}

/// Snapshot the working tree (incl. untracked, honoring .gitignore) into a
/// hidden ref through a throwaway index: never touches the user's branch,
/// HEAD or index. None outside a git repo.
pub fn snapshot(cwd: &Path, session: &str, n: u32) -> Option<String> {
    git(cwd, &[], &["rev-parse", "--is-inside-work-tree"])?;
    // Unique per call: concurrent sessions (and retries) never share a scratch index.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let idx = std::env::temp_dir().join(format!(
        "akira-idx-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_file(&idx);
    // Seed from a copy of the real index so `add -A` reuses its stat cache and
    // only re-hashes changed files (the real index itself is never written).
    if let Some(real) = git(cwd, &[], &["rev-parse", "--git-path", "index"]) {
        let _ = std::fs::copy(cwd.join(real), &idx);
    }
    let e = [("GIT_INDEX_FILE", idx.as_path())];
    let sha = (|| {
        git(cwd, &e, &["add", "-A"])?;
        let tree = git(cwd, &e, &["write-tree"])?;
        let mut args = vec!["-c", "user.name=akira", "-c", "user.email=akira@localhost", "commit-tree", &tree, "-m", "akira goal checkpoint"];
        let head = git(cwd, &[], &["rev-parse", "-q", "--verify", "HEAD"]);
        if let Some(h) = &head {
            args.extend(["-p", h]);
        }
        git(cwd, &[], &args)
    })();
    let _ = std::fs::remove_file(&idx);
    let safe: String = session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let name = format!("refs/akira/goals/{safe}/{n}");
    git(cwd, &[], &["update-ref", &name, &sha?])?;
    Some(name)
}

impl SessionGoal {
    /// Apply one turn's observed score vector: regression note, or (matching
    /// or better + files changed) a ratchet checkpoint. Failed attempts stay
    /// only in the attempt log.
    pub(crate) fn ratchet(&mut self, turn: &Score, files_changed: bool, cwd: Option<&Path>, session: &str, line: &str) {
        if !turn.is_empty() {
            self.verify_runs += turn.len() as u32;
            self.verify_ok = turn.values().all(|&(_, f)| f == 0);
        }
        if let Some(msg) = regression(&self.best, turn) {
            self.regressed = Some(msg);
        } else if accepts(&self.best, turn) {
            self.regressed = None;
            if files_changed {
                self.best.extend(turn.iter().map(|(k, v)| (k.clone(), *v)));
                self.checkpoint_seq += 1;
                let n = self.checkpoint_seq;
                let git_ref = cwd.and_then(|c| snapshot(c, session, n)).unwrap_or_else(|| "no-vcs".into());
                if git_ref != "no-vcs" {
                    self.ref_cwd = cwd.map(|c| c.to_string_lossy().into_owned());
                }
                self.lineage.push(Checkpoint { n, git_ref, score: fmt_score(&self.best), line: line.into() });
                while self.lineage.len() > MAX_LINEAGE {
                    let old = self.lineage.remove(0);
                    if let (Some(c), true) = (cwd, old.git_ref != "no-vcs") {
                        git(c, &[], &["update-ref", "-d", &old.git_ref]);
                    }
                }
            }
        }
    }

    /// Delete this goal's hidden checkpoint refs from the repo they were taken in,
    /// except `keep` (completion keeps the final best ref; `/goal clear` keeps none).
    pub(crate) fn prune_refs(&self, keep: Option<&str>) {
        let Some(cwd) = self.ref_cwd.as_deref().map(Path::new) else { return };
        for c in self.lineage.iter().filter(|c| c.git_ref != "no-vcs" && Some(c.git_ref.as_str()) != keep) {
            git(cwd, &[], &["update-ref", "-d", &c.git_ref]);
        }
    }

    /// The final best checkpoint ref, kept when a goal completes.
    pub(crate) fn final_ref(&self) -> Option<&str> {
        self.last_good_ref().map(|c| c.git_ref.as_str())
    }

    fn last_good_ref(&self) -> Option<&Checkpoint> {
        self.lineage.iter().rev().find(|c| c.git_ref != "no-vcs")
    }

    /// Prompt block: best score, recent lineage, regression guidance. Bounded.
    pub(crate) fn ratchet_block(&self) -> String {
        if self.best.is_empty() {
            return String::new();
        }
        let mut s = format!("\nBest verified score: {}\n", fmt_score(&self.best));
        for c in self.lineage.iter().rev().take(3).rev() {
            s.push_str(&format!("- checkpoint {} {} ({})\n", c.n, c.git_ref, c.score));
        }
        if let Some(msg) = &self.regressed {
            s.push_str(&format!("[Regression] {msg}."));
            match self.last_good_ref() {
                Some(c) => s.push_str(&format!(
                    " Last good checkpoint: {}. Inspect with `git diff {0}`; restore files with `git checkout {0} -- <paths>` if that is the right call.\n",
                    c.git_ref
                )),
                None => s.push_str(" No checkpoint ref exists; undo the change that broke it.\n"),
            }
        }
        s
    }

    /// Plateau episode not yet supervised and the gap since the last call is met.
    pub fn supervisor_due(&self) -> bool {
        self.plateaued() && !self.sup_episode && self.sup_turn.is_none_or(|t| self.turns_used - t >= SUPERVISOR_GAP)
    }

    /// (system, user) for the one supervisor call: compact lineage, <= ~1.5k tokens.
    pub fn supervisor_request(&self) -> (String, String) {
        let mut u = format!("Objective: {}\nBest score: {}\nCheckpoints:\n", self.title.chars().take(300).collect::<String>(), fmt_score(&self.best));
        for c in self.lineage.iter().rev().take(6).rev() {
            u.push_str(&format!("- #{} {} | {}\n", c.n, c.score, c.line));
        }
        u.push_str("Recent attempts:\n");
        for l in &self.attempt_log {
            u.push_str(&format!("- {l}\n"));
        }
        (
            "You supervise a long-running coding agent that has stalled. Review its trajectory and reply with 2-3 DISTINCT alternative strategies, one short line each, no preamble.".into(),
            u.chars().take(4500).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stuck_git_call_is_killed_at_the_limit_and_skips_the_checkpoint() {
        let started = std::time::Instant::now();
        assert_eq!(bounded(Command::new("sleep").arg("30"), std::time::Duration::from_millis(200)), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(bounded(Command::new("echo").arg("ok"), std::time::Duration::from_secs(5)).as_deref(), Some("ok"));
    }

    /// A global attributes file naming a clean filter must not run when Akira snapshots.
    #[test]
    fn global_attribute_filters_do_not_run_during_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("ratchet-attr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        assert!(Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success());
        let (attrs, marker, config) = (dir.join("attrs"), dir.join("ran"), dir.join("gitconfig"));
        std::fs::write(&attrs, "* filter=boom\n").unwrap();
        std::fs::write(&config, format!("[core]\n\tattributesFile = {}\n[filter \"boom\"]\n\tclean = touch {} && cat\n", attrs.display(), marker.display())).unwrap();
        let run = |safe: bool, args: &[&str]| {
            let mut c = Command::new("git");
            c.current_dir(&repo).env("GIT_CONFIG_GLOBAL", &config).env("GIT_INDEX_FILE", dir.join(if safe { "i1" } else { "i2" }));
            if safe {
                c.args(SAFE_GIT);
                safe_git_env(&mut c);
            }
            c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success()
        };
        assert!(run(true, &["add", "-A"]));
        assert!(!marker.exists(), "the global filter must not run");
        assert!(run(false, &["add", "-A"]));
        assert!(marker.exists(), "control: plain git does run it, so this test can fail");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
