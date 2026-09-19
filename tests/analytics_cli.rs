#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
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
    fn unstarted() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-analytics-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, child: None }
    }
    fn new() -> Self {
        let mut service = Self::unstarted();
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
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
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
            assert!(Instant::now() < deadline, "service readiness timed out");
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
    fn session(&self) {
        let profile = json!({"project":self.root,"purpose":"private-purpose-not-analytics",
            "provider":"fake","credentials":{"TOKEN":"fake://analytics"},"cache_ttl_seconds":60,
            "commands":[{"executable":"/usr/bin/printenv","args":["TOKEN"]},{"executable":"/bin/sh","args":["-c","exit 7"]}]});
        let path = self.root.join("profile.json");
        fs::write(&path, profile.to_string()).unwrap();
        self.ok(&[
            "session",
            "start",
            "work",
            "--profile",
            path.to_str().unwrap(),
        ]);
    }
    fn adapter(&self, calls: &[Value]) -> Vec<Value> {
        self.adapter_for(calls, "mcp")
    }
    fn adapter_for(&self, calls: &[Value], name: &str) -> Vec<Value> {
        let mut child = self
            .command()
            .args(["agent", "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":name,"version":"test"}}})).unwrap();
        for (index, call) in calls.iter().enumerate() {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","id":index+1,"method":"tools/call","params":call})
            )
            .unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("latchrun-fake-analytics"));
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn telemetry_failure_preserves_a_completed_mcp_operation_without_retry() {
    let service = Service::unstarted();
    let listener = UnixListener::bind(service.root.join("service.sock")).unwrap();
    fs::set_permissions(
        service.root.join("service.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let server = thread::spawn(move || {
        let (mut command, _) = listener.accept().unwrap();
        command
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(command.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "run");
        assert_eq!(request["origin"], "mcp");
        for response in [
            json!({"type":"accepted","operation":"completed-once"}),
            json!({"type":"finished","exit_code":0}),
        ] {
            writeln!(command, "{response}").unwrap();
        }
        drop(command);
        let (mut telemetry, _) = listener.accept().unwrap();
        telemetry
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        line.clear();
        BufReader::new(telemetry.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["type"], "activity");
        assert_eq!(request["tool"], "latchrun_run");
        assert_eq!(request["success"], true);
        writeln!(telemetry, "{}", json!({"type":"error","code":"analytics_unavailable","message":"Usage recording unavailable."})).unwrap();
    });
    let frames = service.adapter(&[json!({"name":"latchrun_run","arguments":{"session":"work","operation":"completed-once","argv":["/bin/true"]}})]);
    server.join().unwrap();
    assert_eq!(frames[1]["result"]["isError"], false);
    assert_eq!(frames[1]["result"]["structuredContent"]["exit_code"], 0);
    assert!(
        frames[1]["result"]["_meta"]["telemetry_warning"]
            .as_str()
            .unwrap()
            .contains("do not repeat")
    );
}

#[test]
fn command_analytics_survive_history_pruning_and_service_restart() {
    let mut service = Service::new();
    service.session();
    for operation in ["first", "second"] {
        let output = service.cli(&[
            "run",
            "work",
            "--operation",
            operation,
            "--",
            "/usr/bin/printenv",
            "TOKEN",
        ]);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"[REDACTED]\n");
    }
    let denied = service.cli(&[
        "run",
        "work",
        "--operation",
        "denied",
        "--",
        "/bin/sh",
        "-c",
        "printf forbidden-private-argument",
    ]);
    assert!(!denied.status.success());
    let stats = service.ok(&["stats", "--days", "7"]);
    assert_eq!(stats["totals"]["accepted"], 2);
    assert_eq!(stats["totals"]["succeeded"], 2);
    assert_eq!(stats["totals"]["failed"], 0);
    assert_eq!(stats["totals"]["denied"], 1);
    assert_eq!(stats["totals"]["sessions"], 1);
    assert_eq!(stats["cache"]["hits"], 1);
    assert_eq!(stats["cache"]["misses"], 1);
    assert_eq!(stats["cache"]["hit_rate_pct"], 50.0);
    assert_eq!(stats["latency"]["samples"], 2);
    assert_eq!(stats["timeline"].as_array().unwrap().len(), 7);
    service.ok(&["history", "prune", "--keep", "0"]);
    service.stop();
    service.start();
    let restored = service.ok(&["stats"]);
    assert_eq!(restored["totals"], stats["totals"]);
    assert_eq!(restored["cache"], stats["cache"]);
    assert_eq!(restored["latency"], stats["latency"]);
    assert!(!service.cli(&["stats", "--days", "2"]).status.success());
    let database = service.root.join("analytics.sqlite3");
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let content = fs::read(database).unwrap();
    for forbidden in [
        "latchrun-fake-analytics",
        "fake://analytics",
        "private-purpose-not-analytics",
        "forbidden-private-argument",
    ] {
        assert!(
            !content
                .windows(forbidden.len())
                .any(|part| part == forbidden.as_bytes())
        );
        assert!(!restored.to_string().contains(forbidden));
    }
}

