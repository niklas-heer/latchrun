mod agent;
mod client;
mod dashboard;
mod execution;
mod git_credentials;
mod journal;
mod protocol;
mod providers;
mod sandbox;
mod service;
mod terminal;

use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let result = if env::args_os().nth(1).is_some_and(|arg| arg == "__worker") {
        execution::worker_main().map(|()| 0)
    } else if env::args_os().nth(1).is_some_and(|arg| arg == "__ptyexec") {
        execution::pty_exec_main().map(|()| 0)
    } else if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "__git_credential")
    {
        git_credentials::helper().map(|()| 0)
    } else {
        client::main_cli()
    };
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("latchrun: {error}");
            ExitCode::from(if error.code == "usage" { 2 } else { 1 })
        }
    }
}
