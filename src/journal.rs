//! Atomic, owner-only metadata snapshots. No profile or credential values enter this module.
use crate::protocol::{Failure, random_id};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
const LIMIT: u64 = 16 * 1024 * 1024;
fn failure() -> Failure {
    Failure::new(
        "journal_unavailable",
        "Cannot safely read or commit operation history; execution is disabled until the journal is repaired.",
    )
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Failure> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure()),
    };
    let meta = file.metadata().map_err(|_| failure())?;
    if !meta.is_file()
        || meta.uid() != nix::unistd::getuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.len() > LIMIT
    {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| failure())
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), Failure> {
    let bytes = serde_json::to_vec(value).map_err(|_| failure())?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure());
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file()
            || meta.uid() != nix::unistd::getuid().as_raw()
            || meta.mode() & 0o077 != 0)
    {
        return Err(failure());
    }
    let parent = path.parent().ok_or_else(failure)?;
    let temporary = parent.join(format!("journal-{}.tmp", random_id()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| failure())?;
        file.write_all(&bytes).map_err(|_| failure())?;
        file.sync_all().map_err(|_| failure())?;
        fs::rename(&temporary, path).map_err(|_| failure())?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| failure())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
