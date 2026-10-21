//! `ctxd <dir> [--allow-fs <name>]...`: one daemon per data directory (ADR 0008).
use contextdb_daemon::server::{Daemon, Options, StartError};
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut options = Options::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--allow-fs" => match args.next() {
                Some(fs) => options.allow_fs.push(fs),
                None => return usage(),
            },
            "-h" | "--help" => return usage(),
            "-V" | "--version" => {
                println!("ctxd {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            _ => return usage(),
        }
    }
    let Some(dir) = dir else { return usage() };
    match Daemon::start(&dir, &options) {
        Ok(daemon) => {
            for s in &daemon.sockets {
                println!("listening {}", s.display());
            }
            daemon.wait();
            daemon.stop();
            ExitCode::SUCCESS
        }
        Err(StartError::Locked) => {
            eprintln!("ctxd: another ctxd already serves {}", dir.display());
            ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("ctxd: {e:?}");
            ExitCode::from(2)
        }
    }
}
fn usage() -> ExitCode {
    eprintln!("usage: ctxd <dir> [--allow-fs <name>]...");
    ExitCode::from(2)
}
