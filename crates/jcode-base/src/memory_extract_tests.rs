use super::*;
use crate::message::Message;
use std::sync::{Arc, Mutex as StdMutex};

fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
    let _guard = crate::storage::lock_test_env();
    let old = std::env::var("JCODE_HOME").ok();
    let dir = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", dir.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match old {
        Some(value) => crate::env::set_var("JCODE_HOME", value),
        None => crate::env::remove_var("JCODE_HOME"),
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn chat(pairs: usize) -> Vec<Message> {
    (0..pairs)
        .flat_map(|i| {
            [
                Message::user(&format!("Question {i}: which scripting language should the release tooling use?")),
                Message::assistant_text(&format!("Answer {i}: keep the release tooling in Nim, the user prefers it.")),
            ]
        })
        .collect()
}

fn plan_run(
    manager: &MemoryManager,
    trigger: Trigger,
    session: &str,
    messages: &[Message],
    enabled: bool,
) -> Plan {
    plan(manager, trigger, session, messages.len(), enabled, |from| messages[from..].to_vec())
}

/// A fake provider: answers with `reply` and records the system prompts it was given.
fn fake(reply: &str, seen: Arc<StdMutex<Vec<String>>>) -> impl Fn(String, String) -> BoxFuture<'static, Result<Completion>> + Send + Sync {
    let reply = reply.to_string();
    move |system, _prompt| {
        seen.lock().unwrap().push(system);
        let text = reply.clone();
        Box::pin(async move { Ok(Completion { text, input_tokens: 10, output_tokens: 5, model: "fake".into() }) })
    }
}

fn run(manager: &MemoryManager, job: Job, complete: Complete<'_>) -> Vec<String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(run_job(manager, job, complete))
}

fn job_of(plan: Plan) -> Job {
    match plan {
        Plan::Run(job) => job,
        Plan::Skip(reason) => panic!("expected a run, skipped: {reason}"),
    }
}

fn skipped(plan: Plan) -> &'static str {
    match plan {
        Plan::Skip(reason) => reason,
        Plan::Run(_) => panic!("expected a skip"),
    }
}

#[test]
fn parser_reads_pipe_lines_and_ignores_the_rest() {
    let parsed = parse_extracted(
        "Here you go:\nPreference|Prefers Nim for scripts|HIGH\nfact| Uses SQLite |medium\nno pipes here\nfact|only two\nentity||low\ncorrection|a|b|c",
    );
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0], Extracted { category: "preference".into(), content: "Prefers Nim for scripts".into(), trust: "high".into() });
    assert_eq!(parsed[1].content, "Uses SQLite");
    assert_eq!(parsed[2].content, "a");
    assert_eq!(trust_of("high"), TrustLevel::High);
    assert_eq!(trust_of("low"), TrustLevel::Low);
    assert_eq!(trust_of("medium"), TrustLevel::Medium);
    assert_eq!(trust_of("certain"), TrustLevel::Medium, "unknown trust is Medium");
    assert!(parse_extracted("").is_empty());
}

#[test]
fn transcript_strips_reminders_and_takes_oldest_first_within_cap() {
    let mut messages = vec![Message::user("<system-reminder>secret boilerplate</system-reminder>Hello there")];
    messages.push(Message::assistant_text("old reply that should fall off the cap"));
    messages.push(Message::user("newest question"));
    let (all, consumed) = build_window(&messages, usize::MAX, 10_000);
    assert!(all.contains("Hello there") && !all.contains("secret boilerplate"));
    assert_eq!(consumed, 3);
    // Oldest first: what does not fit stays unconsumed for the next window.
    let (capped, consumed) = build_window(&messages, usize::MAX, 60);
    assert!(capped.contains("Hello there") && !capped.contains("newest question"), "{capped}");
    assert_eq!(consumed, 1, "only the messages actually included are consumed");
    let (one, consumed) = build_window(&messages, 1, 10_000);
    assert!(one.contains("Hello there") && consumed == 1);
    assert_eq!(strip_system_reminders("a<system-reminder>x</system-reminder>b<system-reminder>open"), "ab");
}

#[test]
fn floors_gate_short_conversations() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let three = chat(2)[..3].to_vec();
        assert_eq!(skipped(plan_run(&manager, Trigger::SessionEnd, "floor-a", &three, true)), "under_floor");
        let tiny: Vec<Message> = (0..6).map(|_| Message::user("hi")).collect();
        assert_eq!(skipped(plan_run(&manager, Trigger::SessionEnd, "floor-b", &tiny, true)), "under_floor");
        assert_eq!(skipped(plan_run(&manager, Trigger::SessionEnd, "floor-c", &chat(3), false)), "sidecar_off");
        assert_eq!(skipped(plan_run(&manager, Trigger::SessionEnd, "floor-d", &[], true)), "no_new_messages");
        assert!(matches!(plan_run(&manager, Trigger::SessionEnd, "floor-e", &chat(3), true), Plan::Run(_)));
    });
}

