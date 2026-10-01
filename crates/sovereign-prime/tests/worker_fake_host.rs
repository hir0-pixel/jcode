//! Worker protocol against a scripted fake host (plain `python3`, no sandbox).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const WORKER: &str = include_str!("../src/python_worker.py");

/// Runs `code` in a fresh worker; `host` answers each call frame `(fn, args)`
/// with a reply value or error. Returns the `done` frame and the calls seen.
fn run(code: &str, host: impl Fn(&str, &[String]) -> Result<String, String>) -> Option<(Value, Vec<String>)> {
    let dir = std::env::temp_dir().join(format!("worker-fake-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).ok()?;
    let mut child = Command::new("python3")
        .args(["-I", "-S", "-u", "-c", WORKER])
        .arg(&dir)
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).ok()?;
    writeln!(stdin, "{}", json!({"op": "run", "code": code})).unwrap();
    let mut calls = Vec::new();
    loop {
        line.clear();
        out.read_line(&mut line).unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        match msg["op"].as_str() {
            Some("done") => {
                let _ = child.kill();
                return Some((msg, calls));
            }
            Some("call") => {
                let name = msg["fn"].as_str().unwrap().to_string();
                let args: Vec<String> = msg["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
                calls.push(name.clone());
                let reply = match host(&name, &args) {
                    Ok(v) => json!({"op": "reply", "value": v}),
                    Err(e) => json!({"op": "reply", "error": e}),
                };
                writeln!(stdin, "{reply}").unwrap();
            }
            _ => panic!("unexpected frame {line}"),
        }
    }
}

#[test]
fn batch_is_one_host_call_with_ordered_replies_and_error_strings() {
    let Some((done, calls)) = run(
        "r = await llm_query_batch(['a', 'bad', 'c'])\nr",
        |name, args| {
            assert_eq!(name, "llm_query_batch");
            let prompts: Vec<String> = serde_json::from_str(&args[0]).unwrap();
            Ok(json!(prompts.iter().map(|p| if p == "bad" { "Error: boom".into() } else { p.to_uppercase() }).collect::<Vec<String>>()).to_string())
        },
    ) else {
        eprintln!("skipped: no python3");
        return;
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(done["host_calls"], 1);
    assert_eq!(done["value"], "['A', 'Error: boom', 'C']");
}

#[test]
fn batch_rejects_a_bare_string() {
    let Some((done, calls)) = run("await llm_query_batch('abc')", |_, _| Ok("[]".into())) else { return };
    assert!(calls.is_empty());
    assert!(done["error"].as_str().unwrap().contains("list of strings"));
}

#[test]
fn load_reads_only_the_requested_slice() {
    let dir = std::env::temp_dir().join(format!("worker-slice-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("big.txt");
    std::fs::write(&file, "0123456789abcdef").unwrap();
    let info = json!({"path": file, "size": 16}).to_string();
    let host = move |name: &str, _: &[String]| {
        assert_eq!(name, "load_path");
        Ok(info.clone())
    };
    let Some((done, _)) = run("(await load('big.txt', 4, 6), await load('big.txt', 12), await load('big.txt', 99), len(await load('big.txt')))", host) else { return };
    assert_eq!(done["value"], "('456789', 'cdef', '', 16)");
}