#[test]
fn external_reports_deduplicate_identical_payloads_and_reject_conflicts() {
    let mut service = Service::new();
    let report = [
        "activity",
        "record",
        "--id",
        "external-once",
        "--agent",
        "editor",
        "--tool",
        "read_file",
        "--duration-ms",
        "125",
        "--outcome",
        "success",
    ];
    service.ok(&report);
    service.ok(&report);
    let mut conflict = report;
    conflict[9] = "126";
    let output = service.cli(&conflict);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("activity_conflict"));
    let stats = service.ok(&["stats", "--days", "1"]);
    assert_eq!(stats["activity"]["calls"], 1);
    assert_eq!(stats["activity"]["errors"], 0);
    assert_eq!(stats["timeline"].as_array().unwrap().len(), 24);
    assert_eq!(stats["agents"][0]["agent"], "editor");
    assert_eq!(stats["agents"][0]["tool"], "read_file");
    assert_eq!(stats["agents"][0]["source"], "external");
    assert_eq!(stats["agents"][0]["calls"], 1);
    assert_eq!(stats["agents"][0]["p50_ms"], 125);
    service.stop();
    service.start();
    service.ok(&report);
    assert_eq!(service.ok(&["stats"])["activity"]["calls"], 1);
}

#[test]
fn mcp_analytics_and_failed_commands_record_only_safe_tool_metadata() {
    let service = Service::new();
    service.session();
    let frames = service.adapter(&[
        json!({"name":"latchrun_status","arguments":{"session":"work"}}),
        json!({"name":"latchrun_run","arguments":{"session":"work","operation":"mcp-failed","argv":["/bin/sh","-c","exit 7"]}}),
        json!({"name":"latchrun_analytics","arguments":{"days":30}}),
        json!({"name":"latchrun_analytics","arguments":{"days":2}}),
        json!({"name":"private-invalid-tool-name","arguments":{"private":"private-invalid-argument"}}),
    ]);
    assert_eq!(frames[2]["result"]["isError"], false);
    assert_eq!(frames[2]["result"]["structuredContent"]["exit_code"], 7);
    assert_eq!(frames[3]["result"]["structuredContent"]["days"], 30);
    assert_eq!(frames[4]["result"]["isError"], true);
    assert_eq!(frames[5]["result"]["isError"], true);
    let stats = service.ok(&["stats"]);
    assert_eq!(stats["totals"]["failed"], 1);
    assert_eq!(stats["activity"]["calls"], 4);
    assert_eq!(stats["activity"]["errors"], 2);
    let groups = stats["agents"].as_array().unwrap();
    let run = groups
        .iter()
        .find(|group| group["tool"] == "latchrun_run")
        .unwrap();
    assert_eq!(run["agent"], "mcp");
    assert_eq!(run["source"], "mcp");
    assert_eq!(run["calls"], 1);
    assert_eq!(run["errors"], 1);
    let text = stats.to_string();
    assert!(!text.contains("private-invalid"));
    assert!(!text.contains("exit 7"));
}

#[test]
fn mcp_client_labels_are_bounded_metadata_and_unsafe_names_fall_back() {
    let service = Service::new();
    let call = json!({"name":"latchrun_status","arguments":{}});
    service.adapter_for(std::slice::from_ref(&call), "fixture_agent");
    service.adapter_for(&[call], "unsafe/client-private-name");
    let stats = service.ok(&["stats"]);
    assert_eq!(stats["activity"]["calls"], 2);
    let groups = stats["agents"].as_array().unwrap();
    assert!(groups.iter().any(|group| group["agent"] == "fixture_agent"));
    assert!(groups.iter().any(|group| group["agent"] == "mcp"));
    assert!(!stats.to_string().contains("client-private-name"));
    let bytes = fs::read(service.root.join("analytics.sqlite3")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("client-private-name"));
}
