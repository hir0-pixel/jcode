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

fn git(cwd: &Path, envs: &[(&str, &Path)], args: &[&str]) -> Option<String> {
    let mut c = Command::new("git");
    c.current_dir(cwd).args(args).stdin(Stdio::null()).stderr(Stdio::null());
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
