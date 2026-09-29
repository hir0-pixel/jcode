//! `process.*`, `agents.list` and `rollback.*`, answered from the engine's own state
//! (jcode's background-task registry, child sessions, goal-ratchet checkpoint refs)
//! so the desktop's status stack never wakes the Python backend.

use super::*;
use jcode_base::background::BackgroundTaskManager;
use jcode_base::bus::BackgroundTaskStatus;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "process.list" | "process.kill" | "process.stop" | "agents.list" | "rollback.list" | "rollback.restore" | "rollback.diff"
    )
}

fn command_of(t: &jcode_base::background::TaskStatusFile) -> String {
    t.display_name.clone().unwrap_or_else(|| t.tool_name.clone())
}

fn uptime(started_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(started_at).map_or(0, |s| (chrono::Utc::now() - s.to_utc()).num_seconds().max(0))
}

async fn running(mgr: &BackgroundTaskManager, session: Option<&str>) -> Vec<jcode_base::background::TaskStatusFile> {
    let mut tasks = mgr.list().await;
    tasks.retain(|t| t.status == BackgroundTaskStatus::Running && session.is_none_or(|s| t.session_id == s));
    tasks
}

pub(super) async fn process_list(mgr: &BackgroundTaskManager, session: &str) -> Value {
    let rows: Vec<Value> = running(mgr, Some(session)).await.iter().map(|t| json!({
        "session_id": t.task_id, "command": command_of(t), "pid": t.pid, "started_at": t.started_at,
        "uptime_seconds": uptime(&t.started_at), "status": "running", "output_preview": "",
        "session_scoped": true, "detached": t.detached, "notify_on_complete": t.notify,
    })).collect();
    json!({ "processes": rows })
}

pub(super) async fn process_kill(mgr: &BackgroundTaskManager, session: &str, id: &str) -> Result<Value, RpcError> {
    if id.is_empty() {
        return Err(RpcError { code: 4012, message: "process_id required".into(), data: None });
    }
    let Some(task) = mgr.status(id).await.filter(|t| t.session_id == session) else {
        return Err(RpcError { code: 4044, message: format!("no such process: {id}"), data: None });
    };
    let status = if task.status != BackgroundTaskStatus::Running {
        "already_exited"
    } else if mgr.cancel(id).await.map_err(RpcError::internal)? {
        "killed"
    } else {
        "error"
    };
    Ok(json!({ "status": status, "session_id": id, "command": command_of(&task), "exit_code": task.exit_code }))
}

pub(super) async fn process_stop(mgr: &BackgroundTaskManager) -> Value {
    let mut killed = 0;
    for t in running(mgr, None).await {
        killed += usize::from(mgr.cancel(&t.task_id).await.unwrap_or(false));
    }
    json!({ "killed": killed })
}

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    git_capped(cwd, args, usize::MAX)
}

/// Run git with the repo's own config unable to execute anything, reading at most `max` bytes of
/// output (a huge diff is cut off, not read whole).
fn git_capped(cwd: &str, args: &[&str], max: usize) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new("git")
        .current_dir(cwd)
        .args(sovereign_prime::goal_ratchet::SAFE_GIT)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut buf = Vec::new();
    child.stdout.take()?.take(max.min(u64::MAX as usize) as u64).read_to_end(&mut buf).ok()?;
    let cut = buf.len() >= max;
    if cut {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    (cut || status.success()).then(|| String::from_utf8_lossy(&buf).trim_end().to_string())
}

/// This session's goal-ratchet checkpoints, newest first: `(hash, iso time, message)`.
fn checkpoints(cwd: &str, session: &str) -> Option<Vec<(String, String, String)>> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    let safe: String = session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let listing = git(cwd, &["for-each-ref", "--sort=-creatordate", "--format=%(objectname)\t%(creatordate:iso-strict)\t%(refname)", &format!("refs/akira/goals/{safe}/")])?;
    Some(listing.lines().filter_map(|l| {
        let mut f = l.splitn(3, '\t');
        let (hash, at, name) = (f.next()?, f.next()?, f.next()?);
        Some((hash.into(), at.into(), format!("goal checkpoint {}", name.rsplit('/').next().unwrap_or(name))))
    }).collect())
}

/// A full hash, a hash prefix, or a 1-based index into the list.
fn resolve(list: &[(String, String, String)], target: &str) -> Option<String> {
    let by_index = target.parse::<usize>().ok().and_then(|n| list.get(n.checked_sub(1)?));
    by_index.or_else(|| list.iter().find(|(h, _, _)| h.starts_with(target))).map(|c| c.0.clone())
}

