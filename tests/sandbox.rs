#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::{
    fs,
    net::{TcpListener, TcpStream},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    runtime: PathBuf,
    project: PathBuf,
    service: Child,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lrs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create isolated fixture");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let root = root.canonicalize().unwrap();
        let runtime = root.join("runtime");
        let project = root.join("project");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&project).unwrap();
        let service = Command::new(env!("CARGO_BIN_EXE_latchrun"))
            .arg("--runtime-dir")
            .arg(&runtime)
            .args(["service", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let this = Self {
            root,
            runtime,
            project,
            service,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !this.run(&["service", "status"]).status.success() {
            assert!(Instant::now() < deadline, "service startup timed out");
            thread::sleep(Duration::from_millis(10));
        }
        this
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_latchrun"))
            .arg("--runtime-dir")
            .arg(&self.runtime)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn start(&self, name: &str, executable: &Path, args: &[&str], sandbox: &Value) -> Output {
        let profile = json!({
            "project": self.project, "purpose":"sandbox fixture", "provider":"fake",
            "credentials":{"TEST_SECRET":"fake://network-probe"},
            "commands":[{"executable":executable,"args":args}],
            "timeout_seconds":5, "sandbox":sandbox,
        });
        let path = self.root.join(format!("{name}.json"));
        fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
        self.run(&[
            "session",
            "start",
            name,
            "--profile",
            path.to_str().unwrap(),
        ])
    }

    fn execute(&self, name: &str, executable: &Path, args: &[&str]) -> Output {
        let mut command = vec!["run", name, "--", executable.to_str().unwrap()];
        command.extend_from_slice(args);
        self.run(&command)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["service", "stop"]);
        let _ = self.service.kill();
        let _ = self.service.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

// This is deliberately opt-in only for hosts which actually cannot provide the
// backend. Full CI must run the normal enforcement tests without this variable.
fn unavailable_is_expected(output: &Output) -> bool {
    if std::env::var_os("LATCHRUN_TEST_SANDBOX_UNAVAILABLE").is_none() {
        return false;
    }
    assert!(
        !output.status.success(),
        "backend unexpectedly available: run full enforcement checks"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostics.contains("sandbox_unavailable") || diagnostics.contains("bwrap:"),
        "unexpected failure: {diagnostics}"
    );
    true
}

#[test]
fn sandbox_confines_filesystem_and_descendants_with_symlink_escape_attempts() {
    let fixture = Fixture::new();
    let readable = fixture.root.join("readable\"\\policy");
    let writable = fixture.root.join("writable");
    let outside = fixture.root.join("outside");
    for path in [&readable, &writable, &outside] {
        fs::create_dir(path).unwrap();
    }
    fs::write(readable.join("input"), "read-allowed").unwrap();
    fs::write(outside.join("private"), "outside-data").unwrap();
    let protected = writable.join("protected");
    fs::create_dir(&protected).unwrap();
    fs::write(protected.join("private"), "protected-data").unwrap();
    let protected_file = writable.join("protected-file");
    fs::write(&protected_file, "protected-file-data").unwrap();
    symlink(&outside, writable.join("escape")).unwrap();
    symlink(&protected, writable.join("protected-link")).unwrap();
    fs::write(fixture.project.join("source"), "project-read").unwrap();
    let script = format!(
        "set -eu; cat {read}/input; cat source; printf allowed > {write}/output; \
        if (printf no > source) 2>/dev/null; then exit 31; fi; \
        if cat {out}/private 2>/dev/null; then exit 32; fi; \
        if cat {write}/escape/private 2>/dev/null; then exit 33; fi; \
        if cat {write}/protected-link/private 2>/dev/null; then exit 34; fi; \
        if (printf no > {write}/protected/new) 2>/dev/null; then exit 35; fi; \
        if (printf no > {file}) 2>/dev/null; then exit 36; fi; \
        if cat {file} 2>/dev/null | grep -q protected-file-data; then exit 37; fi; \
        /bin/sh -c 'if cat {out}/private 2>/dev/null; then exit 38; fi'; \
        if (printf no > {out}/new) 2>/dev/null; then exit 39; fi; printf sandbox-ok",
        read = quote(&readable),
        write = quote(&writable),
        out = quote(&outside),
        file = quote(&protected_file),
    );
    success(&fixture.start("files", Path::new("/bin/sh"), &["-c", &script], &json!({
        "enabled":true, "read_paths":[readable], "write_paths":[writable], "protected_paths":[protected,protected_file]
    })));
    let output = fixture.execute("files", Path::new("/bin/sh"), &["-c", &script]);
    if unavailable_is_expected(&output) {
        assert!(!writable.join("output").exists());
        return;
    }
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("sandbox-ok"));
    assert_eq!(
        fs::read_to_string(writable.join("output")).unwrap(),
        "allowed"
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("source")).unwrap(),
        "project-read"
    );
    assert_eq!(
        fs::read_to_string(&protected_file).unwrap(),
        "protected-file-data"
    );
    assert_eq!(
        fs::read_to_string(protected.join("private")).unwrap(),
        "protected-data"
    );
    assert!(!outside.join("new").exists());
}

#[test]
fn sandbox_network_denies_tcp_and_unix_sockets_and_allows_explicit_network() {
    let fixture = Fixture::new();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = fixture.project.join("listener.sock");
    let _unix = UnixListener::bind(&socket).unwrap();
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let args = ["--exact", "sandbox_probe_child", "--nocapture"];
    for mode in ["deny", "allow"] {
        fs::write(
            fixture.project.join("probe.json"),
            serde_json::to_vec(&json!({
                "tcp":tcp.local_addr().unwrap().to_string(), "unix":socket, "allow":mode=="allow",
                "protected_socket":fixture.runtime.join("service.sock")
            }))
            .unwrap(),
        )
        .unwrap();
        success(&fixture.start(
            mode,
            &executable,
            &args,
            &json!({"enabled":true,"network":mode,"read_paths":[executable],"write_paths":[fixture.root]}),
        ));
        let output = fixture.execute(mode, &executable, &args);
        if unavailable_is_expected(&output) {
            return;
        }
        success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("network-probe-ok"));
    }
}

