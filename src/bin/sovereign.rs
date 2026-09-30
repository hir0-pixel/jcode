//! `sovereign`: the headless engine binary launched by the Hermes desktop:
//!
//!   sovereign [--profile P] serve --host H --port N
//!   sovereign __pre-tool
//!   sovereign __version      (JSON {version, sha, db_schema}; read by the desktop packager)

use anyhow::Result;
use std::path::{Path, PathBuf};

struct GatewayArgs {
    provider: jcode::sovereign_runtime::ProviderChoice,
    model: Option<String>,
    host: String,
    port: u16,
    allow_remote: bool,
}

fn profile_home(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("invalid Hermes profile name: {name}");
    }
    let root = if root
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|p| p == "profiles")
    {
        root.parent().and_then(Path::parent).unwrap_or(root)
    } else {
        root
    };
    Ok(if name == "default" {
        root.to_path_buf()
    } else {
        root.join("profiles").join(name)
    })
}

/// Standalone (no desktop): cron, key ownership, the unattended policy and messaging all read
/// `HERMES_HOME`, so export the root every Hermes tool would resolve rather than silently doing nothing.
fn export_default_hermes_home() {
    if std::env::var_os("HERMES_HOME").is_none()
        && let Some(root) = hermes_root()
    {
        // SAFETY: called before the Tokio runtime or any child processes start.
        unsafe { std::env::set_var("HERMES_HOME", root) };
    }
}

fn hermes_root() -> Option<PathBuf> {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes")))
}

/// Hermes's rule with no `--profile` flag (`_apply_profile_override`): the sticky `active_profile`
/// file of the Hermes root names the profile, unless `HERMES_HOME` already points into `profiles/`.
/// A name that is not a profile directory here is ignored rather than failing the engine.
fn sticky_profile(root: &Path) -> Option<String> {
    if root.parent().and_then(Path::file_name).is_some_and(|p| p == "profiles") {
        return None;
    }
    let name = std::fs::read_to_string(root.join("active_profile")).ok()?.trim().to_string();
    (name != "default" && profile_home(root, &name).is_ok_and(|home| home.is_dir())).then_some(name)
}

/// Scope this process to a Hermes profile: `HERMES_HOME` is the one place approvals, `.env`, cron
/// and defaults are read from.
fn activate_profile(root: &Path, name: &str) -> anyhow::Result<()> {
    let selected = profile_home(root, name)?;
    if !selected.is_dir() {
        anyhow::bail!("Hermes profile does not exist: {}", selected.display());
    }
    // SAFETY: called before the Tokio runtime or any child processes start.
    unsafe { std::env::set_var("HERMES_HOME", selected) };
    // SAFETY: still before runtime startup and child process creation.
    unsafe { std::env::set_var("SOVEREIGN_PROFILE", name) };
    Ok(())
}

fn select_profile(args: &[String]) -> anyhow::Result<Option<String>> {
    let mut i = 0;
    while i < args.len() {
        let name = match args[i].as_str() {
            "--profile" => {
                i += 1;
                Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--profile requires a name"))?
                        .clone(),
                )
            }
            arg => arg.strip_prefix("--profile=").map(str::to_owned),
        };
        if let Some(name) = name {
            let root = hermes_root().ok_or_else(|| anyhow::anyhow!("cannot resolve Hermes home for --profile"))?;
            activate_profile(&root, &name)?;
            return Ok(Some(name));
        }
        i += 1;
    }
    if let Some(root) = hermes_root()
        && let Some(name) = sticky_profile(&root)
    {
        activate_profile(&root, &name)?;
        return Ok(Some(name));
    }
    export_default_hermes_home();
    Ok(None)
}

