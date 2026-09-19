#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Service {
    root: PathBuf,
    child: Option<Child>,
}
impl Service {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut service = Self { root, child: None };
        service.start();
        service
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
        command.env_clear().arg("--runtime-dir").arg(&self.root);
        command
    }
    fn cli(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
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
    fn start(&mut self) {
        self.child = Some(
            self.command()
                .args(["service", "serve"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.cli(&["service", "status"]).status.success() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = self.cli(&["service", "stop"]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn profile(&self, script: &str) -> Value {
        json!({"project":self.root,"purpose":"sensitive-purpose-do-not-journal","provider":"fake","credentials":{"TOKEN":"fake://test"},"commands":[{"executable":"/bin/sh","args":["-c",script]}]})
    }
    fn session(&self, profile: &Value) -> PathBuf {
        let path = self.root.join("profile.json");
        fs::write(&path, profile.to_string()).unwrap();
        self.ok(&[
            "session",
            "start",
            "work",
            "--profile",
            path.to_str().unwrap(),
        ]);
        path
    }
    fn run(&self, id: &str, script: &str) -> Output {
        self.cli(&[
            "run",
            "work",
            "--operation",
            id,
            "--",
            "/bin/sh",
            "-c",
            script,
        ])
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn journal_recovers_metadata_requires_resume_and_never_replays_pruned_ids() {
    let mut service = Service::new();
    let script = "printf '%s' \"$TOKEN\"";
    let path = service.session(&service.profile(script));
    let out = service.run("durable", script);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"[REDACTED]");
    service.stop();
    let journal = fs::read_to_string(service.root.join("history.json")).unwrap();
    for forbidden in [
        "latchrun-fake-test",
        "fake://test",
        script,
        "sensitive-purpose-do-not-journal",
        "credentials",
    ] {
        assert!(!journal.contains(forbidden));
    }
    assert_eq!(
        fs::metadata(service.root.join("history.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    service.start();
    let status = service.ok(&["session", "reconnect", "work"]);
    assert_eq!(status["operations"][0]["exit_code"], 0);
    assert!(!service.run("new-before-resume", script).status.success());
    service.ok(&[
        "session",
        "resume",
        "work",
        "--profile",
        path.to_str().unwrap(),
    ]);
    assert!(
        String::from_utf8_lossy(&service.run("durable", script).stderr)
            .contains("duplicate_operation")
    );
    service.ok(&["history", "prune", "--keep", "0"]);
    assert!(
        String::from_utf8_lossy(&service.run("durable", script).stderr)
            .contains("duplicate_operation")
    );
    service.stop();
    service.start();
    service.ok(&[
        "session",
        "resume",
        "work",
        "--profile",
        path.to_str().unwrap(),
    ]);
    assert!(
        String::from_utf8_lossy(&service.run("durable", script).stderr)
            .contains("duplicate_operation")
    );
}

#[test]
fn corrupt_or_public_history_fails_closed() {
    let mut service = Service::new();
    service.session(&service.profile("true"));
    service.stop();
    let path = service.root.join("history.json");
    let valid = fs::read(&path).unwrap();
    fs::write(&path, b"{broken-json").unwrap();
    let failed = service.cli(&["service", "serve"]);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("broken-json"));
    fs::write(&path, valid).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!service.cli(&["service", "serve"]).status.success());
}

#[test]
fn cache_refresh_and_environment_inspection_never_return_secret_values() {
    let service = Service::new();
    let script = "test \"$TOKEN\" = latchrun-fake-test";
    let mut profile = service.profile(script);
    profile["cache_ttl_seconds"] = json!(60);
    profile["environment"] = json!({"PUBLIC_MODE":"preview","HIDDEN_MODE":"not-exposed"});
    profile["expose_environment"] = json!(["PUBLIC_MODE"]);
    service.session(&profile);
    assert!(service.run("cache", script).status.success());
    let view = service.ok(&["inspect", "work"]);
    let text = view.to_string();
    assert!(text.contains("cached"));
    assert!(text.contains("preview"));
    assert!(!text.contains("not-exposed"));
    assert!(!text.contains("latchrun-fake-test"));
    assert!(view["cache_expires_at"].is_number());
    service.ok(&["session", "refresh", "work"]);
    let view = service.ok(&["inspect", "work"]);
    assert!(view["cache_expires_at"].is_null());
    assert!(view.to_string().contains("resolved_per_operation"));
    assert!(
        !fs::read_to_string(service.root.join("history.json"))
            .unwrap()
            .contains("latchrun-fake-test")
    );
}

#[test]
fn explicit_shell_mode_still_requires_an_exact_approved_script() {
    let service = Service::new();
    let script = "printf shell-ok";
    let mut profile = service.profile(script);
    profile["shell"] = json!({"executable":"/bin/sh","args":["-c"]});
    service.session(&profile);
    let result = service.cli(&["run", "work", "--operation", "shell-ok", "--shell", script]);
    assert!(result.status.success());
    assert_eq!(result.stdout, b"shell-ok");
    let denied = service.cli(&[
        "run",
        "work",
        "--operation",
        "shell-denied",
        "--shell",
        "printf not-approved",
    ]);
    assert!(!denied.status.success());
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("not-approved"));
    let status = service.ok(&["service", "status"]);
    assert_eq!(status["statistics"]["denied"], 1);
    let events = service.ok(&["events", "work"]);
    assert!(events.to_string().contains("shell-denied"));
    assert!(!events.to_string().contains("not-approved"));
}

#[test]
fn mcp_adapter_uses_existing_scope_redacts_and_preserves_operation_identity() {
    let service = Service::new();
    let script = "printf '%s' \"$TOKEN\"";
    service.session(&service.profile(script));
    let mut agent = service
        .command()
        .args(["agent", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = agent.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}),
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"latchrun_run","arguments":{"session":"work","operation":"agent-op","argv":["/bin/sh","-c",script]}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"latchrun_run","arguments":{"session":"work","operation":"agent-op","argv":["/bin/sh","-c",script]}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = agent.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("latchrun-fake-test"));
    let frames: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[0]["error"]["code"], -32601);
    assert_eq!(
        frames[3]["result"]["structuredContent"]["stdout"],
        "[REDACTED]"
    );
    assert_eq!(frames[4]["result"]["isError"], true);
    assert!(frames[4].to_string().contains("duplicate_operation"));
}

#[test]
fn git_https_helper_only_returns_credentials_to_matching_https_host() {
    let service = Service::new();
    let binary = "/usr/bin/git";
    let mut profile = service.profile("true");
    profile["git_https"] =
        json!({"host":"git.example.invalid","username":"operator","token_env":"TOKEN"});
    profile["commands"] = json!([{"executable":binary,"args":["credential","fill"]}]);
    service.session(&profile);
    for (id, host, success) in [
        ("matching", "git.example.invalid", true),
        ("mismatch", "other.example.invalid", false),
    ] {
        let mut child = service
            .command()
            .args([
                "run",
                "work",
                "--operation",
                id,
                "--stdin",
                "--",
                binary,
                "credential",
                "fill",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "protocol=https\nhost={host}\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), success);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("latchrun-fake-test"));
        if success {
            assert!(String::from_utf8_lossy(&output.stdout).contains("password=[REDACTED]"));
        }
    }
}