#[test]
fn extraction_stores_dedupes_and_marks_the_marker() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let messages = chat(3);
        let job = job_of(plan_run(&manager, Trigger::Compaction, "ex-1", &messages, true));
        let ids = run(&manager, job, &fake("preference|Prefers Nim for the release scripting|high\nfact|Release tooling lives in the tools directory|medium", seen.clone()));
        assert_eq!(ids.len(), 2);
        assert_eq!(manager.list_all().unwrap().len(), 2);
        assert!(!seen.lock().unwrap()[0].contains("Already known (do NOT"), "empty store, no known list");

        // A later run of the same session re-extracts a paraphrase: it merges, no second row.
        forget_session("ex-1");
        let mut more = messages.clone();
        more.extend(chat(3));
        let job = job_of(plan_run(&manager, Trigger::Compaction, "ex-1", &more, true));
        assert_eq!(job.upto, 12);
        let ids2 = run(&manager, job, &fake("preference|The user prefers Nim for release scripting|high", seen.clone()));
        assert_eq!(ids2.len(), 1);
        let active: Vec<_> = manager.list_all().unwrap().into_iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 2, "paraphrase did not create a second active row");
        let prompt = seen.lock().unwrap()[1].clone();
        assert!(prompt.contains("Already known (do NOT") && prompt.contains("Prefers Nim"), "{prompt}");
        assert!(active.iter().all(|e| e.source.as_deref() == Some("ex-1")));

        // The marker moved: the same history yields nothing new, a longer one only the tail.
        forget_session("ex-1");
        assert_eq!(skipped(plan_run(&manager, Trigger::SessionEnd, "ex-1", &more, true)), "no_new_messages");
        let mut longer = more.clone();
        longer.extend(chat(3));
        let job = job_of(plan_run(&manager, Trigger::SessionEnd, "ex-1", &longer, true));
        assert_eq!(job.upto, 18);
        assert!(!job.transcript.is_empty());
    });
}

#[test]
fn failure_leaves_the_marker_so_the_window_is_retried() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let messages = chat(3);
        let job = job_of(plan_run(&manager, Trigger::SessionEnd, "fail-1", &messages, true));
        let failing = |_s: String, _p: String| -> BoxFuture<'static, Result<Completion>> { Box::pin(async { Err(anyhow!("provider down")) }) };
        assert!(run(&manager, job, &failing).is_empty());
        forget_session("fail-1");
        assert!(matches!(plan_run(&manager, Trigger::SessionEnd, "fail-1", &messages, true), Plan::Run(_)));
    });
}

#[test]
fn cooldown_spaces_periodic_runs_but_never_blocks_session_end_or_compaction() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let messages = chat(3);
        assert!(matches!(plan_run(&manager, Trigger::Periodic, "cool-1", &messages, true), Plan::Run(_)));
        assert_eq!(skipped(plan_run(&manager, Trigger::Periodic, "cool-1", &messages, true)), "in_flight");
        release("cool-1");
        assert_eq!(skipped(plan_run(&manager, Trigger::Periodic, "cool-1", &messages, true)), "cooldown");
        assert!(matches!(plan_run(&manager, Trigger::SessionEnd, "cool-1", &messages, true), Plan::Run(_)), "session end is not cooled");
        assert!(matches!(plan_run(&manager, Trigger::Compaction, "cool-1", &messages, true), Plan::Run(_)));
        forget_session("cool-1");
        assert!(matches!(plan_run(&manager, Trigger::Periodic, "cool-1", &messages, true), Plan::Run(_)));
    });
}

#[test]
fn every_twelfth_user_turn_is_periodic_and_state_is_pruned() {
    let hits: Vec<usize> = (1..=24).filter(|_| note_user_turn("turns-1")).collect();
    assert_eq!(hits.len(), 2);
    forget_session("turns-1");
    assert!(!note_user_turn("turns-1"), "counter restarted after close");
    forget_session("turns-1");
    assert!(SESSIONS.lock().unwrap().get("turns-1").is_none());
}

#[test]
fn periodic_run_takes_at_most_forty_messages_and_capped_chars() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let messages = chat(60);
        let job = job_of(plan_run(&manager, Trigger::Periodic, "cap-1", &messages, true));
        assert_eq!(job.transcript.matches("**User:**").count(), 20);
        assert!(job.transcript.contains("Question 0:") && !job.transcript.contains("Question 59:"), "oldest window first");
        assert_eq!(job.upto, 40, "only the 40 messages included are marked");
        let big = vec![Message::user(&"x".repeat(30_000)); 5];
        let job = job_of(plan_run(&manager, Trigger::SessionEnd, "cap-2", &big, true));
        assert!(job.transcript.chars().count() <= MAX_TRANSCRIPT_CHARS);
    });
}

