#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::too_many_lines
)]

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Dashboard {
    runtime: PathBuf,
    service: Child,
    server: Option<Child>,
    authority: String,
    token: String,
}

impl Dashboard {
    fn start() -> Self {
        let runtime = PathBuf::from(format!(
            "/tmp/lrd-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let service = command(&runtime)
            .args(["service", "serve"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut guard = Self {
            runtime,
            service,
            server: None,
            authority: String::new(),
            token: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !guard.cli(&["service", "status"]).status.success() {
            assert!(Instant::now() < deadline, "service startup timed out");
            thread::sleep(Duration::from_millis(20));
        }
        let mut child = command(&guard.runtime)
            .args(["dashboard", "serve"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        guard.server = Some(child);
        let (send, receive) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(output).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        let line = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("dashboard startup timed out")
            .unwrap();
        let data: Value = serde_json::from_str(&line).expect("dashboard startup JSON");
        let url = data["url"].as_str().expect("dashboard URL");
        let (origin, token) = url.split_once("/#token=").expect("fragment capability");
        origin
            .strip_prefix("http://")
            .expect("HTTP loopback URL")
            .clone_into(&mut guard.authority);
        assert!(guard.authority.starts_with("127.0.0.1:"));
        assert_eq!(token.len(), 64);
        token.clone_into(&mut guard.token);
        guard
    }

    fn cli(&self, args: &[&str]) -> Output {
        command(&self.runtime).args(args).output().unwrap()
    }

    fn raw(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(&self.authority).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut result = String::new();
        let _ = stream.read_to_string(&mut result);
        result
    }

    fn get(&self, path: &str, authenticated: bool) -> String {
        let auth = if authenticated {
            format!("Authorization: Bearer {}\r\n", self.token)
        } else {
            String::new()
        };
        self.raw(&format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{auth}\r\n",
            self.authority
        ))
    }

    fn post(&self, path: &str, origin: Option<&str>, body: &str) -> String {
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        self.raw(&format!("POST {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",self.authority,self.token,body.len()))
    }

    fn session(&self) {
        let profile = self.runtime.join("fake.json");
        fs::write(&profile,json!({"project":self.runtime,"purpose":"dashboard security fixture","provider":"fake","credentials":{"TEST_SECRET":"fake://dashboard"},"commands":[{"executable":"/usr/bin/printenv","args":["TEST_SECRET"]}]}).to_string()).unwrap();
        assert!(
            self.cli(&[
                "session",
                "start",
                "dashboard-test",
                "--profile",
                profile.to_str().unwrap()
            ])
            .status
            .success()
        );
        let run = self.cli(&[
            "run",
            "dashboard-test",
            "--operation",
            "dashboard-op",
            "--",
            "/usr/bin/printenv",
            "TEST_SECRET",
        ]);
        assert!(run.status.success());
        assert!(!String::from_utf8_lossy(&run.stdout).contains("latchrun-fake-dashboard"));
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        if let Some(child) = self.server.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.cli(&["service", "stop"]);
        let _ = self.service.kill();
        let _ = self.service.wait();
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

fn command(runtime: &PathBuf) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
    command
        .arg("--runtime-dir")
        .arg(runtime)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn status(response: &str, expected: u16) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {expected} ")),
        "unexpected HTTP status (payload withheld)"
    );
}

fn json_body(response: &str) -> Value {
    serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
        .expect("JSON response")
}

#[test]
fn dashboard_authenticates_metadata_and_never_exposes_credential_values() {
    let dashboard = Dashboard::start();
    dashboard.session();
    let html = dashboard.get("/", false);
    status(&html, 200);
    for header in [
        "Cache-Control: no-store",
        "Referrer-Policy: no-referrer",
        "X-Content-Type-Options: nosniff",
        "X-Frame-Options: DENY",
        "frame-ancestors 'none'",
        "script-src 'nonce-",
    ] {
        assert!(html.contains(header));
    }
    assert!(!html.contains("unsafe-inline"));
    assert!(!html.contains("unsafe-eval"));
    assert!(!html.contains(&dashboard.token));
    for path in [
        "/api/status",
        "/api/events",
        "/api/inspect?session=dashboard-test",
    ] {
        let rejected = dashboard.get(path, false);
        status(&rejected, 401);
        assert!(!rejected.contains("dashboard-test"));
        let accepted = dashboard.get(path, true);
        status(&accepted, 200);
        assert!(!accepted.contains("latchrun-fake-dashboard"));
        assert!(!accepted.contains("fake://dashboard"));
        assert!(!accepted.contains("dashboard security fixture"));
    }
    let response = dashboard.get("/api/status", true);
    let data = json_body(&response);
    assert_eq!(data["service"]["statistics"]["succeeded"], 1);
    assert_eq!(data["sessions"][0]["operations"][0]["id"], "dashboard-op");
    let response = dashboard.get("/api/inspect?session=dashboard-test", true);
    let data = json_body(&response);
    assert!(
        data["environment"]
            .as_array()
            .unwrap()
            .iter()
            .any(|variable| variable["name"] == "TEST_SECRET")
    );
    for path in [
        "/api/output",
        "/api/secrets",
        "/../Cargo.toml",
        "/api/status?token=ignored",
    ] {
        status(&dashboard.get(path, true), 404);
    }
    status(
        &dashboard.get(&format!("/api/status?token={}", dashboard.token), false),
        401,
    );
}

#[test]
fn dashboard_rejects_rebinding_cross_origin_and_csrf_mutations() {
    let dashboard = Dashboard::start();
    dashboard.session();
    for host in ["localhost", "attacker.example", "127.0.0.1"] {
        status(
            &dashboard.raw(&format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n")),
            403,
        );
    }
    for header in [
        "Origin: https://attacker.example",
        "Origin: null",
        "Sec-Fetch-Site: cross-site",
        "Sec-Fetch-Site: same-site",
    ] {
        status(&dashboard.raw(&format!("GET /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n{header}\r\n\r\n",dashboard.authority,dashboard.token)),403);
    }
    let body = r#"{"session":"dashboard-test"}"#;
    status(&dashboard.post("/api/session/stop", None, body), 403);
    status(
        &dashboard.post("/api/session/stop", Some("http://attacker.example"), body),
        403,
    );
    let origin = format!("http://{}", dashboard.authority);
    status(
        &dashboard.post("/api/session/refresh", Some(&origin), body),
        200,
    );
    let before = dashboard.cli(&["session", "status", "dashboard-test"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&before.stdout).unwrap()["status"],
        "active"
    );
    status(
        &dashboard.post("/api/session/stop", Some(&origin), body),
        200,
    );
    let after = dashboard.cli(&["session", "status", "dashboard-test"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&after.stdout).unwrap()["status"],
        "stopped"
    );
    status(
        &dashboard.post(
            "/api/session/stop",
            Some(&origin),
            r#"{"session":"dashboard-test","unknown":"sensitive-malformed-input"}"#,
        ),
        400,
    );
    let wrong_type=dashboard.raw(&format!("POST /api/session/stop HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nOrigin: {origin}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",dashboard.authority,dashboard.token,body.len()));
    status(&wrong_type, 403);
}

#[test]
fn dashboard_bounds_requests_and_rejects_ambiguous_http() {
    let dashboard = Dashboard::start();
    for extra in [
        "Host: attacker.example\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Content-Length: 0\r\nContent-Length: 0\r\n",
        "Expect: 100-continue\r\n",
        "Bad Header: sensitive-malformed-input\r\n",
    ] {
        let response = dashboard.raw(&format!(
            "GET / HTTP/1.1\r\nHost: {}\r\n{extra}\r\n",
            dashboard.authority
        ));
        status(&response, 400);
        assert!(!response.contains("sensitive-malformed-input"));
    }
    status(
        &dashboard.raw(&format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Length: 4097\r\n\r\n",
            dashboard.authority
        )),
        413,
    );
    status(
        &dashboard.raw(&format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nX-Padding: {}\r\n\r\n",
            dashboard.authority,
            "x".repeat(17000)
        )),
        413,
    );
    status(
        &dashboard.get("/api/inspect?session=dashboard-test&token=anything", true),
        400,
    );
    let mut fragmented = TcpStream::connect(&dashboard.authority).unwrap();
    fragmented
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    fragmented.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    thread::sleep(Duration::from_millis(100));
    write!(fragmented, "Host: {}\r\n\r\n", dashboard.authority).unwrap();
    let mut response = String::new();
    fragmented.read_to_string(&mut response).unwrap();
    status(&response, 200);
    let started = Instant::now();
    status(
        &dashboard.raw(&format!(
            "GET / HTTP/1.1\r\nHost: {}\r\n",
            dashboard.authority
        )),
        400,
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(started.elapsed() < Duration::from_secs(5));
    status(&dashboard.get("/", false), 200);
}

#[test]
fn dashboard_terminates_cleanly_on_sigterm() {
    let mut dashboard = Dashboard::start();
    let child = dashboard.server.as_mut().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(TcpStream::connect(&dashboard.authority).is_err());
    assert!(dashboard.cli(&["service", "status"]).status.success());
}
