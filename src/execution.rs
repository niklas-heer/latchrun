//! Credential resolution and process supervision in a short-lived guardian.
use crate::{
    protocol::{Failure, InputMode, Profile, Response, read_frame, write_frame},
    providers::{self, Credentials, SECRET_LIMIT},
};
use nix::{
    sys::{
        resource::{Resource, setrlimit},
        signal::{Signal, killpg},
    },
    unistd::{Pid, User, getpgid, getsid, getuid},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::{self, BufReader, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(10);
const KILL_GRACE: Duration = Duration::from_millis(200);
const INPUT_LIMIT: usize = 8192;

pub struct Worker {
    pub child: Child,
    pub control: ChildStdin,
    pub output: ChildStdout,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerFrame {
    Output { response: Response },
    Resolved { credentials: Credentials },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerControl {
    Signal { signal: i32 },
    Input { data: Vec<u8>, eof: bool },
    Resize { rows: u16, cols: u16 },
}

#[derive(Serialize, Deserialize)]
struct WorkerSpec {
    profile: Profile,
    argv: Vec<String>,
    cached_credentials: Option<Credentials>,
    input: InputMode,
}
#[derive(Serialize, Deserialize)]
struct PtySpec {
    profile: Profile,
    argv: Vec<String>,
}

pub fn spawn(
    profile: &Profile,
    argv: &[String],
    cached_credentials: Option<Credentials>,
    input: InputMode,
) -> Result<Worker, Failure> {
    let mut child = Command::new(std::env::current_exe().map_err(|_| worker_failure())?)
        .arg("__worker")
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
        cached_credentials,
        input,
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
fn cancelled() -> Failure {
    Failure::new("cancelled", "Command execution was cancelled.")
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct InputChunk {
    data: Vec<u8>,
    eof: bool,
}
struct ProcessState {
    group: Option<Pid>,
    session: Option<Pid>,
    cancellation: Option<Instant>,
    forced: bool,
    terminal: Option<Box<dyn MasterPty + Send>>,
    size: PtySize,
    input_error: bool,
}
impl Default for ProcessState {
    fn default() -> Self {
        Self {
            group: None,
            session: None,
            cancellation: None,
            forced: false,
            terminal: None,
            size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            input_error: false,
        }
    }
}

pub struct Supervisor {
    state: Arc<Mutex<ProcessState>>,
    done: Arc<AtomicBool>,
    input: Mutex<Option<mpsc::Receiver<InputChunk>>>,
}
impl Supervisor {
    fn start(
        input: BufReader<io::Stdin>,
        timeout: Duration,
        mode: InputMode,
    ) -> Result<Self, Failure> {
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
        let (sender, receiver) = mpsc::sync_channel(128);
        let control_state = Arc::clone(&state);
        thread::Builder::new()
            .name("control".into())
            .spawn(move || monitor_control(input, &control_state, &sender, mode))
            .map_err(|_| worker_failure())?;
        let watchdog_state = Arc::clone(&state);
        let watchdog_done = Arc::clone(&done);
        thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || {
                let start = Instant::now();
                while !watchdog_done.load(Ordering::Acquire) {
                    let mut state = lock(&watchdog_state);
                    if interrupted.load(Ordering::Acquire) || start.elapsed() >= timeout {
                        cancel(&mut state, Signal::SIGTERM);
                    }
                    if state
                        .cancellation
                        .is_some_and(|at| at.elapsed() >= KILL_GRACE)
                        && !state.forced
                    {
                        signal_tree(&state, Signal::SIGKILL);
                        state.forced = true;
                    }
                    drop(state);
                    thread::sleep(POLL);
                }
            })
            .map_err(|_| worker_failure())?;
        Ok(Self {
            state,
            done,
            input: Mutex::new(Some(receiver)),
        })
    }
    fn spawn(&self, command: &mut Command) -> Result<Child, Failure> {
        let mut state = lock(&self.state);
        if state.cancellation.is_some() {
            return Err(cancelled());
        }
        let mut child = command
            .process_group(0)
            .spawn()
            .map_err(|_| worker_failure())?;
        let Ok(raw) = i32::try_from(child.id()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(worker_failure());
        };
        state.group = Some(Pid::from_raw(raw));
        drop(state);
        Ok(child)
    }
    fn spawn_tty(
        &self,
        spec: &WorkerSpec,
        credentials: &Credentials,
    ) -> Result<(Child, Box<dyn Read + Send>), Failure> {
        let mut state = lock(&self.state);
        if state.cancellation.is_some() {
            return Err(cancelled());
        }
        let pair = native_pty_system()
            .openpty(state.size)
            .map_err(|_| worker_failure())?;
        let mut builder =
            CommandBuilder::new(std::env::current_exe().map_err(|_| worker_failure())?);
        builder.arg("__ptyexec");
        builder.env_clear();
        builder.cwd(&spec.profile.project);
        let mut environment = Command::new("unused");
        configure_environment(&mut environment, &spec.profile, credentials);
        for (name, value) in environment.get_envs() {
            if let Some(value) = value {
                builder.env(name, value);
            }
        }
        builder.env(
            "TERM",
            spec.profile
                .environment
                .get("TERM")
                .map_or("xterm-256color", String::as_str),
        );
        let payload = serde_json::to_string(&PtySpec {
            profile: spec.profile.clone(),
            argv: spec.argv.clone(),
        })
        .map_err(|_| worker_failure())?;
        if payload.len() > 65_536 {
            return Err(Failure::new(
                "pty_spec",
                "TTY profile and command exceed the 64 KiB limit.",
            ));
        }
        builder.env("LATCHRUN_PTY_SPEC", payload);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| worker_failure())?;
        let writer = pair.master.take_writer().map_err(|_| worker_failure())?;
        let boxed: Box<dyn portable_pty::Child> = pair
            .slave
            .spawn_command(builder)
            .map_err(|_| worker_failure())?;
        let mut child = match boxed.downcast::<Child>() {
            Ok(child) => *child,
            Err(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(worker_failure());
            }
        };
        drop(pair.slave);
        let Ok(raw) = i32::try_from(child.id()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(worker_failure());
        };
        let group = Pid::from_raw(raw);
        state.group = Some(group);
        state.session = Some(group);
        state.terminal = Some(pair.master);
        drop(state);
        self.start_input(writer)?;
        Ok((child, reader))
    }
    fn start_input(&self, mut writer: impl Write + Send + 'static) -> Result<(), Failure> {
        let receiver = lock(&self.input).take().ok_or_else(worker_failure)?;
        thread::Builder::new()
            .name("stdin".into())
            .spawn(move || {
                while let Ok(chunk) = receiver.recv() {
                    if writer.write_all(&chunk.data).is_err()
                        || writer.flush().is_err()
                        || chunk.eof
                    {
                        return;
                    }
                }
            })
            .map_err(|_| worker_failure())?;
        Ok(())
    }
    fn cleanup(&self) {
        let mut state = lock(&self.state);
        signal_tree(&state, Signal::SIGKILL);
        state.group = None;
        state.session = None;
        state.terminal = None;
    }
    pub fn cancelled(&self) -> bool {
        lock(&self.state).cancellation.is_some()
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cleanup();
        self.done.store(true, Ordering::Release);
    }
}

fn signal_tree(state: &ProcessState, signal: Signal) {
    if let Some(group) = state
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.process_group_leader())
    {
        let _ = killpg(Pid::from_raw(group), signal);
    }
    if let Some(session) = state.session {
        signal_session(session, signal);
    }
    if let Some(group) = state.group {
        let _ = killpg(group, signal);
    }
}

fn signal_session(session: Pid, signal: Signal) {
    // Shell job control creates multiple groups in one session. Ask the OS to
    // verify membership before signaling each discovered group.
    let mut groups = BTreeSet::new();
    for pid in process_ids() {
        if getsid(Some(pid)).ok() == Some(session)
            && let Ok(group) = getpgid(Some(pid))
        {
            groups.insert(group);
        }
    }
    for group in groups {
        let _ = killpg(group, signal);
    }
}

#[cfg(target_os = "linux")]
fn process_ids() -> Vec<Pid> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .take(262_144)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
        })
        .filter(|pid| *pid > 0)
        .map(Pid::from_raw)
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn process_ids() -> Vec<Pid> {
    let Ok(mut ps) = Command::new("/bin/ps")
        .args(["-axo", "pid="])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let Some(output) = ps.stdout.take() else {
        let _ = ps.kill();
        let _ = ps.wait();
        return Vec::new();
    };
    let mut bytes = Vec::new();
    let result = output.take(2_097_153).read_to_end(&mut bytes);
    if bytes.len() > 2_097_152 {
        let _ = ps.kill();
    }
    let _ = ps.wait();
    if result.is_err() || bytes.len() > 2_097_152 {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .split_whitespace()
        .filter_map(|pid| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .map(Pid::from_raw)
        .collect()
}

fn cancel(state: &mut ProcessState, signal: Signal) {
    if state.cancellation.is_none() {
        state.cancellation = Some(Instant::now());
        signal_tree(state, signal);
    }
}

fn monitor_control(
    mut input: BufReader<io::Stdin>,
    state: &Mutex<ProcessState>,
    sender: &mpsc::SyncSender<InputChunk>,
    mode: InputMode,
) {
    let mut input_ended = false;
    loop {
        let Ok(control) = read_frame::<_, WorkerControl>(&mut input) else {
            cancel(&mut lock(state), Signal::SIGTERM);
            return;
        };
        match control {
            WorkerControl::Signal { signal } => {
                let signal = match signal {
                    1 => Signal::SIGHUP,
                    2 => Signal::SIGINT,
                    3 => Signal::SIGQUIT,
                    _ => Signal::SIGTERM,
                };
                cancel(&mut lock(state), signal);
            }
            WorkerControl::Input { data, eof } => {
                if !matches!(mode, InputMode::Null)
                    && !input_ended
                    && data.len() <= INPUT_LIMIT
                    && sender.try_send(InputChunk { data, eof }).is_ok()
                {
                    input_ended = eof;
                } else {
                    let mut state = lock(state);
                    state.input_error = true;
                    cancel(&mut state, Signal::SIGTERM);
                    drop(state);
                }
            }
            WorkerControl::Resize { rows, cols } => {
                if !matches!(mode, InputMode::Tty) || rows == 0 || cols == 0 {
                    continue;
                }
                let mut state = lock(state);
                state.size = PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                if let Some(terminal) = &state.terminal {
                    let _ = terminal.resize(state.size);
                }
            }
        }
    }
}

pub fn worker_main() -> Result<(), Failure> {
    setrlimit(Resource::RLIMIT_CORE, 0, 0).map_err(|_| worker_failure())?;
    let mut input = BufReader::new(io::stdin());
    let mut spec: WorkerSpec = read_frame(&mut input)?;
    let supervisor = Supervisor::start(
        input,
        Duration::from_secs(spec.profile.timeout_seconds),
        spec.input,
    )?;
    let result = spec
        .profile
        .validate()
        .and_then(|()| spec.profile.authorize(&spec.argv))
        .and_then(|()| execute(&mut spec, &supervisor));
    supervisor.cleanup();
    let response = match result {
        Ok(exit_code) => Response::Finished { exit_code },
        Err(error) => Response::Error {
            code: error.code,
            message: error.message,
        },
    };
    emit(response)
}

fn emit(response: Response) -> Result<(), Failure> {
    write_frame(&mut io::stdout().lock(), &WorkerFrame::Output { response })
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

pub fn read_provider(
    executable: &Path,
    arguments: &[&str],
    supervisor: &Supervisor,
) -> Result<Vec<u8>, Failure> {
    let mut command = clean_command(executable);
    command.args(arguments);
    let user = User::from_uid(getuid())
        .map_err(|_| providers::failure())?
        .ok_or_else(providers::failure)?;
    command
        .env("HOME", user.dir)
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin");
    let mut child = supervisor
        .spawn(&mut command)
        .map_err(|_| providers::failure())?;
    let output = child.stdout.take().ok_or_else(providers::failure)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("provider".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = output.take(65_537).read_to_end(&mut bytes);
            let _ = sender.send(result.map(|_| bytes));
        })
        .map_err(|_| providers::failure())?;
    let start = Instant::now();
    let mut status = None;
    let mut bytes = None;
    let result = loop {
        if supervisor.cancelled() || start.elapsed() > Duration::from_secs(30) {
            break Err(providers::failure());
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    supervisor.cleanup();
                    if !exit.success() {
                        break Err(providers::failure());
                    }
                }
                Ok(None) => {}
                Err(_) => break Err(providers::failure()),
            }
        }
        if bytes.is_none() {
            match receiver.try_recv() {
                Ok(Ok(value)) if value.len() <= SECRET_LIMIT => bytes = Some(value),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => break Err(providers::failure()),
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

fn configure_environment(command: &mut Command, profile: &Profile, credentials: &Credentials) {
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .envs(&profile.environment)
        .current_dir(&profile.project);
    for (name, value) in credentials {
        command.env(name, OsStr::from_bytes(value));
    }
    if let Some(socket) = &profile.ssh_auth_sock {
        command.env("SSH_AUTH_SOCK", socket);
    }
}

/// Runs inside the allocated terminal; preparing here preserves sandbox filter FDs.
pub fn pty_exec_main() -> Result<(), Failure> {
    let payload = std::env::var("LATCHRUN_PTY_SPEC").map_err(|_| worker_failure())?;
    let mut spec: PtySpec = serde_json::from_str(&payload).map_err(|_| worker_failure())?;
    spec.profile.validate()?;
    spec.profile.authorize(&spec.argv)?;
    let mut credentials = Credentials::new();
    for name in spec.profile.credentials.keys() {
        let value = std::env::var_os(name).ok_or_else(worker_failure)?;
        credentials.insert(name.clone(), value.into_vec());
    }
    providers::validate(&spec.profile, &credentials)?;
    let mut prepared = crate::sandbox::prepare_terminal(&spec.profile, &spec.argv)?;
    configure_environment(&mut prepared.command, &spec.profile, &credentials);
    prepared.command.env(
        "TERM",
        spec.profile
            .environment
            .get("TERM")
            .map_or("xterm-256color", String::as_str),
    );
    if let Some(git) = &spec.profile.git_https {
        crate::git_credentials::configure(&mut prepared.command, git)?;
    }
    let _ = prepared.command.exec();
    Err(worker_failure())
}

fn execute(spec: &mut WorkerSpec, supervisor: &Supervisor) -> Result<i32, Failure> {
    let fresh = spec.cached_credentials.is_none();
    let credentials =
        providers::resolve(&spec.profile, supervisor, spec.cached_credentials.take())?;
    if fresh && spec.profile.cache_ttl_seconds > 0 {
        write_frame(
            &mut io::stdout().lock(),
            &WorkerFrame::Resolved {
                credentials: credentials.clone(),
            },
        )?;
    }
    let mut patterns: Vec<Vec<u8>> = credentials.values().cloned().collect();
    if matches!(spec.input, InputMode::Tty) {
        for value in credentials.values().filter(|value| value.contains(&b'\n')) {
            let mut terminal_value = Vec::new();
            for byte in value {
                if *byte == b'\n' {
                    terminal_value.push(b'\r');
                }
                terminal_value.push(*byte);
            }
            patterns.push(terminal_value);
        }
    }
    let secrets = Arc::new(patterns);
    let (sender, receiver) = mpsc::sync_channel(16);
    let mut child = if matches!(spec.input, InputMode::Tty) {
        let (child, reader) = supervisor.spawn_tty(spec, &credentials)?;
        pump(reader, "stdout", Arc::clone(&secrets), sender.clone())?;
        child
    } else {
        let mut prepared = crate::sandbox::prepare(&spec.profile, &spec.argv)?;
        configure_environment(&mut prepared.command, &spec.profile, &credentials);
        if let Some(git) = &spec.profile.git_https {
            crate::git_credentials::configure(&mut prepared.command, git)?;
        }
        prepared
            .command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if matches!(spec.input, InputMode::Pipe) {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = supervisor.spawn(&mut prepared.command)?;
        if matches!(spec.input, InputMode::Pipe) {
            supervisor.start_input(child.stdin.take().ok_or_else(worker_failure)?)?;
        }
        pump(
            child.stdout.take().ok_or_else(worker_failure)?,
            "stdout",
            Arc::clone(&secrets),
            sender.clone(),
        )?;
        pump(
            child.stderr.take().ok_or_else(worker_failure)?,
            "stderr",
            secrets,
            sender.clone(),
        )?;
        child
    };
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
        if lock(&supervisor.state).input_error {
            return Err(Failure::new(
                "input_limit",
                "Input was invalid or exceeded the bounded input buffer.",
            ));
        }
        if draining.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1)) {
            return Err(worker_failure());
        }
        match receiver.recv_timeout(POLL) {
            Ok(frame) => emit(frame?)?,
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
) -> Result<(), Failure> {
    thread::Builder::new()
        .name("output".into())
        .spawn(move || {
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
        })
        .map_err(|_| worker_failure())?;
    Ok(())
}
struct Redactor {
    secrets: Arc<Vec<Vec<u8>>>,
    pending: Vec<u8>,
}

impl Redactor {
    const fn new(secrets: Arc<Vec<Vec<u8>>>) -> Self {
        Self {
            secrets,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        if self.secrets.is_empty() {
            return bytes.to_vec();
        }
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            let remaining = &self.pending[offset..];
            // Only a suffix that could still become a secret needs buffering.
            // Unrelated terminal prompts must be visible before the next read.
            if !eof
                && self
                    .secrets
                    .iter()
                    .any(|secret| secret.len() > remaining.len() && secret.starts_with(remaining))
            {
                break;
            }
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
    fn unrelated_terminal_prompts_are_not_buffered() {
        let mut redactor = Redactor::new(Arc::new(vec![b"long-private-secret".to_vec()]));
        assert_eq!(redactor.push(b"prompt> ", false), b"prompt> ");
        assert!(redactor.push(b"long-pr", false).is_empty());
        assert_eq!(redactor.push(b"ivate-secret\n", false), b"[REDACTED]\n");
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