pub(super) fn rollback(method: &str, cwd: &str, session: &str, p: &Value) -> Result<Value, RpcError> {
    let err = |code, message: &str| RpcError { code, message: message.into(), data: None };
    let Some(list) = checkpoints(cwd, session) else {
        return match method {
            "rollback.list" => Ok(json!({ "enabled": false, "checkpoints": [] })),
            "rollback.restore" => Ok(json!({ "success": false, "error": "no checkpoints: this folder is not a git repository" })),
            _ => Err(err(5022, "no checkpoints: this folder is not a git repository")),
        };
    };
    if method == "rollback.list" {
        let rows: Vec<Value> = list.iter().map(|(hash, timestamp, message)| json!({ "hash": hash, "timestamp": timestamp, "message": message })).collect();
        return Ok(json!({ "enabled": true, "checkpoints": rows }));
    }
    let target = p["hash"].as_str().unwrap_or_default();
    if target.is_empty() {
        return Err(err(4014, "hash required"));
    }
    let Some(hash) = resolve(&list, target) else {
        return if method == "rollback.diff" { Err(err(5022, "unknown checkpoint")) } else { Ok(json!({ "success": false, "error": "unknown checkpoint" })) };
    };
    if method == "rollback.diff" {
        let stat = git_capped(cwd, &["diff", "--no-ext-diff", "--no-textconv", "--stat", &hash], 16_000).unwrap_or_default();
        let diff: String = git_capped(cwd, &["diff", "--no-ext-diff", "--no-textconv", &hash], 16_000).unwrap_or_default().chars().take(4000).collect();
        return Ok(json!({ "stat": stat, "diff": diff }));
    }
    let file = p["file_path"].as_str().filter(|f| !f.is_empty());
    let source = format!("--source={hash}");
    let mut args = vec!["restore", source.as_str(), "--worktree", "--"];
    args.push(file.unwrap_or("."));
    Ok(match git(cwd, &args) {
        Some(_) => json!({ "success": true, "restored_to": hash, "directory": cwd, "file": file, "history_removed": 0 }),
        None => json!({ "success": false, "error": "git could not restore that checkpoint" }),
    })
}

