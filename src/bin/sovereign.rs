//! `sovereign`: the engine binary with the Hermes backend command line, so the
//! Hermes desktop can launch it exactly as it launches `hermes serve`:
//!
//!   sovereign [--profile P] serve --host H --port N   →  jcode gateway --host H --port N
//!   sovereign [--profile P] dashboard --no-open ...   →  same (legacy spelling)
//!
//! Anything else passes through unchanged, so `sovereign login` etc. still work.

use anyhow::Result;

// Counts allocations so a REPL worker (this binary with `__repl-worker`) can
// enforce a hard memory ceiling. The engine process never arms a limit, so
// for it this is the system allocator plus one counter.
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

/// Tools off by default in the sovereign engine: remote services
/// (integration discovery, Gmail relay, maintainer feedback, remote compile),
/// full computer control (opt in explicitly), terminal-UI panels and
/// schedules the desktop does not render, jcode's own docs, and the swarm
/// (Prime's `repl` + `llm_query` covers focused sub-questions far cheaper).
const DEFAULT_DISABLED_TOOLS: &str = "integration_tools,gmail,maintainer_feedback,compile_remote,macos_computer_use,panel,side_panel,schedule,jcode_docs,swarm";

fn translate(args: Vec<String>) -> Vec<String> {
    let mut out = vec!["sovereign".to_string()];
    let mut rest = args.into_iter().peekable();
    let mut profile_skipped = false;
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            // Profiles are a Hermes concept the engine does not have yet.
            "--profile" if !profile_skipped => {
                rest.next();
                profile_skipped = true;
            }
            "serve" => out.push("gateway".into()),
            "dashboard" => {
                out.push("gateway".into());
                if rest.peek().map(String::as_str) == Some("--no-open") {
                    rest.next();
                }
            }
            _ => out.push(arg),
        }
    }
    out
}

fn main() -> Result<()> {
    // Prime REPL worker mode: no runtime, no jcode startup, no environment.
    if std::env::args().nth(1).as_deref() == Some("__repl-worker") {
        return Ok(sovereign_prime::worker::run(sovereign_prime::worker::Limits::default())?);
    }
    // pre_tool gate (spawned by jcode before each tool call): ask a human
    // before risky shell commands. Exit 0 allows, 2 blocks.
    if std::env::args().nth(1).as_deref() == Some("__pre-tool") {
        let file = jcode::storage::jcode_dir().map(|d| d.join("sovereign-approval.json")).unwrap_or_default();
        std::process::exit(sovereign_gateway::approvals::hook::run(&file));
    }
    // Keep the desktop's session token out of the environment that tool
    // subprocesses (including the model's shell commands) inherit.
    if let Ok(token) = std::env::var("HERMES_DASHBOARD_SESSION_TOKEN") {
        // SAFETY: single-threaded here, before the runtime starts.
        unsafe { std::env::remove_var("HERMES_DASHBOARD_SESSION_TOKEN") };
        sovereign_gateway::auth::set_launch_token(token);
    }
    if let Ok(exe) = std::env::current_exe() {
        // SAFETY: single-threaded here, before the runtime starts.
        unsafe { std::env::set_var("SOVEREIGN_REPL_WORKER", &exe) };
        // Human approval gate for risky shell commands, unless the user
        // configured their own pre_tool hook. The long timeout keeps jcode
        // from failing open while a person decides; the hook gives up (deny)
        // first.
        if std::env::var_os("JCODE_HOOK_PRE_TOOL").is_none() {
            let hook = format!("\"{}\" __pre-tool", exe.display());
            // SAFETY: as above.
            unsafe {
                std::env::set_var("JCODE_HOOK_PRE_TOOL", hook);
                std::env::set_var("JCODE_HOOK_PRE_TOOL_TIMEOUT_MS", "600000");
            }
        }
    }
    // Sovereign: nothing leaves the machine except model calls. jcode's
    // anonymous usage telemetry is disabled unconditionally.
    // SAFETY: single-threaded here, before the runtime starts.
    unsafe { std::env::set_var("JCODE_NO_TELEMETRY", "1") };
    // Memory recall is local; remote Jev relevance calls are disabled.
    // SAFETY: as above.
    unsafe { std::env::set_var("SOVEREIGN_LOCAL_MEMORY", "1") };
    // Hermes does not stamp user messages with times; jcode's stamps cost
    // tokens and read to models like injected text.
    if std::env::var_os("JCODE_MESSAGE_TIMESTAMPS").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("JCODE_MESSAGE_TIMESTAMPS", "0") };
    }
    // No integration discovery (it contacts a remote endpoint).
    // SAFETY: as above.
    unsafe { std::env::set_var("JCODE_SPONSORS_ENABLED", "0") };
    // Token budget: tool definitions are ~97% of every request. Drop tools
    // that reach third parties or that the desktop cannot render. Override
    // with JCODE_DISABLED_TOOLS (set it to empty to keep everything).
    if std::env::var_os("JCODE_DISABLED_TOOLS").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("JCODE_DISABLED_TOOLS", DEFAULT_DISABLED_TOOLS) };
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let argv = translate(std::env::args().skip(1).collect());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(jcode::cli::startup::run_from(argv))
}

#[cfg(test)]
mod tests {
    use super::translate;

    fn t(args: &[&str]) -> Vec<String> {
        translate(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn hermes_serve_becomes_gateway() {
        assert_eq!(
            t(&["--profile", "work", "serve", "--host", "127.0.0.1", "--port", "0"]),
            ["sovereign", "gateway", "--host", "127.0.0.1", "--port", "0"]
        );
        assert_eq!(t(&["dashboard", "--no-open", "--port", "0"]), ["sovereign", "gateway", "--port", "0"]);
    }

    #[test]
    fn other_commands_pass_through() {
        assert_eq!(t(&["login", "openai"]), ["sovereign", "login", "openai"]);
    }
}
