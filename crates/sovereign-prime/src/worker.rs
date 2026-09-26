//! Embedded CPython worker program. The long-lived interpreter is started only
//! when a session first calls the REPL; its parent owns framing and host calls.

pub const PYTHON_WORKER: &str = include_str!("python_worker.py");
