//! Credential resolution and process-group supervision in a short-lived worker.
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::resource::{Resource, setrlimit};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, User, getuid};
use serde::{Deserialize, Serialize};

use crate::protocol::{Failure, Profile, Provider, Response, read_frame, write_frame};

const SECRET_LIMIT: usize = 65_536;
const POLL: Duration = Duration::from_millis(10);
const KILL_GRACE: Duration = Duration::from_millis(200);

pub struct Worker {
    pub child: Child,
    pub control: ChildStdin,
    pub output: ChildStdout,
}

#[derive(Serialize, Deserialize)]
struct WorkerSpec {
    profile: Profile,
    argv: Vec<String>,
}

pub fn spawn(profile: &Profile, argv: &[String]) -> Result<Worker, Failure> {
    let executable = std::env::current_exe().map_err(|_| worker_failure())?;
    let mut child = Command::new(executable)
        .arg("__worker")
        // A daemon process-group kill must leave this guardian alive to observe EOF.
        .process_group(0)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| worker_failure())?;
    let Some(mut control) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(worker_failure());
    };
    let Some(output) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(worker_failure());
    };
    let spec = WorkerSpec {
        profile: profile.clone(),
        argv: argv.to_vec(),
    };
    if write_frame(&mut control, &spec).is_err() {
        drop(control);
        let _ = child.wait();
        return Err(worker_failure());
    }
    Ok(Worker {
        child,
        control,
        output,
    })
}

fn worker_failure() -> Failure {
    Failure::new(
        "execution_failed",
        "Could not start or supervise the approved command.",
    )
}

fn provider_failure() -> Failure {
    Failure::new(
        "provider_unavailable",
        "Credential resolution failed. Unlock 1Password and sign in with its official CLI, then try again.",
    )
}

#[derive(Default)]
struct ProcessState {
    group: Option<Pid>,
    cancellation: Option<(Instant, Signal)>,
}

struct Supervisor {
    state: Arc<Mutex<ProcessState>>,
    done: Arc<AtomicBool>,
}

impl Supervisor {
    fn start(input: BufReader<io::Stdin>, timeout: Duration) -> Result<Self, Failure> {
        let state = Arc::new(Mutex::new(ProcessState::default()));
        let done = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        for signal in [
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGQUIT,
        ] {
            signal_hook::flag::register(signal, Arc::clone(&interrupted))
                .map_err(|_| worker_failure())?;
        }
        let control_state = Arc::clone(&state);
        thread::spawn(move || monitor_control(input, &control_state));
        let watchdog_state = Arc::clone(&state);
        let watchdog_done = Arc::clone(&done);
        thread::spawn(move || {
            let start = Instant::now();
            while !watchdog_done.load(Ordering::Acquire) {
                let mut locked = watchdog_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if interrupted.load(Ordering::Acquire) || start.elapsed() >= timeout {
                    cancel(&mut locked, Signal::SIGTERM);
                }
                if let (Some(group), Some((at, _))) = (locked.group, locked.cancellation)
                    && at.elapsed() >= KILL_GRACE
                {
                    let _ = killpg(group, Signal::SIGKILL);
                }
                drop(locked);
                thread::sleep(POLL);
            }
        });
        Ok(Self { state, done })
    }

    fn spawn(&self, command: &mut Command) -> Result<Child, Failure> {
        // Hold the same lock used by cancellation across spawn and registration.
        let mut state = self.state.lock().map_err(|_| worker_failure())?;
        if state.cancellation.is_some() {
            return Err(Failure::new(
                "cancelled",
                "Command execution was cancelled.",
            ));
        }
        let child = command
            .process_group(0)
            .spawn()
            .map_err(|_| worker_failure())?;
        let raw = i32::try_from(child.id()).map_err(|_| worker_failure())?;
        state.group = Some(Pid::from_raw(raw));
        drop(state);
        Ok(child)
    }

