#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde_json::json;

static NEXT: AtomicUsize = AtomicUsize::new(0);
const SECRET: &str = "latchrun-fake-journal-fault";
const REFERENCE: &str = "fake://journal-fault";

struct Fixture {
    root: PathBuf,
    runtime: PathBuf,
    project: PathBuf,
    profile: PathBuf,
    daemon: Option<Child>,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-jf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let runtime = root.join("runtime");
        let project = root.join("project");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&project).unwrap();
        let profile = root.join("profile.json");
        fs::write(
            &profile,
            serde_json::to_vec(&json!({
                "project":project,"purpose":"journal fault fixture","provider":"fake",
                "credentials":{"TEST_SECRET":REFERENCE},"cache_ttl_seconds":60,
                "commands":[{"executable":"/bin/sh","args":["-c",script]}],"timeout_seconds":5
            }))
            .unwrap(),
        )
        .unwrap();
        let mut fixture = Self {
            root,
            runtime,
            project,
            profile,
            daemon: None,
        };
        fixture.start_daemon();
        success(&fixture.invoke(&[
            "session",
            "start",
            "work",
            "--profile",
            fixture.profile.to_str().unwrap(),
        ]));
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("--runtime-dir")
            .arg(&self.runtime)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn invoke(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn start_daemon(&mut self) {
        self.daemon = Some(self.command(&["service", "serve"]).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.invoke(&["service", "status"]).status.success() {
                return;
            }
            assert!(Instant::now() < deadline, "service did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_stopped(&mut self) {
        let daemon = self.daemon.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while daemon.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "journal fault did not stop service"
            );
            thread::sleep(Duration::from_millis(10));
        }
        no_credentials(&self.daemon.take().unwrap().wait_with_output().unwrap());
    }

    fn execute(&self, operation: &str, script: &str) -> Output {
        self.invoke(&[
            "run",
            "work",
            "--operation",
            operation,
            "--",
            "/bin/sh",
            "-c",
            script,
        ])
    }

    fn history(&self) -> PathBuf {
        self.runtime.join("history.json")
    }

    fn verify_journal(&self) {
        let data = fs::read(self.history()).unwrap();
        let text = String::from_utf8_lossy(&data);
        assert!(!text.contains(SECRET));
        assert!(!text.contains(REFERENCE));
        assert!(!text.contains("TEST_SECRET"));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = self.invoke(&["service", "stop"]);
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status {}; stdout {}; stderr {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    no_credentials(output);
}

fn no_credentials(output: &Output) {
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(SECRET));
        assert!(!text.contains(REFERENCE));
    }
}

fn failed_before_execution(use_symlink: bool) {
    let script = "printf '%s' \"$TEST_SECRET\"; printf effect >> effect.txt";
    let mut fixture = Fixture::new(script);
    let warm = fixture.execute("warm", script);
    success(&warm);
    assert_eq!(warm.stdout, b"[REDACTED]");
    fixture.verify_journal();
    let original = fs::read(fixture.history()).unwrap();
    let target = fixture.root.join("symlink-target");
    if use_symlink {
        fs::write(&target, b"unrelated-fake-file").unwrap();
        fs::remove_file(fixture.history()).unwrap();
        symlink(&target, fixture.history()).unwrap();
    } else {
        fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o644)).unwrap();
    }
    let output = fixture.execute("must-not-execute", script);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("journal_unavailable"));
    no_credentials(&output);
    let further = fixture.execute("also-must-not-execute", script);
    assert!(!further.status.success());
    no_credentials(&further);
    fixture.wait_stopped();
    assert_eq!(
        fs::read(fixture.project.join("effect.txt")).unwrap(),
        b"effect"
    );
    if use_symlink {
        assert!(
            fs::symlink_metadata(fixture.history())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target).unwrap(), b"unrelated-fake-file");
    } else {
        assert_eq!(fs::read(fixture.history()).unwrap(), original);
        fixture.verify_journal();
    }
}

#[test]
fn unsafe_journal_permissions_deny_execution_and_stop_service() {
    failed_before_execution(false);
}

#[test]
fn journal_symlink_does_not_touch_target_or_execute_command() {
    failed_before_execution(true);
}

#[test]
fn completion_commit_failure_reports_unknown_and_restart_never_replays() {
    let script = "printf ready > ready.tmp; mv ready.tmp ready; while [ ! -e release ]; do sleep 0.02; done; printf effect >> effect.txt; printf '%s' \"$TEST_SECRET\"";
    let mut fixture = Fixture::new(script);
    let mut client = fixture
        .command(&[
            "run",
            "work",
            "--operation",
            "effect-once",
            "--",
            "/bin/sh",
            "-c",
            script,
        ])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.project.join("ready").exists() {
        assert!(
            Instant::now() < deadline,
            "child did not reach fault barrier"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The operation reservation is already durable; fail only the final commit.
    fixture.verify_journal();
    fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(fixture.project.join("release"), b"go").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while client.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = client.kill();
            let _ = client.wait();
            panic!("client did not report failed completion commit");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = client.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outcome_unknown"));
    no_credentials(&output);
    fixture.wait_stopped();
    assert_eq!(
        fs::read(fixture.project.join("effect.txt")).unwrap(),
        b"effect"
    );
    fixture.verify_journal();
    fs::set_permissions(fixture.history(), fs::Permissions::from_mode(0o600)).unwrap();
    fixture.start_daemon();
    let status = fixture.invoke(&["session", "status", "work"]);
    success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("unknown"));
    success(&fixture.invoke(&[
        "session",
        "resume",
        "work",
        "--profile",
        fixture.profile.to_str().unwrap(),
    ]));
    let replay = fixture.execute("effect-once", script);
    assert!(!replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("duplicate_operation"));
    no_credentials(&replay);
    assert_eq!(
        fs::read(fixture.project.join("effect.txt")).unwrap(),
        b"effect"
    );
    fixture.verify_journal();
}
