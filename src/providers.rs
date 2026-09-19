//! Providers return bounded in-memory values and never relay provider diagnostics.
use crate::{
    execution::{Supervisor, read_provider},
    protocol::{Failure, Profile, Provider},
};
use nix::unistd::getuid;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

pub const SECRET_LIMIT: usize = 65_536;
pub type Credentials = BTreeMap<String, Vec<u8>>;

pub fn failure() -> Failure {
    Failure::new(
        "provider_unavailable",
        "Credential resolution failed. Check the configured provider, unlock its store and sign in, then try again.",
    )
}

pub fn validate(profile: &Profile, credentials: &Credentials) -> Result<(), Failure> {
    let total = credentials
        .values()
        .fold(0_usize, |total, value| total.saturating_add(value.len()));
    if credentials.keys().ne(profile.credentials.keys())
        || total > SECRET_LIMIT
        || credentials
            .values()
            .any(|value| value.is_empty() || value.contains(&0))
    {
        return Err(failure());
    }
    Ok(())
}

pub fn resolve(
    profile: &Profile,
    supervisor: &Supervisor,
    cached: Option<Credentials>,
) -> Result<Credentials, Failure> {
    if let Some(credentials) = cached {
        if profile.cache_ttl_seconds == 0 {
            return Err(failure());
        }
        validate(profile, &credentials)?;
        return Ok(credentials);
    }
    let mut credentials = Credentials::new();
    let mut total = 0_usize;
    for (name, reference) in &profile.credentials {
        if supervisor.cancelled() {
            return Err(Failure::new(
                "cancelled",
                "Command execution was cancelled.",
            ));
        }
        let value = match profile.provider {
            Provider::Fake => reference
                .strip_prefix("fake://")
                .map(|name| format!("latchrun-fake-{name}").into_bytes())
                .ok_or_else(failure)?,
            Provider::OnePassword => read_provider(
                profile.op_path.as_deref().ok_or_else(failure)?,
                &["read", "--no-newline", reference],
                supervisor,
            )?,
            Provider::File => read_file(reference)?,
            Provider::PasswordStore => {
                let entry = pass_entry(reference)?;
                let value = read_provider(
                    profile.provider_path.as_deref().ok_or_else(failure)?,
                    &["show", entry],
                    supervisor,
                )?;
                value
                    .split(|byte| *byte == b'\n')
                    .next()
                    .unwrap_or_default()
                    .to_vec()
            }
        };
        total = total.saturating_add(value.len());
        if value.is_empty() || value.contains(&0) || total > SECRET_LIMIT {
            return Err(failure());
        }
        credentials.insert(name.clone(), value);
    }
    Ok(credentials)
}

fn pass_entry(reference: &str) -> Result<&str, Failure> {
    let entry = reference.strip_prefix("pass://").ok_or_else(failure)?;
    if entry.is_empty()
        || entry.starts_with('-')
        || entry.contains(['\0', '\n', '\r'])
        || entry.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(failure());
    }
    Ok(entry)
}

fn read_file(reference: &str) -> Result<Vec<u8>, Failure> {
    let path = Path::new(reference.strip_prefix("file://").ok_or_else(failure)?);
    if !path.is_absolute() {
        return Err(failure());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| failure())?;
    let metadata = file.metadata().map_err(|_| failure())?;
    if !metadata.is_file()
        || metadata.uid() != getuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 65_536
    {
        return Err(failure());
    }
    let mut value = Vec::new();
    file.take(65_537)
        .read_to_end(&mut value)
        .map_err(|_| failure())?;
    if value.len() > SECRET_LIMIT {
        return Err(failure());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::pass_entry;
    #[test]
    fn password_store_rejects_options_and_traversal() {
        for reference in [
            "pass://",
            "pass://-option",
            "pass://../secret",
            "pass:///absolute",
            "pass://folder/../secret",
            "pass://folder//secret",
        ] {
            assert!(pass_entry(reference).is_err());
        }
        assert_eq!(
            pass_entry("pass://work/github.com").ok(),
            Some("work/github.com")
        );
    }
}
