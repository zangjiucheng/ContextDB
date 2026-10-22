//! `ctx-hook <claude|codex> <event>`: one hook event from stdin, at most one
//! JSON object on stdout, always exit 0 (ADR 0010 §3.4). A hook must never
//! break the harness: bad arguments and failures print nothing.
use contextdb_hooks::{
    adapter::{EventName, Harness},
    hook,
};
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("-V" | "--version")) {
        println!("ctx-hook {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (Some(harness), Some(event)) = (
        args.first().and_then(|h| Harness::parse(h)),
        args.get(1).and_then(|e| EventName::from_arg(e)),
    ) else {
        eprintln!("usage: ctx-hook <claude|codex> <event>");
        return;
    };
    let mut stdin = String::new();
    if std::io::stdin().read_to_string(&mut stdin).is_err() {
        return;
    }
    if let Some(out) = hook::run(harness, event, &stdin) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
}
