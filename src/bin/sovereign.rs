//! `sovereign`: the engine binary with the Hermes backend command line, so the
//! Hermes desktop can launch it exactly as it launches `hermes serve`:
//!
//!   sovereign [--profile P] serve --host H --port N   →  jcode gateway --host H --port N
//!   sovereign [--profile P] dashboard --no-open ...   →  same (legacy spelling)
//!
//! Anything else passes through unchanged, so `sovereign login` etc. still work.

use anyhow::Result;

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
