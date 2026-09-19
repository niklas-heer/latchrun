#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    runtime: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-data-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self {
            runtime: root.join("run"),
            data: root.join("config/latchrun"),
            root,
        }
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_latchrun"))
            .env_clear()
            .args([
                "--runtime-dir",
                self.runtime.to_str().unwrap(),
                "--data-dir",
                self.data.to_str().unwrap(),
            ])
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli(&["service", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(6);
        while self.runtime.join("service.sock").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn data_location_is_explicit_and_path_inspection_creates_nothing() {
    let fixture = Fixture::new();
    let binary = env!("CARGO_BIN_EXE_latchrun");
    let result = Command::new(binary)
        .env_clear()
        .env("XDG_CONFIG_HOME", &fixture.root)
        .args(["data", "path"])
        .output()
        .unwrap();
    assert!(result.status.success());
    let data: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        data["directory"],
        fixture.root.join("latchrun").to_str().unwrap()
    );
    let result = Command::new(binary)
        .env_clear()
        .env("XDG_CONFIG_HOME", &fixture.root)
        .args([
            "--runtime-dir",
            fixture.runtime.to_str().unwrap(),
            "data",
            "path",
        ])
        .output()
        .unwrap();
    let data: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(data["directory"], fixture.runtime.to_str().unwrap());
    assert_eq!(
        fixture.ok(&["data", "path"])["directory"],
        fixture.data.to_str().unwrap()
    );
    assert!(!fixture.data.exists());
    assert!(!fixture.runtime.exists());
}

#[test]
fn background_service_uses_private_custom_storage_and_protects_it_from_children() {
    let fixture = Fixture::new();
    fixture.ok(&["service", "start"]);
    let database = fixture.data.join("analytics.sqlite3");
    assert!(database.is_file());
    assert!(!fixture.runtime.join("analytics.sqlite3").exists());
    for (path, mode) in [(&fixture.data, 0o700), (&database, 0o600)] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    let script = format!(
        "if /bin/cat '{}' 2>/dev/null | /usr/bin/grep -q SQLite; then exit 9; fi; printf protected",
        database.display()
    );
    let profile = fixture.root.join("profile.json");
    fs::write(&profile, json!({"project":fixture.root,"purpose":"fake storage test","provider":"fake","sandbox":{"enabled":true},"commands":[{"executable":"/bin/sh","args":["-c",script]}]}).to_string()).unwrap();
    fixture.ok(&[
        "session",
        "start",
        "work",
        "--profile",
        profile.to_str().unwrap(),
    ]);
    let out = fixture.cli(&[
        "run",
        "work",
        "--operation",
        "data-protection",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"protected");
    assert_eq!(fixture.ok(&["stats"])["totals"]["succeeded"], 1);
}

#[test]
fn existing_public_or_symlink_data_directory_is_rejected_without_chmod() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.data).unwrap();
    fs::set_permissions(&fixture.data, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!fixture.cli(&["service", "serve"]).status.success());
    assert_eq!(
        fs::metadata(&fixture.data).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fs::remove_dir(&fixture.data).unwrap();
    let target = fixture.root.join("private");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &fixture.data).unwrap();
    assert!(!fixture.cli(&["service", "serve"]).status.success());
    assert!(!target.join("analytics.sqlite3").exists());
}
