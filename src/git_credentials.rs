//! Git's private credential-helper pipe, scoped to one HTTPS host.
use crate::protocol::{Failure, GitHttps};
use std::{
    env,
    io::{self, BufRead, Read, Write},
    process::Command,
};

pub fn configure(command: &mut Command, policy: &GitHttps) -> Result<(), Failure> {
    let executable = env::current_exe()?;
    let path = executable.to_str().ok_or_else(failure)?;
    let helper = format!("!'{}' __git_credential", path.replace('\'', "'\\''"));
    command
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", helper)
        .env("GIT_CONFIG_KEY_2", "credential.useHttpPath")
        .env("GIT_CONFIG_VALUE_2", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LATCHRUN_GIT_HOST", &policy.host)
        .env("LATCHRUN_GIT_USERNAME", &policy.username)
        .env("LATCHRUN_GIT_TOKEN_ENV", &policy.token_env);
    Ok(())
}

fn failure() -> Failure {
    Failure::new(
        "git_credential",
        "Git credential request is not authorized.",
    )
}

pub fn helper() -> Result<(), Failure> {
    let action = env::args().nth(2).ok_or_else(failure)?;
    if matches!(action.as_str(), "store" | "erase") {
        return Ok(());
    }
    if action != "get" {
        return Err(failure());
    }
    let mut protocol = None;
    let mut host = None;
    let mut total = 0usize;
    let mut reader = io::stdin().lock();
    loop {
        let mut line = String::new();
        let count = reader.by_ref().take(8193).read_line(&mut line)?;
        total = total.saturating_add(count);
        if total > 8192 {
            return Err(failure());
        }
        if count == 0 || line == "\n" {
            break;
        }
        let (key, value) = line
            .trim_end_matches(['\r', '\n'])
            .split_once('=')
            .ok_or_else(failure)?;
        match key {
            "protocol" if protocol.is_none() => protocol = Some(value.to_owned()),
            "host" if host.is_none() => host = Some(value.to_owned()),
            "protocol" | "host" => return Err(failure()),
            _ => (),
        }
    }
    let allowed_host = env::var("LATCHRUN_GIT_HOST").map_err(|_| failure())?;
    if protocol.as_deref() != Some("https") || host.as_deref() != Some(&allowed_host) {
        return Err(failure());
    }
    let username = env::var("LATCHRUN_GIT_USERNAME").map_err(|_| failure())?;
    let name = env::var("LATCHRUN_GIT_TOKEN_ENV").map_err(|_| failure())?;
    let value = env::var(&name).map_err(|_| failure())?;
    if value.is_empty()
        || value.contains(['\n', '\r', '\0'])
        || username.contains(['\n', '\r', '\0'])
    {
        return Err(failure());
    }
    // stdout is the pipe Git opens for its helper, never a dashboard/agent endpoint.
    let mut out = io::stdout().lock();
    writeln!(out, "username={username}\npassword={value}\n")?;
    Ok(())
}