fn parse_gateway_args(args: &[String]) -> anyhow::Result<GatewayArgs> {
    use clap::ValueEnum;
    let mut provider = jcode::sovereign_runtime::ProviderChoice::YoloAuto;
    let mut model = None;
    let mut host = "127.0.0.1".to_string();
    let mut port = 8000;
    let mut allow_remote = false;
    let mut i = 0;
    while i < args.len() {
        let value = args[i].as_str();
        let take = |i: &mut usize, option: &str| -> anyhow::Result<&str> {
            *i += 1;
            args.get(*i)
                .map(String::as_str)
                .ok_or_else(|| anyhow::anyhow!("{option} requires a value"))
        };
        match value {
            "serve" | "gateway" | "--profile" => {
                if value == "--profile" {
                    i += 1;
                }
            }
            v if v.starts_with("--profile=") => {}
            "--token-stdin" => {}
            "--provider" | "-p" => {
                provider = ValueEnum::from_str(take(&mut i, value)?, true)
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
            "--model" | "-m" => model = Some(take(&mut i, value)?.to_string()),
            "--host" => host = take(&mut i, value)?.to_string(),
            "--port" => port = take(&mut i, value)?.parse()?,
            "--allow-remote" => allow_remote = true,
            other => anyhow::bail!("unsupported sovereign argument: {other}"),
        }
        i += 1;
    }
    Ok(GatewayArgs {
        provider,
        model,
        host,
        port,
        allow_remote,
    })
}

/// `--token-stdin`: the first stdin line is the token (an empty one is an error, not a silent fallback
/// to a generated token the desktop does not know). Otherwise the dev env var, if any.
fn launch_token(
    args: &[String],
    mut stdin: impl std::io::BufRead,
    env_token: Option<String>,
) -> Result<Option<String>> {
    if !args.iter().any(|a| a == "--token-stdin") {
        return Ok(env_token);
    }
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let token = line.trim();
    anyhow::ensure!(!token.is_empty(), "--token-stdin: no token on stdin");
    Ok(Some(token.to_owned()))
}

fn main() -> Result<()> {
    // pre_tool gate (spawned by jcode before each tool call): ask a human
    // before risky shell commands. Exit 0 allows, 2 blocks.
    if std::env::args().nth(1).as_deref() == Some("__version") {
        println!("{}", serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "sha": sovereign_gateway::build_sha(), "db_schema": sovereign_gateway::db_schema() }));
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("__pre-tool") {
        std::process::exit(sovereign_gateway::approvals::hook::run());
    }

    // Always unload a warmed Ollama alias on process exit (including SIGTERM),
    // so keep_alive=-1 never leaves the model resident after the desktop closes.
    let _ollama_guard = OllamaUnloadOnDrop;
    install_ollama_signal_unload();

    #[cfg(unix)]
    if let Some(parent) = std::env::var("HERMES_PARENT_PID")
        .ok()
        .and_then(|pid| pid.parse::<libc::pid_t>().ok())
    {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                // A direct Electron child is reparented when the app is force-killed.
                if unsafe { libc::getppid() } != parent {
                    unload_ollama_from_warm_file();
                    std::process::exit(0);
                }
            }
        });
    }
    // The desktop's session token arrives on stdin (`--token-stdin`), so it is never in this process's
    // environment or argv, both of which `ps` shows to the model's shell commands. The env var is the
    // dev path (running `sovereign serve` by hand): read, then removed for tool subprocesses.
    let argv: Vec<String> = std::env::args().collect();
    if let Some(token) = launch_token(&argv, std::io::stdin().lock(), std::env::var("HERMES_DASHBOARD_SESSION_TOKEN").ok())? {
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
    // SAFETY: single-threaded here, before the runtime starts.
    // Hermes does not stamp user messages with times; jcode's stamps cost
    // tokens and read to models like injected text.
    if std::env::var_os("JCODE_MESSAGE_TIMESTAMPS").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("JCODE_MESSAGE_TIMESTAMPS", "0") };
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    select_profile(&args)?;
    let args = parse_gateway_args(&args)?;
    // SAFETY: startup is single-threaded until the runtime is built.
    unsafe { std::env::set_var("JCODE_NON_INTERACTIVE", "1") };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(jcode::sovereign_runtime::run_gateway(
            &args.provider,
            args.model.as_deref(),
            &args.host,
            args.port,
            args.allow_remote,
        ))
}

/// Best-effort Ollama unload when the process exits for any reason other than
/// `std::process::exit` / abort. Complements the async unload in `run_gateway`.
struct OllamaUnloadOnDrop;

impl Drop for OllamaUnloadOnDrop {
    fn drop(&mut self) {
        unload_ollama_from_warm_file();
    }
}