#[test]
fn extraction_without_working_dir_writes_global() {
    with_temp_home(|| {
        let manager = MemoryManager::new();
        assert!(manager.remember_extracted(MemoryEntry::new(MemoryCategory::Fact, "A global fact about builds")).is_ok());
        assert_eq!(manager.list_all().unwrap().len(), 1);
    });
}

#[test]
fn detailed_recall_counts_candidates_and_suppressed() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        for text in ["Release tooling is written in Nim", "Release notes go in the changelog file", "Unrelated fact about gardening"] {
            manager.remember_project(MemoryEntry::new(MemoryCategory::Fact, text)).unwrap();
        }
        let recall = manager
            .recall_local_detailed(Some("recall-1"), "where does the release tooling live", 5, crate::memory::MemoryScope::All)
            .unwrap();
        assert!(recall.candidates >= recall.entries.len());
        assert_eq!(recall.suppressed, recall.candidates - recall.entries.len());
        assert!(recall.terms.contains(&"release".to_string()));
        let first = recall.entries[0].id.clone();
        crate::memory::mark_memories_known("recall-1", &[first], "test");
        let again = manager.recall_local_detailed(Some("recall-1"), "where does the release tooling live", 5, crate::memory::MemoryScope::All).unwrap();
        assert!(again.suppressed >= 1);
        crate::memory::clear_injected_memories("recall-1");
    });
}

#[test]
fn oversized_backlog_is_consumed_across_windows_without_skipping() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let big: Vec<Message> = (0..6).map(|i| Message::user(&format!("m{i} {}", "x".repeat(10_000)))).collect();
        let job = job_of(plan_run(&manager, Trigger::Compaction, "win-1", &big, true));
        assert_eq!(job.upto, 2, "24k cap fits two 10k messages");
        assert!(job.transcript.contains("m0 ") && job.transcript.contains("m1 ") && !job.transcript.contains("m2 "));
        run(&manager, job, &fake("", seen.clone()));
        assert_eq!(seen.lock().unwrap().len(), 3, "compaction drains the whole range window by window");
        assert_eq!(read_marker(&manager, "win-1"), 6);
    });
}

#[test]
fn periodic_leaves_the_rest_for_the_next_run() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let messages = chat(60);
        let job = job_of(plan_run(&manager, Trigger::Periodic, "per-1", &messages, true));
        run(&manager, job, &fake("", seen.clone()));
        assert_eq!(read_marker(&manager, "per-1"), 40);
        forget_session("per-1");
        let job = job_of(plan_run(&manager, Trigger::Periodic, "per-1", &messages, true));
        assert_eq!(job.from, 40);
        assert!(job.transcript.contains("Question 20:"));
    });
}

#[test]
fn session_end_after_periodic_does_not_repeat_or_drop() {
    with_temp_home(|| {
        let manager = MemoryManager::new_test();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let messages = chat(60);
        let periodic = job_of(plan_run(&manager, Trigger::Periodic, "end-1", &messages, true));
        // SessionEnd is planned while the periodic job is still in flight, from the same marker.
        let end = job_of(plan_run(&manager, Trigger::SessionEnd, "end-1", &messages, true));
        assert_eq!(skipped(plan_run(&manager, Trigger::Periodic, "end-1", &messages, true)), "in_flight");
        run(&manager, periodic, &fake("", seen.clone()));
        run(&manager, end, &fake("", seen.clone()));
        assert_eq!(seen.lock().unwrap().len(), 2, "periodic window, then only the tail: no repeated window");
        assert_eq!(read_marker(&manager, "end-1"), 120);
    });
}

#[test]
fn a_session_that_pre_dates_extraction_is_stamped_and_only_new_messages_are_extracted() {
    with_temp_home(|| {
        let manager = MemoryManager::new();
        let old = chat(40);
        adopt_session("legacy-1", old.len());
        assert_eq!(extracted_through(&manager, "legacy-1", old.len()), old.len());
        assert!(matches!(plan_run(&manager, Trigger::SessionEnd, "legacy-1", &old, true), Plan::Skip("no_new_messages")));
        let mut grown = old.clone();
        grown.extend(chat(3));
        let job = job_of(plan_run(&manager, Trigger::SessionEnd, "legacy-1", &grown, true));
        assert_eq!(job.from, old.len());
        // Adopting again, or after real activity, never moves a marker.
        adopt_session("legacy-1", grown.len());
        assert_eq!(extracted_through(&manager, "legacy-1", grown.len()), old.len());
        forget_session("legacy-1");
    });
}