    fn cleanup(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(group) = state.group.take() {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }

    fn cancelled(&self) -> bool {
        self.state
            .lock()
            .map_or(true, |state| state.cancellation.is_some())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cleanup();
        self.done.store(true, Ordering::Release);
    }
}

fn cancel(state: &mut ProcessState, signal: Signal) {
    state
        .cancellation
        .get_or_insert_with(|| (Instant::now(), signal));
    if let Some(group) = state.group {
        let _ = killpg(group, signal);
    }
}

fn monitor_control(mut input: BufReader<io::Stdin>, state: &Mutex<ProcessState>) {
    loop {
        let mut line = String::new();
        let result = input.by_ref().take(32).read_line(&mut line);
        let signal = match result {
            Ok(0) | Err(_) => None,
            Ok(_) if !line.ends_with('\n') => None,
            Ok(_) => Some(match line.trim().parse::<i32>() {
                Ok(1) => Signal::SIGHUP,
                Ok(2) => Signal::SIGINT,
                Ok(3) => Signal::SIGQUIT,
                _ => Signal::SIGTERM,
            }),
        };
        let mut locked = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cancel(&mut locked, signal.unwrap_or(Signal::SIGTERM));
        if signal.is_none() {
            return;
        }
    }
}

pub fn worker_main() -> Result<(), Failure> {
    setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(|_| worker_failure())?;
    let mut input = BufReader::new(io::stdin());
    let mut spec: WorkerSpec = read_frame(&mut input)?;
    let supervisor = Supervisor::start(input, Duration::from_secs(spec.profile.timeout_seconds))?;
    let result = spec
        .profile
        .validate()
        .and_then(|()| spec.profile.authorize(&spec.argv))
        .and_then(|()| execute(&spec, &supervisor));
    supervisor.cleanup();
    match result {
        Ok(exit_code) => write_frame(&mut io::stdout().lock(), &Response::Finished { exit_code }),
        Err(failure) => write_frame(
            &mut io::stdout().lock(),
            &Response::Error {
                code: failure.code,
                message: failure.message,
            },
        ),
    }
}

fn clean_command(executable: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn resolve(
    profile: &Profile,
    supervisor: &Supervisor,
) -> Result<BTreeMap<String, Vec<u8>>, Failure> {
    let mut credentials = BTreeMap::new();
    let mut total: usize = 0;
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
                .ok_or_else(provider_failure)?,
            Provider::OnePassword => read_one_password(profile, reference, supervisor)?,
        };
        total = total.saturating_add(value.len());
        if value.is_empty() || value.contains(&0) || total > SECRET_LIMIT {
            return Err(provider_failure());
        }
        credentials.insert(name.clone(), value);
    }
    Ok(credentials)
}

fn read_one_password(
    profile: &Profile,
    reference: &str,
    supervisor: &Supervisor,
) -> Result<Vec<u8>, Failure> {
    let executable = profile.op_path.as_ref().ok_or_else(provider_failure)?;
    let mut command = clean_command(executable);
    command.args(["read", "--no-newline", reference]);
    let user = User::from_uid(getuid())
        .map_err(|_| provider_failure())?
        .ok_or_else(provider_failure)?;
    command.env("HOME", user.dir);
    let mut child = supervisor
        .spawn(&mut command)
        .map_err(|_| provider_failure())?;
    let output = child.stdout.take().ok_or_else(provider_failure)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = output.take(65_537).read_to_end(&mut bytes);
        let _ = sender.send(result.map(|_| bytes));
    });
    let start = Instant::now();
    let mut status = None;
    let mut bytes = None;
    let result = loop {
        if supervisor.cancelled() || start.elapsed() > Duration::from_secs(30) {
            break Err(provider_failure());
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    supervisor.cleanup();
                    if !exit.success() {
                        break Err(provider_failure());
                    }
                }
                Ok(None) => {}
                Err(_) => break Err(provider_failure()),
            }
        }
        if bytes.is_none() {
            match receiver.try_recv() {
                Ok(Ok(value)) if value.len() <= SECRET_LIMIT => bytes = Some(value),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => break Err(provider_failure()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if status.is_some()
            && let Some(value) = bytes.take()
        {
            break Ok(value);
        }
        thread::sleep(POLL);
    };
    supervisor.cleanup();
    let _ = child.wait();
    result
}

fn execute(spec: &WorkerSpec, supervisor: &Supervisor) -> Result<i32, Failure> {
    let executable = spec.argv.first().ok_or_else(worker_failure)?;
    let credentials = resolve(&spec.profile, supervisor)?;
    let secrets: Arc<Vec<Vec<u8>>> = Arc::new(credentials.values().cloned().collect());
    let mut command = clean_command(executable);
    command
        .args(spec.argv.iter().skip(1))
        .current_dir(&spec.profile.project)
        .stderr(Stdio::piped());
    for (name, value) in &credentials {
        command.env(name, OsStr::from_bytes(value));
    }
    if let Some(socket) = &spec.profile.ssh_auth_sock {
        command.env("SSH_AUTH_SOCK", socket);
    }
    let mut child = supervisor.spawn(&mut command)?;
    let stdout = child.stdout.take().ok_or_else(worker_failure)?;
    let stderr = child.stderr.take().ok_or_else(worker_failure)?;
    let (sender, receiver) = mpsc::sync_channel(16);
    pump(stdout, "stdout", Arc::clone(&secrets), sender.clone());
    pump(stderr, "stderr", secrets, sender.clone());
    drop(sender);
    let result = stream_output(&mut child, &receiver, supervisor);
    supervisor.cleanup();
    let _ = child.wait();
    result
}

fn stream_output(
    child: &mut Child,
    receiver: &mpsc::Receiver<Result<Response, Failure>>,
    supervisor: &Supervisor,
) -> Result<i32, Failure> {
    let mut status = None;
    let mut draining = None;
    loop {
        if status.is_none() {
            status = child.try_wait().map_err(|_| worker_failure())?;
            if status.is_some() {
                supervisor.cleanup();
                draining = Some(Instant::now());
            }
        }
        // Even a descendant that escaped the process group cannot retain our
        // output pipes forever after the approved direct child has exited.
        if draining.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1)) {
            return Err(worker_failure());
        }
        match receiver.recv_timeout(POLL) {
            Ok(frame) => write_frame(&mut io::stdout().lock(), &frame?)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(exit) = status {
                    return Ok(exit
                        .code()
                        .unwrap_or_else(|| 128_i32.saturating_add(exit.signal().unwrap_or(1))));
                }
                thread::sleep(POLL);
            }
        }
    }
}

