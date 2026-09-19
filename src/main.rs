use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let first = args.next();
    if args.next().is_some() {
        eprintln!("Unexpected arguments. Use latchrun --help.");
        return ExitCode::from(2);
    }

    match first.as_deref().and_then(std::ffi::OsStr::to_str) {
        None if first.is_none() => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version" | "-V") => {
            println!("latchrun {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Unknown command. Use latchrun --help.");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "Latchrun — local command sessions with scoped credential access.\n\n\
         Usage: latchrun [--help | --version]\n\n\
         Project scaffold only. Session and credential operations are not implemented.\n\
         See BUILD_BRIEF.md for the implementation plan."
    );
}
