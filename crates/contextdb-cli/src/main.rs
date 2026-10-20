//! Bootstrap command metadata; database commands are implemented in later phases.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print_help();
            ExitCode::SUCCESS
        }
        [arg] if arg == "--help" || arg == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        [arg] if arg == "--version" || arg == "-V" => {
            println!("ctx {} (workspace scaffold)", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("ctx: database commands are not implemented; use --help for project status.");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "ContextDB — workspace scaffold\n\nUsage: ctx [--help | --version]\n\nStatus: P0 in progress; transaction semantics and database commands are not implemented.\nSpecification: SPEC.md; verification checklist: docs/gates/P0.md"
    );
}
