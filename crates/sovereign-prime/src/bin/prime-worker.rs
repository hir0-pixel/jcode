//! Standalone REPL worker (tests and diagnostics). The sovereign binary runs
//! the same worker for `sovereign __repl-worker`.

#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() != Some("__repl-worker") {
        eprintln!("usage: prime-worker __repl-worker");
        std::process::exit(2);
    }
    sovereign_prime::worker::run(sovereign_prime::worker::Limits::default())
}
