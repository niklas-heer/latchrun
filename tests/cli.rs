#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use nix::unistd::{User, getuid};
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    os::unix::{
        fs::PermissionsExt,
        fs::symlink,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FAKE_SECRET: &str = "latchrun-fake-test";
const REDACTED: &str = "[REDACTED]";
const READY_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(4);

static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(0);

struct Service {
    runtime_dir: PathBuf,
    child: Option<Child>,
}

struct RuntimeGuard {
    path: PathBuf,
}

impl RuntimeGuard {
    fn private() -> Self {
        let path = unique_runtime_dir();
        fs::create_dir(&path).expect("create guarded runtime directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("secure guarded runtime directory");
        Self { path }
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let _ = invoke_at(&self.path, ["service", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.path.join("service.sock").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let _ = fs::remove_file(&self.path);
            }
            Ok(_) => {
                let _ = fs::remove_dir_all(&self.path);
            }
            Err(_) => {}
        }
    }
}

impl Service {
    fn start() -> Self {
        let runtime_dir = unique_runtime_dir();
        fs::create_dir(&runtime_dir).expect("create isolated runtime directory");
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
            .expect("secure isolated runtime directory");
        let child = start_foreground_service(&runtime_dir);

        Self {
            runtime_dir,
            child: Some(child),
        }
    }

    fn invoke<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        invoke_at(&self.runtime_dir, args)
    }

    fn spawn<I, S>(&self, args: I) -> Child
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = command_at(&self.runtime_dir);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn latchrun client")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.runtime_dir.join(name)
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("service is running").id()
    }

    fn crash(&mut self) {
        let mut child = self.child.take().expect("service is running");
        kill_process_group(child.id());
        child.wait().expect("reap crashed service");
    }

    fn restart(&mut self) {
        assert!(self.child.is_none(), "service is already running");
        self.child = Some(start_foreground_service(&self.runtime_dir));
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = invoke_at(&self.runtime_dir, ["service", "stop"]);
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Ok(Some(_)) | Err(_) => break,
                }
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

fn start_foreground_service(runtime_dir: &Path) -> Child {
    let mut child = clean_command()
        .arg("--runtime-dir")
        .arg(runtime_dir)
        .args(["service", "serve"])
        .env("LATCHRUN_TEST_AMBIENT", "ambient-should-not-leak")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("start foreground service");

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("inspect foreground service") {
            let output = child
                .wait_with_output()
                .expect("collect failed service output");
            panic!(
                "service exited before becoming ready: {status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let status = invoke_at(runtime_dir, ["service", "status"]);
        if status.status.success() {
            return child;
        }
        assert!(Instant::now() < deadline, "service did not become ready");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn help_version_and_invalid_cli_are_stable() {
    let help = invoke(["--help"]);
    assert_success(&help, "--help");
    let help_text = stdout(&help);
    for command in ["service", "session", "run", "inspect", "events"] {
        assert!(
            help_text.contains(command),
            "help omitted {command}: {help_text}"
        );
    }
    assert_no_secret(&help);

    let version = invoke(["--version"]);
    assert_success(&version, "--version");
    assert_eq!(
        stdout(&version).trim(),
        concat!("latchrun ", env!("CARGO_PKG_VERSION"))
    );

    let invalid = invoke(["definitely-not-a-command"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(!stderr(&invalid).is_empty());
    assert_no_secret(&invalid);
}

#[test]
fn session_lifecycle_reuses_a_snapshot_and_exposes_only_safe_metadata() {
    let service = Service::start();
    assert_private(&service.runtime_dir);
    assert_private(&service.path("service.sock"));

    let profile_path = service.path("profile.json");
    let script = "test \"$TEST_SECRET\" = \"latchrun-fake-test\" && printf credential-ok";
    write_profile(&profile_path, &service.runtime_dir, script, 30, 3_600);

    let started = service.invoke([
        "session",
        "start",
        "integration",
        "--profile",
        profile_path.to_str().expect("utf-8 profile path"),
    ]);
    assert_success(&started, "session start");
    let started_json = stdout(&started);
    let session = json_string_field(&started_json, "session")
        .or_else(|| json_string_field(&started_json, "id"))
        .expect("session start returns a session identifier");
    assert!(
        started_json.contains("active"),
        "unexpected start response: {started_json}"
    );
    assert_no_secret(&started);

    let status = service.invoke(["session", "status", &session]);
    assert_success(&status, "session status");
    assert!(stdout(&status).contains(&session));
    assert!(stdout(&status).contains("active"));

    let reconnect = service.invoke(["session", "reconnect", &session]);
    assert_success(&reconnect, "session reconnect");
    assert!(stdout(&reconnect).contains(&session));
    assert!(stdout(&reconnect).contains("active"));

    let sessions = service.invoke(["session", "status"]);
    assert_success(&sessions, "session list");
    assert!(stdout(&sessions).contains(&session));
    assert_no_secret(&sessions);

    let first = service.invoke([
        "run",
        &session,
        "--operation",
        "credential-check-1",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    assert_success(&first, "first authorized run");
    assert_eq!(stdout(&first), "credential-ok");
    assert_no_secret(&first);

    // The session owns an immutable profile snapshot. Replacing the source file
    // must not change the policy or credentials of an already active session.
    write_profile(
        &profile_path,
        &service.runtime_dir,
        "printf changed-profile",
        30,
        3_600,
    );
    let second = service.invoke([
        "run",
        &session,
        "--operation",
        "credential-check-2",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    assert_success(&second, "run from snapshotted profile");
    assert_eq!(stdout(&second), "credential-ok");
    assert_no_secret(&second);

    let generated = service.invoke(["run", &session, "--", "/bin/sh", "-c", script]);
    assert_success(&generated, "run with generated operation id");
    assert_eq!(stdout(&generated), "credential-ok");
    assert!(
        stderr(&generated).contains("operation"),
        "generated operation id was not reported on stderr"
    );
    assert_no_secret(&generated);

    let denied_marker = "private-denied-argument";
    let denied = service.invoke([
        "run",
        &session,
        "--operation",
        "denied-operation",
        "--",
        "/bin/sh",
        "-c",
        denied_marker,
    ]);
    assert!(!denied.status.success());
    assert!(!stdout(&denied).contains(denied_marker));
    assert!(!stderr(&denied).contains(denied_marker));
    assert_no_secret(&denied);

    let inspect = service.invoke(["inspect", &session]);
    assert_success(&inspect, "inspect");
    let inspect_json = stdout(&inspect);
    assert!(inspect_json.contains("TEST_SECRET"));
    assert!(inspect_json.contains("fake"));
    assert!(inspect_json.contains("declared"));
    assert!(inspect_json.contains("resolved_per_operation"));
    assert!(inspect_json.contains("\"values_available\":false"));
    assert!(inspect_json.contains("\"HOME\""));
    assert!(inspect_json.contains(os_user_home().to_str().expect("utf-8 home")));
    assert!(!inspect_json.contains(FAKE_SECRET));
    assert!(!inspect_json.contains("fake://test"));
    assert!(!inspect_json.contains(script));

    let events = service.invoke(["events", &session]);
    assert_success(&events, "events");
    assert!(stdout(&events).contains(&session));
    assert!(stdout(&events).contains("credential-check-1"));
    assert_no_secret(&events);

    let stopped = service.invoke(["session", "stop", &session]);
    assert_success(&stopped, "session stop");
    assert!(stdout(&stopped).contains("stopped"));

    let after_stop = service.invoke([
        "run",
        &session,
        "--operation",
        "after-stop",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    assert!(!after_stop.status.success());
    assert_no_secret(&after_stop);
}

#[test]
fn approved_commands_receive_home_without_ambient_environment() {
    let service = Service::start();
    let profile_path = service.path("home.json");
    let script = "test -z \"$LATCHRUN_TEST_AMBIENT\" && printf '%s\\n' \"$HOME\"";
    write_profile(&profile_path, &service.runtime_dir, script, 30, 3_600);
    let session = start_session(&service, "home", &profile_path);

    let output = service.invoke([
        "run",
        &session,
        "--operation",
        "home-environment",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    assert_success(&output, "approved command HOME");
    assert_eq!(
        stdout(&output).trim(),
        os_user_home().to_str().expect("utf-8 home")
    );
}

fn os_user_home() -> PathBuf {
    User::from_uid(getuid())
        .expect("lookup test user")
        .expect("test user")
        .dir
}

#[test]
fn child_output_is_redacted_and_exit_status_is_preserved() {
    let service = Service::start();
    let profile_path = service.path("redaction.json");
    let script =
        "printf 'out:%s\\n' \"$TEST_SECRET\"; printf 'err:%s\\n' \"$TEST_SECRET\" >&2; exit 7";
    write_profile(&profile_path, &service.runtime_dir, script, 30, 3_600);
    let session = start_session(&service, "redaction", &profile_path);

    let output = service.invoke([
        "run",
        &session,
        "--operation",
        "redacted-exit",
        "--",
        "/bin/sh",
        "-c",
        script,
    ]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(stdout(&output), format!("out:{REDACTED}\n"));
    assert_eq!(stderr(&output), format!("err:{REDACTED}\n"));
    assert_no_secret(&output);

    let status = service.invoke(["session", "status", &session]);
    assert_success(&status, "status after child failure");
    let json = stdout(&status);
    assert!(json.contains("redacted-exit"));
    assert!(json.contains("failed"));
    assert!(json.contains('7'));
    assert_no_secret(&status);
}

#[test]
fn duplicate_operation_after_a_lost_response_is_not_replayed() {
    let service = Service::start();
    let marker = service.path("side-effect.txt");
    let script = format!(
        "printf x >> {}; sleep 0.25",
        shell_single_quote(marker.to_str().expect("utf-8 marker path"))
    );
    let profile_path = service.path("lost-response.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 3_600);
    let session = start_session(&service, "lost-response", &profile_path);

    let mut first = service.spawn([
        "run",
        &session,
        "--operation",
        "only-once",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    wait_for_path(&marker, COMMAND_TIMEOUT);
    first.kill().expect("disconnect first client");
    let _ = first.wait();

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        let status = service.invoke(["session", "status", &session]);
        let text = stdout(&status);
        if status.status.success() && text.contains("only-once") && text.contains("succeeded") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "operation did not finish after disconnect: {text}"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let duplicate = service.invoke([
        "run",
        &session,
        "--operation",
        "only-once",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert!(!duplicate.status.success());
    assert!(stderr(&duplicate).contains("duplicate_operation"));
    assert_eq!(fs::read_to_string(&marker).expect("read side effect"), "x");
    assert_no_secret(&duplicate);
}

#[test]
fn timeout_kills_the_child_and_expired_sessions_deny_access() {
    let service = Service::start();
    let marker = service.path("child.pid");
    let script = format!(
        "printf '%s' \"$$\" > {}; exec sleep 30",
        shell_single_quote(marker.to_str().expect("utf-8 marker path"))
    );
    let timeout_profile = service.path("timeout.json");
    write_profile(&timeout_profile, &service.runtime_dir, &script, 1, 3_600);
    let timeout_session = start_session(&service, "timeout", &timeout_profile);

    let started_at = Instant::now();
    let timed_out = service.invoke([
        "run",
        &timeout_session,
        "--operation",
        "timeout-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert!(!timed_out.status.success());
    assert!(started_at.elapsed() < COMMAND_TIMEOUT);
    wait_for_path(&marker, Duration::from_secs(1));
    let pid = fs::read_to_string(&marker)
        .expect("read child pid")
        .parse::<u32>()
        .expect("child pid is numeric");
    assert_process_gone(pid, Duration::from_secs(1));
    assert_no_secret(&timed_out);

    let expiry_script = "printf should-not-run-after-expiry";
    let expiry_profile = service.path("expiry.json");
    write_profile(&expiry_profile, &service.runtime_dir, expiry_script, 30, 1);
    let expired_session = start_session(&service, "expiry", &expiry_profile);
    thread::sleep(Duration::from_millis(1_100));
    let expired = service.invoke([
        "run",
        &expired_session,
        "--operation",
        "too-late",
        "--",
        "/bin/sh",
        "-c",
        expiry_script,
    ]);
    assert!(!expired.status.success());
    assert!(!stdout(&expired).contains("should-not-run-after-expiry"));
    let status = service.invoke(["session", "status", &expired_session]);
    assert_success(&status, "expired session status");
    assert!(stdout(&status).contains("expired"));
    assert_no_secret(&expired);
}

#[test]
fn ttl_expiry_terminates_an_active_child() {
    let service = Service::start();
    let marker = service.path("ttl-active-child.pid");
    let script = format!(
        "printf '%s' \"$$\" > {}; exec sleep 30",
        shell_single_quote(marker.to_str().expect("utf-8 marker path"))
    );
    let profile_path = service.path("ttl-active.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 1);
    let session = start_session(&service, "ttl-active", &profile_path);
    let mut client = service.spawn([
        "run",
        &session,
        "--operation",
        "ttl-active-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    wait_for_path(&marker, COMMAND_TIMEOUT);
    let pid = read_pid(&marker);

    assert!(!wait_for_child(&mut client, COMMAND_TIMEOUT).success());
    assert_process_gone(pid, Duration::from_secs(1));
    let status = service.invoke(["session", "status", &session]);
    assert_success(&status, "status after active TTL expiry");
    assert!(stdout(&status).contains("expired"));
}

#[test]
fn sigterm_is_forwarded_and_the_child_is_reaped() {
    let service = Service::start();
    let marker = service.path("signal-child.pid");
    let script = format!(
        "printf '%s' \"$$\" > {}; exec sleep 30",
        shell_single_quote(marker.to_str().expect("utf-8 marker path"))
    );
    let profile_path = service.path("signal.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 3_600);
    let session = start_session(&service, "signal", &profile_path);
    let mut client = service.spawn([
        "run",
        &session,
        "--operation",
        "signal-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    wait_for_path(&marker, COMMAND_TIMEOUT);
    send_signal(client.id(), "-TERM");

    let client_status = wait_for_child(&mut client, COMMAND_TIMEOUT);
    assert_eq!(client_status.code(), Some(143));
    let pid = read_pid(&marker);
    assert_process_gone(pid, Duration::from_secs(1));

    let status = service.invoke(["session", "status", &session]);
    assert_success(&status, "status after forwarded signal");
    let json = stdout(&status);
    assert!(json.contains("signal-operation"));
    assert!(json.contains("failed"));
    assert!(json.contains("143"));
}

#[test]
fn daemon_crash_cleans_up_children_and_recovers_unknown_outcomes() {
    let mut service = Service::start();
    let pid_marker = service.path("crash-child.pid");
    let guardian_marker = service.path("crash-guardian.pid");
    let effect_marker = service.path("crash-effect.txt");
    let script = format!(
        "printf x >> {}; printf '%s' \"$$\" > {}; printf '%s' \"$PPID\" > {}; exec sleep 30",
        shell_single_quote(effect_marker.to_str().expect("utf-8 effect path")),
        shell_single_quote(pid_marker.to_str().expect("utf-8 pid path")),
        shell_single_quote(guardian_marker.to_str().expect("utf-8 guardian path")),
    );
    let profile_path = service.path("crash.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 3_600);
    let session = start_session(&service, "crash", &profile_path);
    let mut client = service.spawn([
        "run",
        &session,
        "--operation",
        "crash-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    wait_for_path(&pid_marker, COMMAND_TIMEOUT);
    wait_for_path(&guardian_marker, COMMAND_TIMEOUT);
    let pid = read_pid(&pid_marker);
    let guardian_pid = read_pid(&guardian_marker);
    assert_eq!(process_group_of(guardian_pid), guardian_pid);
    assert_ne!(process_group_of(guardian_pid), service.pid());

    service.crash();
    assert!(!wait_for_child(&mut client, COMMAND_TIMEOUT).success());
    assert_process_gone(pid, Duration::from_secs(1));
    assert_process_gone(guardian_pid, Duration::from_secs(1));
    assert_eq!(
        fs::read_to_string(&effect_marker).expect("read crash side effect"),
        "x"
    );

    // A hard crash leaves the socket path behind. Restart must remove that stale
    // socket, while keeping the lost operation's outcome explicitly unknown.
    service.restart();
    let previous = service.invoke(["session", "status", &session]);
    assert_success(&previous, "recover session metadata");
    assert!(stdout(&previous).contains("interrupted"));
    assert!(stdout(&previous).contains("unknown"));
    let retry = service.invoke([
        "run",
        &session,
        "--operation",
        "crash-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert!(!retry.status.success());
    assert!(stderr(&retry).contains("duplicate_operation"));
    assert_eq!(
        fs::read_to_string(&effect_marker).expect("read crash side effect after retry"),
        "x"
    );
}

#[test]
fn one_password_adapter_uses_exact_args_without_ambient_environment_or_diagnostics() {
    let service = Service::start();
    let op_path = service.path("fake-op");
    write_executable(
        &op_path,
        concat!(
            "#!/bin/sh\n",
            "test -z \"$LATCHRUN_TEST_AMBIENT\" || exit 61\n",
            "test \"$#\" -eq 3 || exit 62\n",
            "test \"$1\" = read || exit 63\n",
            "test \"$2\" = --no-newline || exit 64\n",
            "test \"$3\" = op://test/item/value || exit 65\n",
            "printf op-fixture-secret\n",
        ),
    );
    let child_script = "printf '%s' \"$TEST_SECRET\"";
    let profile_path = service.path("op-success.json");
    write_one_password_profile(&profile_path, &service.runtime_dir, &op_path, child_script);
    let session = start_session(&service, "op-success", &profile_path);
    let success = service.invoke([
        "run",
        &session,
        "--operation",
        "op-provider-success",
        "--",
        "/bin/sh",
        "-c",
        child_script,
    ]);
    assert_success(&success, "fake op provider success");
    assert_eq!(stdout(&success), REDACTED);
    assert!(!stdout(&success).contains("op-fixture-secret"));
    assert!(!stderr(&success).contains("op://test/item/value"));

    let failure_diagnostic = "provider-private-diagnostic";
    let failing_op = service.path("failing-op");
    write_executable(
        &failing_op,
        &format!("#!/bin/sh\nprintf '{failure_diagnostic}\\n' >&2\nexit 70\n"),
    );
    let failure_profile = service.path("op-failure.json");
    write_one_password_profile(
        &failure_profile,
        &service.runtime_dir,
        &failing_op,
        child_script,
    );
    let failure_session = start_session(&service, "op-failure", &failure_profile);
    let failure = service.invoke([
        "run",
        &failure_session,
        "--operation",
        "op-provider-failure",
        "--",
        "/bin/sh",
        "-c",
        child_script,
    ]);
    assert!(!failure.status.success());
    assert!(stderr(&failure).contains("provider_unavailable"));
    assert!(!stdout(&failure).contains(failure_diagnostic));
    assert!(!stderr(&failure).contains(failure_diagnostic));
    assert!(!stderr(&failure).contains("op://test/item/value"));
    assert!(!stdout(&failure).contains("op-fixture-secret"));
}

#[test]
fn stopping_a_session_kills_a_stalled_provider_before_the_child_starts() {
    let service = Service::start();
    let provider_marker = service.path("stalled-provider.pid");
    let child_marker = service.path("provider-child-must-not-start");
    let op_path = service.path("stalled-fake-op");
    write_executable(
        &op_path,
        &format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > {}\nexec sleep 30\n",
            shell_single_quote(
                provider_marker
                    .to_str()
                    .expect("utf-8 provider marker path")
            )
        ),
    );
    let child_script = format!(
        "printf child-started > {}",
        shell_single_quote(child_marker.to_str().expect("utf-8 child marker path"))
    );
    let profile_path = service.path("stalled-provider.json");
    write_one_password_profile(&profile_path, &service.runtime_dir, &op_path, &child_script);
    let session = start_session(&service, "stalled-provider", &profile_path);
    let mut client = service.spawn([
        "run",
        &session,
        "--operation",
        "stalled-provider-operation",
        "--",
        "/bin/sh",
        "-c",
        &child_script,
    ]);
    wait_for_path(&provider_marker, COMMAND_TIMEOUT);
    let provider_pid = read_pid(&provider_marker);

    let stopped = service.invoke(["session", "stop", &session]);
    assert_success(&stopped, "stop session with stalled provider");
    assert!(!wait_for_child(&mut client, COMMAND_TIMEOUT).success());
    assert_process_gone(provider_pid, Duration::from_secs(1));
    assert!(!child_marker.exists(), "approved child started after stop");
}

#[test]
fn declared_ssh_agent_socket_reaches_the_child_without_private_key_material() {
    let service = Service::start();
    let socket_path = service.path("fake-ssh-agent.sock");
    let _listener = UnixListener::bind(&socket_path).expect("bind fake SSH agent socket");
    let canonical_socket = fs::canonicalize(&socket_path).expect("canonicalize fake SSH socket");
    let quoted_socket = shell_single_quote(
        canonical_socket
            .to_str()
            .expect("utf-8 canonical SSH socket path"),
    );
    let script = format!("test \"$SSH_AUTH_SOCK\" = {quoted_socket} && printf ssh-socket-ok");
    let profile_path = service.path("ssh-agent.json");
    write_ssh_profile(&profile_path, &service.runtime_dir, &socket_path, &script);
    let session = start_session(&service, "ssh-agent", &profile_path);
    let output = service.invoke([
        "run",
        &session,
        "--operation",
        "ssh-agent-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_success(&output, "child receives declared SSH_AUTH_SOCK");
    assert_eq!(stdout(&output), "ssh-socket-ok");
    assert_no_secret(&output);
}

#[test]
fn concurrent_callers_with_one_operation_id_execute_exactly_once() {
    let service = Service::start();
    let marker = service.path("concurrent-effect.txt");
    let script = format!(
        "printf x >> {}; sleep 0.15",
        shell_single_quote(marker.to_str().expect("utf-8 marker path"))
    );
    let profile_path = service.path("concurrent.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 3_600);
    let session = start_session(&service, "concurrent", &profile_path);
    let barrier = Arc::new(Barrier::new(3));

    let spawn_client = || {
        let runtime = service.runtime_dir.clone();
        let session = session.clone();
        let script = script.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            invoke_at(
                &runtime,
                [
                    "run",
                    &session,
                    "--operation",
                    "same-operation",
                    "--",
                    "/bin/sh",
                    "-c",
                    &script,
                ],
            )
        })
    };
    let clients = [spawn_client(), spawn_client()];
    barrier.wait();
    let outputs: Vec<Output> = clients
        .into_iter()
        .map(|client| client.join().expect("concurrent client did not panic"))
        .collect();

    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.status.success())
            .count(),
        1
    );
    let rejected = outputs
        .iter()
        .find(|output| !output.status.success())
        .expect("one concurrent request is rejected");
    assert!(stderr(rejected).contains("duplicate_operation"));
    assert_eq!(
        fs::read_to_string(&marker).expect("read concurrent side effect"),
        "x"
    );
}

#[test]
fn service_stop_cleans_up_the_active_process_tree() {
    let service = Service::start();
    let shell_marker = service.path("stop-shell.pid");
    let grandchild_marker = service.path("stop-grandchild.pid");
    let script = format!(
        "sleep 30 & grandchild=$!; printf '%s' \"$$\" > {}; printf '%s' \"$grandchild\" > {}; wait",
        shell_single_quote(shell_marker.to_str().expect("utf-8 shell path")),
        shell_single_quote(grandchild_marker.to_str().expect("utf-8 grandchild path")),
    );
    let profile_path = service.path("stop-tree.json");
    write_profile(&profile_path, &service.runtime_dir, &script, 30, 3_600);
    let session = start_session(&service, "stop-tree", &profile_path);
    let mut client = service.spawn([
        "run",
        &session,
        "--operation",
        "stop-tree-operation",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ]);
    wait_for_path(&shell_marker, COMMAND_TIMEOUT);
    wait_for_path(&grandchild_marker, COMMAND_TIMEOUT);
    let shell_pid = read_pid(&shell_marker);
    let grandchild_pid = read_pid(&grandchild_marker);

    let stopped = service.invoke(["service", "stop"]);
    assert_success(&stopped, "service stop with active process tree");
    let client_status = wait_for_child(&mut client, COMMAND_TIMEOUT);
    assert!(!client_status.success());
    assert_process_gone(shell_pid, Duration::from_secs(1));
    assert_process_gone(grandchild_pid, Duration::from_secs(1));
}

#[test]
fn unsafe_runtime_directories_and_symlinks_are_refused() {
    let world_readable = RuntimeGuard::private();
    fs::set_permissions(&world_readable.path, fs::Permissions::from_mode(0o755))
        .expect("make runtime intentionally unsafe");
    let mode_failure = invoke_at(&world_readable.path, ["service", "status"]);
    assert!(!mode_failure.status.success());
    assert!(stderr(&mode_failure).contains("unsafe_runtime"));
    assert!(!world_readable.path.join("service.sock").exists());

    let target = RuntimeGuard::private();
    let symlink_path = unique_runtime_dir();
    symlink(&target.path, &symlink_path).expect("create runtime symlink fixture");
    let symlink_guard = RuntimeGuard { path: symlink_path };
    let symlink_failure = invoke_at(&symlink_guard.path, ["service", "status"]);
    assert!(!symlink_failure.status.success());
    assert!(stderr(&symlink_failure).contains("unsafe_runtime"));
    assert!(!target.path.join("service.sock").exists());
}

#[test]
fn background_service_start_is_idempotent_and_stops_cleanly() {
    let runtime = RuntimeGuard::private();
    let first = invoke_at(&runtime.path, ["service", "start"]);
    assert_success(&first, "first background service start");
    assert!(stdout(&first).contains("running"));

    let second = invoke_at(&runtime.path, ["service", "start"]);
    assert_success(&second, "idempotent background service start");
    assert!(stdout(&second).contains("running"));
    assert_private(&runtime.path.join("service.sock"));

    let stopped = invoke_at(&runtime.path, ["service", "stop"]);
    assert_success(&stopped, "background service stop");
    assert!(stdout(&stopped).contains("stopping"));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let status = invoke_at(&runtime.path, ["service", "status"]);
        if !status.status.success() {
            assert_no_secret(&status);
            break;
        }
        assert!(Instant::now() < deadline, "background service did not stop");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn malformed_ipc_gets_a_static_bounded_error() {
    let service = Service::start();
    let mut stream = UnixStream::connect(service.path("service.sock")).expect("connect socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set socket timeout");
    stream
        .write_all(b"{ definitely-not-json }\n")
        .expect("write malformed request");

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .expect("read malformed response");
    assert!(
        response.len() < 1_024,
        "malformed response was unexpectedly large"
    );
    assert!(
        response.contains("invalid_request"),
        "unexpected response: {response}"
    );
    assert!(!response.contains("definitely-not-json"));
    assert!(!response.contains(FAKE_SECRET));

    let status = service.invoke(["service", "status"]);
    assert_success(&status, "service remains healthy after malformed IPC");
}

fn start_session(service: &Service, name: &str, profile_path: &Path) -> String {
    let output = service.invoke([
        "session",
        "start",
        name,
        "--profile",
        profile_path.to_str().expect("utf-8 profile path"),
    ]);
    assert_success(&output, "session start");
    let json = stdout(&output);
    json_string_field(&json, "session")
        .or_else(|| json_string_field(&json, "id"))
        .expect("session response contains identifier")
}

fn write_profile(
    profile_path: &Path,
    project: &Path,
    script: &str,
    timeout_seconds: u64,
    ttl_seconds: u64,
) {
    let project = json_quote(project.to_str().expect("utf-8 project path"));
    let script = json_quote(script);
    let profile = format!(
        concat!(
            "{{",
            "\"project\":{project},",
            "\"purpose\":\"test\",",
            "\"ttl_seconds\":{ttl_seconds},",
            "\"provider\":\"fake\",",
            "\"credentials\":{{\"TEST_SECRET\":\"fake://test\"}},",
            "\"commands\":[{{\"executable\":\"/bin/sh\",",
            "\"args\":[\"-c\",{script}]}}],",
            "\"timeout_seconds\":{timeout_seconds}",
            "}}"
        ),
        project = project,
        ttl_seconds = ttl_seconds,
        script = script,
        timeout_seconds = timeout_seconds,
    );
    fs::write(profile_path, profile).expect("write fake-only profile");
}

fn write_one_password_profile(profile_path: &Path, project: &Path, op_path: &Path, script: &str) {
    let project = json_quote(project.to_str().expect("utf-8 project path"));
    let op_path = json_quote(op_path.to_str().expect("utf-8 op path"));
    let script = json_quote(script);
    let profile = format!(
        concat!(
            "{{",
            "\"project\":{project},",
            "\"purpose\":\"test\",",
            "\"ttl_seconds\":3600,",
            "\"provider\":\"one_password\",",
            "\"credentials\":{{\"TEST_SECRET\":\"op://test/item/value\"}},",
            "\"commands\":[{{\"executable\":\"/bin/sh\",",
            "\"args\":[\"-c\",{script}]}}],",
            "\"op_path\":{op_path},",
            "\"timeout_seconds\":30",
            "}}"
        ),
        project = project,
        script = script,
        op_path = op_path,
    );
    fs::write(profile_path, profile).expect("write fake op profile");
}

fn write_ssh_profile(profile_path: &Path, project: &Path, socket_path: &Path, script: &str) {
    let project = json_quote(project.to_str().expect("utf-8 project path"));
    let socket = json_quote(socket_path.to_str().expect("utf-8 SSH socket path"));
    let script = json_quote(script);
    let profile = format!(
        concat!(
            "{{",
            "\"project\":{project},",
            "\"purpose\":\"test\",",
            "\"ttl_seconds\":3600,",
            "\"provider\":\"fake\",",
            "\"credentials\":{{}},",
            "\"commands\":[{{\"executable\":\"/bin/sh\",",
            "\"args\":[\"-c\",{script}]}}],",
            "\"ssh_auth_sock\":{socket},",
            "\"timeout_seconds\":30",
            "}}"
        ),
        project = project,
        script = script,
        socket = socket,
    );
    fs::write(profile_path, profile).expect("write fake SSH agent profile");
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make fixture executable");
}

fn invoke<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    clean_command()
        .args(args)
        .output()
        .expect("invoke latchrun")
}

fn invoke_at<I, S>(runtime_dir: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    command_at(runtime_dir)
        .args(args)
        .output()
        .expect("invoke latchrun client")
}

fn command_at(runtime_dir: &Path) -> Command {
    let mut command = clean_command();
    command.arg("--runtime-dir").arg(runtime_dir);
    command
}

fn clean_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C");
    command
}

fn unique_runtime_dir() -> PathBuf {
    let counter = NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    // Keep this short: macOS limits the complete Unix-domain socket path.
    env::temp_dir().join(format!("lr-{}-{counter}-{nanos:x}", std::process::id()))
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is utf-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is utf-8")
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn assert_no_secret(output: &Output) {
    assert!(!stdout(output).contains(FAKE_SECRET));
    assert!(!stderr(output).contains(FAKE_SECRET));
    assert!(!stdout(output).contains("fake://test"));
    assert!(!stderr(output).contains("fake://test"));
}

fn assert_private(path: &Path) {
    let mode = fs::metadata(path)
        .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o077,
        0,
        "{} is not private: {mode:o}",
        path.display()
    );
}

fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} was not created",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("inspect child") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child {} did not exit in time", child.id());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        if !text.is_empty() {
            return text.parse().expect("fixture pid is numeric");
        }
        assert!(Instant::now() < deadline, "fixture pid was never written");
        thread::sleep(Duration::from_millis(10));
    }
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .status()
        .expect("send signal");
    assert!(status.success(), "could not send {signal} to {pid}");
}

fn kill_process_group(group: u32) {
    let group = i32::try_from(group).expect("process group fits i32");
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(group),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill service process group");
}

fn process_group_of(pid: u32) -> u32 {
    let output = Command::new("/bin/ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("inspect process group");
    assert_success(&output, "inspect process group");
    stdout(&output)
        .trim()
        .parse()
        .expect("process group is numeric")
}

fn assert_process_gone(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("probe child process");
        if !status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child process {pid} survived timeout"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn json_string_field(json: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let after_key = json.get(json.find(&key)? + key.len()..)?;
    let after_colon = after_key.get(after_key.find(':')? + 1..)?.trim_start();
    let quoted = after_colon.strip_prefix('"')?;
    let mut value = String::new();
    let mut escaped = false;
    for character in quoted.chars() {
        if escaped {
            value.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Some(value);
        } else {
            value.push(character);
        }
    }
    None
}

fn json_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                write!(quoted, "\\u{:04x}", u32::from(character)).expect("write JSON escape");
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