fn unload_ollama_from_warm_file() {
    let Ok(home) = jcode::storage::jcode_dir() else {
        return;
    };
    let path = home.join("sovereign-ollama-warm.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(model) = meta.get("model").and_then(|v| v.as_str()) else {
        return;
    };
    // Only the process that warmed the model releases it: any other `sovereign` run (a CLI call,
    // a second serve, a bad argument) would otherwise evict a model someone else is serving.
    if meta.get("pid").and_then(|v| v.as_u64()) != Some(u64::from(std::process::id())) {
        return;
    }
    // A keep_alive=0 request for a model that is not resident makes Ollama load it first.
    if !ollama_has_loaded(model) {
        let _ = std::fs::remove_file(path);
        return;
    }
    let body = serde_json::json!({ "model": model, "keep_alive": 0 }).to_string();
    let _ = std::process::Command::new("curl")
        .args([
            "-s",
            "-m",
            "5",
            "http://127.0.0.1:11434/api/generate",
            "-d",
            &body,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_file(path);
    eprintln!("sovereign: unloaded Ollama {model}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

fn ollama_has_loaded(model: &str) -> bool {
    let Ok(out) = std::process::Command::new("curl")
        .args(["-s", "-m", "3", "http://127.0.0.1:11434/api/ps"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v["models"].as_array().cloned())
        .is_some_and(|models| models.iter().any(|m| m["name"] == model || m["model"] == model))
}

fn install_ollama_signal_unload() {
    #[cfg(unix)]
    {
        // Catch SIGTERM before the default terminate-without-destructors path.
        std::thread::spawn(|| {
            let mut signals =
                match signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT]) {
                    Ok(s) => s,
                    Err(_) => return,
                };
            for _ in signals.forever() {
                unload_ollama_from_warm_file();
                std::process::exit(0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{export_default_hermes_home, launch_token, parse_gateway_args, profile_home, sticky_profile};
    use std::path::{Path, PathBuf};

    #[test]
    fn launch_token_comes_from_stdin_when_asked_else_the_dev_env() {
        let flag = ["serve".to_string(), "--token-stdin".to_string()];
        let env = Some("from-env".to_string());
        assert_eq!(launch_token(&flag, &b"from-pipe\n"[..], env.clone()).unwrap().as_deref(), Some("from-pipe"));
        assert!(launch_token(&flag, &b"\n"[..], env.clone()).is_err(), "no silent fallback");
        assert_eq!(launch_token(&["serve".to_string()], &b"ignored\n"[..], env).unwrap().as_deref(), Some("from-env"));
        assert_eq!(launch_token(&[], &b""[..], None).unwrap(), None);
        assert!(parse_gateway_args(&["serve".to_string(), "--token-stdin".to_string()]).is_ok());
    }

    #[test]
    fn desktop_gateway_arguments_parse_without_cli_dispatch() {
        let args = [
            "--profile",
            "work",
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
        ]
        .map(str::to_string);
        let args = parse_gateway_args(&args).unwrap();
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, 0);
        assert!(!args.allow_remote);
    }

    #[test]
    fn selected_profile_scopes_hermes_home_under_profiles() {
        assert_eq!(
            profile_home(Path::new("/tmp/hermes"), "research").unwrap(),
            PathBuf::from("/tmp/hermes/profiles/research")
        );
        assert_eq!(
            profile_home(Path::new("/tmp/hermes/profiles/active"), "research").unwrap(),
            PathBuf::from("/tmp/hermes/profiles/research")
        );
        assert_eq!(
            profile_home(Path::new("/tmp/hermes"), "default").unwrap(),
            PathBuf::from("/tmp/hermes")
        );
        assert!(profile_home(Path::new("/tmp/hermes"), "../escape").is_err());
    }

    #[test]
    fn standalone_exports_the_default_hermes_home() {
        // SAFETY: the only test that touches these variables.
        unsafe { std::env::remove_var("HERMES_HOME") };
        export_default_hermes_home();
        assert_eq!(std::env::var_os("HERMES_HOME").map(PathBuf::from), std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hermes")));
        unsafe { std::env::set_var("HERMES_HOME", "/tmp/other") };
        export_default_hermes_home();
        assert_eq!(std::env::var("HERMES_HOME").unwrap(), "/tmp/other", "an explicit value wins");
    }

    #[test]
    fn without_a_profile_flag_the_sticky_active_profile_applies_like_in_hermes() {
        let root = std::env::temp_dir().join(format!("sticky-{}", std::process::id()));
        std::fs::create_dir_all(root.join("profiles/work")).unwrap();
        assert_eq!(sticky_profile(&root), None, "no active_profile file");
        std::fs::write(root.join("active_profile"), "work\n").unwrap();
        assert_eq!(sticky_profile(&root).as_deref(), Some("work"));
        assert_eq!(sticky_profile(&root.join("profiles/work")), None, "HERMES_HOME already names a profile");
        std::fs::write(root.join("active_profile"), "default").unwrap();
        assert_eq!(sticky_profile(&root), None);
        std::fs::write(root.join("active_profile"), "gone").unwrap();
        assert_eq!(sticky_profile(&root), None, "not a profile directory: ignored");
        let _ = std::fs::remove_dir_all(root);
    }
}