impl Conn {
    pub(super) async fn local_state(self: &Arc<Self>, method: &str, p: &Value) -> Result<Value, RpcError> {
        let session = p["session_id"].as_str().unwrap_or_default();
        let mgr = jcode_base::background::global();
        match method {
            "process.list" => Ok(process_list(mgr, session).await),
            "process.kill" => process_kill(mgr, session, p["process_id"].as_str().unwrap_or_default()).await,
            "process.stop" => Ok(process_stop(mgr).await),
            "agents.list" => {
                let mut rows: Vec<Value> = running(mgr, None).await.iter().map(|t| json!({
                    "session_id": t.task_id, "command": command_of(t).chars().take(80).collect::<String>(),
                    "status": "running", "uptime": uptime(&t.started_at),
                })).collect();
                if !session.is_empty() {
                    let reply = self.call(json!({ "req": "list_sessions" })).await.map_err(RpcError::internal)?;
                    let live = self.sessions.lock().await;
                    for child in reply["sessions"].as_array().into_iter().flatten().filter(|s| s["parent_session_id"].as_str() == Some(session)) {
                        let status = Self::map_subagent_status(child, &live);
                        let name = child["agent_label"].as_str().or(child["title"].as_str()).unwrap_or("subagent");
                        rows.push(json!({ "session_id": child["session_id"], "command": name.chars().take(80).collect::<String>(), "status": status, "uptime": 0 }));
                    }
                }
                Ok(json!({ "processes": rows }))
            }
            _ => {
                let cwd = match self.session_cwd(session).await {
                    Some(cwd) => cwd,
                    None => self.config.default_cwd.clone(),
                };
                let (method, session, p) = (method.to_string(), session.to_string(), p.clone());
                tokio::task::spawn_blocking(move || rollback(&method, &cwd, &session, &p))
                    .await
                    .map_err(|e| RpcError::internal(anyhow!(e)))?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_base::background::TaskStatusFile;

    fn task(id: &str, session: &str, status: &str) -> TaskStatusFile {
        serde_json::from_value(json!({
            "task_id": id, "tool_name": "bash", "display_name": "npm run dev", "session_id": session, "status": status,
            "exit_code": null, "error": null, "started_at": chrono::Utc::now().to_rfc3339(), "completed_at": null, "duration_secs": null,
        })).unwrap()
    }

    #[tokio::test]
    async fn process_calls_answer_from_the_engine_registry_and_are_scoped_to_the_session() {
        let dir = std::env::temp_dir().join(format!("local-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mgr = BackgroundTaskManager::with_output_dir(dir.clone());
        for t in [task("a1", "s1", "running"), task("b2", "s2", "running"), task("c3", "s1", "completed")] {
            std::fs::write(dir.join(format!("{}.status.json", t.task_id)), serde_json::to_string(&t).unwrap()).unwrap();
        }
        let listed = process_list(&mgr, "s1").await;
        assert_eq!(listed["processes"].as_array().unwrap().len(), 1);
        assert_eq!((listed["processes"][0]["session_id"].as_str(), listed["processes"][0]["command"].as_str()), (Some("a1"), Some("npm run dev")));
        assert_eq!(process_list(&mgr, "nobody").await, json!({ "processes": [] }));
        assert_eq!(process_kill(&mgr, "s1", "b2").await.unwrap_err().code, 4044, "another session's process");
        assert_eq!(process_kill(&mgr, "s1", "c3").await.unwrap()["status"], "already_exited");
        assert_eq!(process_stop(&BackgroundTaskManager::with_output_dir(dir.join("empty"))).await, json!({ "killed": 0 }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rollback_lists_goal_checkpoints_and_answers_empty_outside_a_repo() {
        let plain = std::env::temp_dir().join(format!("rollback-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        let none = rollback("rollback.list", plain.to_str().unwrap(), "s1", &json!({})).unwrap();
        // /tmp may itself sit inside a repo on odd setups; either way the shape holds.
        assert!(none["checkpoints"].as_array().unwrap().is_empty());

        let repo = std::env::temp_dir().join(format!("rollback-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let cwd = repo.to_str().unwrap();
        git(cwd, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("f.txt"), "one").unwrap();
        let name = sovereign_prime::goal_ratchet::snapshot(&repo, "s1", 1).unwrap();
        std::fs::write(repo.join("f.txt"), "two").unwrap();

        let list = rollback("rollback.list", cwd, "s1", &json!({})).unwrap();
        assert_eq!((list["enabled"].clone(), list["checkpoints"].as_array().unwrap().len()), (json!(true), 1), "{name}");
        assert_eq!(rollback("rollback.list", cwd, "other", &json!({})).unwrap()["checkpoints"], json!([]));
        assert!(rollback("rollback.diff", cwd, "s1", &json!({ "hash": "1" })).unwrap()["diff"].as_str().unwrap().contains("-one"));
        assert_eq!(rollback("rollback.restore", cwd, "s1", &json!({})).unwrap_err().code, 4014);
        assert_eq!(rollback("rollback.restore", cwd, "s1", &json!({ "hash": "1" })).unwrap()["success"], true);
        assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "one");
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(plain);
    }

    #[test]
    fn repo_config_cannot_run_code_through_snapshot_diff_or_restore() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("rollback-fsmon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let (marker, script) = (root.join("ran"), root.join("fsmon.sh"));
        std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let raw = |args: &[&str]| std::process::Command::new("git").current_dir(&repo).args(args).output().unwrap();
        raw(&["init", "-q"]);
        raw(&["config", "core.fsmonitor", script.to_str().unwrap()]);
        std::fs::write(repo.join("f.txt"), "one").unwrap();
        raw(&["status"]);
        assert!(marker.exists(), "sanity: unprotected git runs the configured script");
        std::fs::remove_file(&marker).unwrap();

        let cwd = repo.to_str().unwrap();
        sovereign_prime::goal_ratchet::snapshot(&repo, "s1", 1).unwrap();
        std::fs::write(repo.join("f.txt"), "two").unwrap();
        rollback("rollback.list", cwd, "s1", &json!({})).unwrap();
        assert!(rollback("rollback.diff", cwd, "s1", &json!({ "hash": "1" })).unwrap()["diff"].as_str().unwrap().contains("-one"));
        rollback("rollback.restore", cwd, "s1", &json!({ "hash": "1" })).unwrap();
        assert!(!marker.exists(), "repo-controlled fsmonitor must not run");
        let _ = std::fs::remove_dir_all(root);
    }
}