fn pump(
    reader: impl Read + Send + 'static,
    stream: &'static str,
    secrets: Arc<Vec<Vec<u8>>>,
    sender: mpsc::SyncSender<Result<Response, Failure>>,
) {
    thread::spawn(move || {
        let mut reader = reader;
        let mut redactor = Redactor::new(secrets);
        let mut chunk = [0_u8; 8192];
        loop {
            let size = match reader.read(&mut chunk) {
                Ok(size) => size,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = sender.send(Err(worker_failure()));
                    return;
                }
            };
            let data = redactor.push(&chunk[..size], size == 0);
            for chunk in data.chunks(8192) {
                if sender
                    .send(Ok(Response::Output {
                        stream: stream.to_owned(),
                        data: chunk.to_vec(),
                    }))
                    .is_err()
                {
                    return;
                }
            }
            if size == 0 {
                return;
            }
        }
    });
}

struct Redactor {
    secrets: Arc<Vec<Vec<u8>>>,
    pending: Vec<u8>,
    maximum: usize,
}

impl Redactor {
    fn new(secrets: Arc<Vec<Vec<u8>>>) -> Self {
        let maximum = secrets.iter().map(Vec::len).max().unwrap_or(1).max(1);
        Self {
            secrets,
            pending: Vec::new(),
            maximum,
        }
    }

    fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < self.pending.len()
            && (eof || self.pending.len().saturating_sub(offset) >= self.maximum)
        {
            let remaining = &self.pending[offset..];
            let matched = self
                .secrets
                .iter()
                .filter(|secret| !secret.is_empty() && remaining.starts_with(secret))
                .map(Vec::len)
                .max();
            if let Some(length) = matched {
                output.extend_from_slice(b"[REDACTED]");
                offset += length;
            } else {
                output.push(self.pending[offset]);
                offset += 1;
            }
        }
        self.pending.drain(..offset);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::Redactor;
    use std::sync::Arc;

    #[test]
    fn redact_every_chunk_boundary_and_binary_output() {
        let input = b"\xff token-long token end token-long\x00";
        for boundary in 0..=input.len() {
            let mut redactor =
                Redactor::new(Arc::new(vec![b"token".to_vec(), b"token-long".to_vec()]));
            let mut actual = redactor.push(&input[..boundary], false);
            actual.extend(redactor.push(&input[boundary..], false));
            actual.extend(redactor.push(&[], true));
            assert_eq!(actual, b"\xff [REDACTED] [REDACTED] end [REDACTED]\x00");
        }
    }

    #[test]
    fn preserve_partial_secret_and_redact_repeated_values() {
        let mut redactor = Redactor::new(Arc::new(vec![b"secret".to_vec()]));
        let mut actual = Vec::new();
        for byte in b"secretsecretsecre" {
            actual.extend(redactor.push(&[*byte], false));
        }
        actual.extend(redactor.push(&[], true));
        assert_eq!(actual, b"[REDACTED][REDACTED]secre");
    }
}
