#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixStream,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Service {
    root: PathBuf,
    project: PathBuf,
    daemon: Child,
}
impl Service {
    fn start() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.with_extension("project");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_latchrun"))
            .env_clear()
            .arg("--runtime-dir")
            .arg(&root)
            .args(["service", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let service = Self {
            root,
            project,
            daemon,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !service
            .cli(&["service", "status"])
            .status()
            .unwrap()
            .success()
        {
            assert!(Instant::now() < deadline, "service startup");
            thread::sleep(Duration::from_millis(10));
        }
        service
    }
    fn cli(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
        command
            .env_clear()
            .arg("--runtime-dir")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    fn socket(&self) -> UnixStream {
        let socket = UnixStream::connect(self.root.join("service.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
    }
    fn request(&self, request: &Value) -> Value {
        let mut socket = self.socket();
        writeln!(socket, "{request}").unwrap();
        read(&mut BufReader::new(socket))
    }
    fn profile(&self, script: &str) -> Value {
        json!({"project":self.project,"purpose":"runtime test","provider":"fake","credentials":{},"commands":[{"executable":"/bin/sh","args":["-c",script]}],"timeout_seconds":5})
    }
    fn start_session(&self, profile: &Value) {
        let response = self.request(&json!({"type":"start","name":"work","profile":profile}));
        assert_eq!(response["type"], "ok", "{response}");
    }
    fn run(&self, id: &str, script: &str, input: &str) -> BufReader<UnixStream> {
        let mut socket = self.socket();
        writeln!(socket,"{}",json!({"type":"run","session":"work","operation":id,"argv":["/bin/sh","-c",script],"input":input})).unwrap();
        let mut reader = BufReader::new(socket);
        let response = read(&mut reader);
        assert_eq!(response["type"], "accepted", "{response}");
        reader
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(mut socket) = UnixStream::connect(self.root.join("service.sock")) {
            let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = writeln!(socket, "{{\"type\":\"shutdown\"}}");
            let mut response = String::new();
            let _ = BufReader::new(socket).read_line(&mut response);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.project);
    }
}
fn read(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|_| panic!("expected JSON response"))
}
fn finish(mut reader: BufReader<UnixStream>) -> (Vec<u8>, Value) {
    let mut output = Vec::new();
    loop {
        let frame = read(&mut reader);
        if frame["type"] == "output" {
            output.extend(
                frame["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap()),
            );
        } else {
            return (output, frame);
        }
    }
}

#[test]
fn pipe_input_and_eof_are_independent_of_guardian_control() {
    let service = Service::start();
    let script = "cat; printf finished";
    service.start_session(&service.profile(script));
    let reader = service.run("pipe", script, "pipe");
    let response=service.request(&json!({"type":"input","session":"work","operation":"pipe","data":b"hello\n".to_vec(),"eof":true}));
    assert_eq!(response["type"], "ok", "{response}");
    let (output, finished) = finish(reader);
    assert_eq!(output, b"hello\nfinished");
    assert_eq!(finished["exit_code"], 0);
}

#[test]
fn tty_allocates_controlling_terminal_accepts_input_and_resizes() {
    let service = Service::start();
    let script =
        "test -t 0 && test -t 1 || exit 9; read answer; stty size; printf 'answer:%s' \"$answer\"";
    service.start_session(&service.profile(script));
    let reader = service.run("tty", script, "tty");
    let resize = service
        .request(&json!({"type":"resize","session":"work","operation":"tty","rows":37,"cols":101}));
    assert_eq!(resize["type"], "ok", "{resize}");
    let response=service.request(&json!({"type":"input","session":"work","operation":"tty","data":b"hello\n".to_vec(),"eof":false}));
    assert_eq!(response["type"], "ok", "{response}");
    let (output, finished) = finish(reader);
    assert_eq!(finished["exit_code"], 0, "{finished}");
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("37 101"), "{output}");
    assert!(output.contains("answer:hello"), "{output}");
}

#[test]
fn private_file_provider_redacts_and_rejects_symlinks_and_public_files() {
    for mode in ["private", "public", "symlink"] {
        let service = Service::start();
        let source = service.root.join("secret");
        fs::write(&source, "fixture-file-secret").unwrap();
        fs::set_permissions(
            &source,
            fs::Permissions::from_mode(if mode == "public" { 0o644 } else { 0o600 }),
        )
        .unwrap();
        let path = if mode == "symlink" {
            let path = service.root.join("link");
            symlink(&source, &path).unwrap();
            path
        } else {
            source
        };
        let script = "printf '%s' \"$TOKEN\"";
        let mut profile = service.profile(script);
        profile["provider"] = json!("file");
        profile["credentials"] = json!({"TOKEN":format!("file://{}",path.display())});
        service.start_session(&profile);
        let (output, response) = finish(service.run("file", script, "null"));
        assert!(!String::from_utf8_lossy(&output).contains("fixture-file-secret"));
        if mode == "private" {
            assert_eq!(output, b"[REDACTED]");
            assert_eq!(response["exit_code"], 0);
        } else {
            assert_eq!(response["type"], "error");
        }
    }
}

#[test]
fn password_store_uses_first_line_and_suppresses_notes() {
    let service = Service::start();
    let executable = service.root.join("fake-pass");
    fs::write(&executable,"#!/bin/sh\n[ \"$1\" = show ] && [ \"$2\" = work/test ] || exit 9\nprintf 'fixture-pass-secret\\nprivate notes\\n'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let script = "printf '%s' \"$TOKEN\"";
    let mut profile = service.profile(script);
    profile["provider"] = json!("password_store");
    profile["provider_path"] = json!(executable);
    profile["credentials"] = json!({"TOKEN":"pass://work/test"});
    service.start_session(&profile);
    let (output, response) = finish(service.run("pass", script, "null"));
    assert_eq!(output, b"[REDACTED]");
    assert_eq!(response["exit_code"], 0);
}

#[test]
fn cache_is_opt_in_and_refreshes_after_expiry() {
    let service = Service::start();
    let source = service.root.join("secret");
    fs::write(&source, "old-fixture").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let script =
        "case \"$TOKEN\" in old-fixture) printf old;; new-fixture) printf new;; *) exit 9;; esac";
    let mut profile = service.profile(script);
    profile["provider"] = json!("file");
    profile["credentials"] = json!({"TOKEN":format!("file://{}",source.display())});
    profile["cache_ttl_seconds"] = json!(1);
    service.start_session(&profile);
    assert_eq!(finish(service.run("cache-first", script, "null")).0, b"old");
    fs::write(&source, "new-fixture").unwrap();
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        finish(service.run("cache-second", script, "null")).0,
        b"old"
    );
    thread::sleep(Duration::from_millis(600));
    assert_eq!(finish(service.run("cache-third", script, "null")).0, b"new");
}

#[test]
fn stopping_a_tty_also_terminates_background_job_groups() {
    let service = Service::start();
    let marker = service.root.join("background-pid");
    let script = format!("set -m; sleep 30 & echo $! > {}; wait", marker.display());
    service.start_session(&service.profile(&script));
    let reader = service.run("tty-stop", &script, "tty");
    let deadline = Instant::now() + Duration::from_secs(3);
    let pid = loop {
        if let Ok(contents) = fs::read_to_string(&marker)
            && let Ok(pid) = contents.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "background job did not start");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(pid > 0);
    let response = service.request(&json!({"type":"stop","session":"work"}));
    assert_eq!(response["type"], "ok", "{response}");
    let (_, finished) = finish(reader);
    assert_eq!(finished["type"], "finished", "{finished}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_running(pid) {
        assert!(
            Instant::now() < deadline,
            "TTY background job survived stop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn process_running(pid: i32) -> bool {
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
        return false;
    }
    #[cfg(target_os = "linux")]
    if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat.contains(") Z ")) {
        return false;
    }
    true
}

#[test]
fn tty_redacts_multiline_credentials_after_terminal_newline_conversion() {
    let service = Service::start();
    let source = service.root.join("multiline-secret");
    fs::write(&source, "first-private-line\nsecond-private-line").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let script = "printf '%s' \"$TOKEN\"";
    let mut profile = service.profile(script);
    profile["provider"] = json!("file");
    profile["credentials"] = json!({"TOKEN":format!("file://{}",source.display())});
    service.start_session(&profile);
    let (output, response) = finish(service.run("multiline", script, "tty"));
    assert_eq!(output, b"[REDACTED]");
    assert_eq!(response["exit_code"], 0);
}

#[test]
fn large_output_is_delivered_completely_over_local_transport() {
    let service = Service::start();
    let script = "head -c 1048576 /dev/zero";
    service.start_session(&service.profile(script));
    let (output, response) = finish(service.run("large", script, "null"));
    assert_eq!(response["exit_code"], 0, "{response}");
    assert_eq!(output.len(), 1_048_576);
    assert!(output.iter().all(|byte| *byte == 0));
}

#[test]
fn cli_stdin_flag_forwards_bytes_and_closes_only_command_input() {
    let service = Service::start();
    let script = "cat; printf done";
    service.start_session(&service.profile(script));
    let mut client = Command::new(env!("CARGO_BIN_EXE_latchrun"))
        .env_clear()
        .arg("--runtime-dir")
        .arg(&service.root)
        .args([
            "run",
            "work",
            "--stdin",
            "--operation",
            "cli-input",
            "--",
            "/bin/sh",
            "-c",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    client
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin-data\n")
        .unwrap();
    let output = client.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"stdin-data\ndone");
}

#[test]
fn sandboxed_tty_preserves_its_controlling_terminal_and_input() {
    let service = Service::start();
    let script = "test -t 0 && stty </dev/tty >/dev/null || exit 9; read answer; printf 'sandbox-tty:%s' \"$answer\"";
    let mut profile = service.profile(script);
    profile["sandbox"] = json!({"enabled":true});
    service.start_session(&profile);
    let reader = service.run("sandbox-tty", script, "tty");
    let _ = service.request(&json!({"type":"input","session":"work","operation":"sandbox-tty","data":b"hello\n".to_vec(),"eof":false}));
    let (output, response) = finish(reader);
    if std::env::var_os("LATCHRUN_TEST_SANDBOX_UNAVAILABLE").is_some() {
        assert_ne!(response["exit_code"], 0);
        assert!(
            String::from_utf8_lossy(&output).contains("bwrap:")
                || response["code"] == "sandbox_unavailable"
        );
    } else {
        assert_eq!(
            response["exit_code"],
            0,
            "{response}: {}",
            String::from_utf8_lossy(&output)
        );
        assert!(String::from_utf8_lossy(&output).contains("sandbox-tty:hello"));
    }
}