#[test]
fn sandbox_probe_child() {
    if std::env::var("TEST_SECRET").as_deref() != Ok("latchrun-fake-network-probe") {
        return;
    }
    let probe: Value = serde_json::from_slice(&fs::read("probe.json").unwrap()).unwrap();
    let allow = probe["allow"].as_bool().unwrap();
    let tcp = TcpStream::connect_timeout(
        &probe["tcp"].as_str().unwrap().parse().unwrap(),
        Duration::from_secs(1),
    );
    assert_eq!(tcp.is_ok(), allow, "TCP policy mismatch");
    let unix = UnixStream::connect(probe["unix"].as_str().unwrap());
    assert_eq!(unix.is_ok(), allow, "Unix socket policy mismatch");
    assert!(
        UnixStream::connect(probe["protected_socket"].as_str().unwrap()).is_err(),
        "sandbox must not connect back to its service socket"
    );
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("/usr/bin/unshare")
            .args(["--user", "--map-root-user", "/bin/true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "nested user namespaces must remain disabled"
        );
    }
    println!("network-probe-ok");
}

#[test]
fn sandbox_rejects_implicit_protection_and_missing_or_relative_roots() {
    let fixture = Fixture::new();
    for policy in [
        json!({"protected_paths":[fixture.project]}),
        json!({"enabled":true,"read_paths":["relative"]}),
        json!({"enabled":true,"write_paths":[fixture.root.join("missing")]}),
        json!({"enabled":true,"read_paths":["/"]}),
        json!({"enabled":true,"protected_paths":[fixture.project]}),
    ] {
        let output = fixture.start(
            "invalid",
            Path::new("/bin/echo"),
            &["should-never-run"],
            &policy,
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_sandbox"));
    }
    let path = fixture.root.join("runtime-project.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "project": fixture.runtime, "purpose":"reject runtime as project", "provider":"fake",
            "commands":[{"executable":"/bin/echo","args":["should-never-run"]}],
            "sandbox":{"enabled":true}
        }))
        .unwrap(),
    )
    .unwrap();
    let output = fixture.run(&[
        "session",
        "start",
        "runtime-project",
        "--profile",
        path.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_sandbox"));
}
