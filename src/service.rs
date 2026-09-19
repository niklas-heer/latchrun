//! In-memory local session service. A connection never owns an operation's lifetime.
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nix::fcntl::{Flock, FlockArg};
use serde_json::{Value, json};

use crate::{
    execution,
    protocol::{
        Failure, Profile, Request, Response, random_id, read_frame, validate_id, write_frame,
    },
};

const MAX_SESSIONS: usize = 128;
const MAX_OPERATIONS: usize = 1024;
const MAX_RUNNING: usize = 32;
const MAX_CLIENTS: usize = 64;
const MAX_EVENTS: usize = 512;
type Control = Arc<Mutex<Option<ChildStdin>>>;
type Shared = Arc<Mutex<State>>;

struct Session {
    id: String,
    name: String,
    profile: Profile,
    started: Instant,
    created_at: u64,
    status: &'static str,
}

struct Operation {
    session: String,
    status: &'static str,
    started_at: u64,
    started: Instant,
    duration_ms: Option<u64>,
    finished_at: Option<u64>,
    exit_code: Option<i32>,
    control: Control,
}

impl Operation {
    fn active(&self) -> bool {
        matches!(self.status, "reserved" | "running")
    }

    fn view(&self, id: &str) -> Value {
        json!({"id":id,"session":self.session,"status":self.status,
            "started_at":self.started_at,"finished_at":self.finished_at,"exit_code":self.exit_code,
            "duration_ms":self.duration_ms.unwrap_or_else(||milliseconds(self.started.elapsed()))})
    }
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Session>,
    operations: BTreeMap<String, Operation>,
    events: VecDeque<Value>,
    sequence: u64,
    denied: u64,
    stopping: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Retain access to controls after a panic so shutdown can still kill children.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl State {
    fn record(&mut self, kind: &str, session: Option<&str>, operation: Option<&str>) {
        self.sequence = self.sequence.saturating_add(1);
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events
            .push_back(json!({"sequence":self.sequence,"timestamp":timestamp(),
            "kind":kind,"session":session,"operation":operation}));
    }

    fn session_id(&self, name: &str) -> Result<String, Failure> {
        validate_id(name)?;
        self.sessions.values().find(|s| s.id == name || s.name == name)
            .map(|s| s.id.clone())
            .ok_or_else(|| Failure::new("unknown_session", "Session history is unavailable; operations must not be automatically replayed."))
    }

    fn session_view(&self, id: &str) -> Value {
        self.sessions.get(id).map_or(Value::Null, |session| {
            let operations: Vec<Value> = self.operations.iter()
                .filter(|(_, operation)| operation.session == id)
                .map(|(id, operation)| operation.view(id)).collect();
            json!({"id":session.id,"name":session.name,"status":session.status,
                "created_at":session.created_at,"expires_at":session.created_at.saturating_add(session.profile.ttl_seconds),
                "operations":operations})
        })
    }

    fn stop_session(&mut self, id: &str, status: &'static str) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if session.status != "active" {
            return;
        }
        session.status = status;
        for operation in self
            .operations
            .values()
            .filter(|op| op.session == id && op.active())
        {
            lock(&operation.control).take();
        }
        self.record(status, Some(id), None);
    }

    fn expire(&mut self, now: Instant) {
        let expired: Vec<String> = self
            .sessions
            .values()
            .filter(|s| {
                s.status == "active"
                    && now.saturating_duration_since(s.started)
                        >= Duration::from_secs(s.profile.ttl_seconds)
            })
            .map(|s| s.id.clone())
            .collect();
        for id in expired {
            self.stop_session(&id, "expired");
        }
    }

    fn shutdown(&mut self) {
        self.stopping = true;
        let sessions: Vec<String> = self.sessions.keys().cloned().collect();
        for id in sessions {
            self.stop_session(&id, "stopped");
        }
    }

    fn start(&mut self, name: String, mut profile: Profile) -> Result<Value, Failure> {
        validate_id(&name)?;
        profile.validate()?;
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(Failure::new(
                "capacity",
                "Session capacity reached; restart the service after reviewing completed work.",
            ));
        }
        if self
            .sessions
            .values()
            .any(|s| s.name == name || s.id == name)
        {
            return Err(Failure::new(
                "session_exists",
                "Session name is already in use.",
            ));
        }
        let id = random_id()?;
        let session = Session {
            id: id.clone(),
            name,
            profile,
            started: Instant::now(),
            created_at: timestamp(),
            status: "active",
        };
        self.sessions.insert(id.clone(), session);
        self.record("started", Some(&id), None);
        Ok(self.session_view(&id))
    }

    fn reserve(
        &mut self,
        session: &str,
        operation: &str,
        argv: &[String],
    ) -> Result<String, Failure> {
        validate_id(operation)?;
        let id = self.session_id(session)?;
        if self.operations.contains_key(operation) {
            return Err(Failure::new(
                "duplicate_operation",
                "Operation ID was already used; inspect its status and do not replay it.",
            ));
        }
        let profile = self
            .sessions
            .get(&id)
            .ok_or_else(|| Failure::new("unknown_session", "Session history is unavailable."))?;
        if profile.status != "active" || self.stopping {
            return Err(Failure::new(
                "session_inactive",
                "Session is stopped or expired.",
            ));
        }
        if let Err(error) = profile.profile.authorize(argv) {
            self.record("denied", Some(&id), Some(operation));
            return Err(error);
        }
        if self.operations.len() >= MAX_OPERATIONS
            || self.operations.values().filter(|op| op.active()).count() >= MAX_RUNNING
        {
            return Err(Failure::new(
                "capacity",
                "Operation capacity reached; existing IDs remain reserved.",
            ));
        }
        self.operations.insert(
            operation.to_owned(),
            Operation {
                session: id.clone(),
                status: "reserved",
                started_at: timestamp(),
                started: Instant::now(),
                duration_ms: None,
                finished_at: None,
                exit_code: None,
                control: Arc::new(Mutex::new(None)),
            },
        );
        self.record("accepted", Some(&id), Some(operation));
        Ok(id)
    }

    fn finish(&mut self, operation: &str, status: &'static str, exit_code: Option<i32>) {
        if let Some(op) = self.operations.get_mut(operation) {
            op.status = status;
            op.exit_code = exit_code;
            op.finished_at = Some(timestamp());
            op.duration_ms = Some(milliseconds(op.started.elapsed()));
            lock(&op.control).take();
            let session = op.session.clone();
            self.record(status, Some(&session), Some(operation));
        }
    }
}

fn error_response(error: Failure) -> Response {
    Response::Error {
        code: error.code,
        message: error.message,
    }
}

fn dispatch(state: &mut State, request: Request) -> Result<Value, Failure> {
    state.expire(Instant::now());
    if state.stopping {
        return Err(Failure::new("service_stopping", "The service is stopping."));
    }
    match request {
        Request::Ping {} => Ok(json!({"status":"running","version":env!("CARGO_PKG_VERSION"),
            "statistics":{"accepted":state.operations.len(),"denied":state.denied,
                "running":state.operations.values().filter(|op|op.active()).count(),
                "succeeded":state.operations.values().filter(|op|op.status == "succeeded").count(),
                "failed":state.operations.values().filter(|op|op.status == "failed").count(),
                "unknown":state.operations.values().filter(|op|op.status == "unknown").count()}})),
        Request::Shutdown {} => { state.shutdown(); Ok(json!({"status":"stopping"})) }
        Request::Start { name, profile } => state.start(name, profile),
        Request::Status { session } => session.map_or_else(
            || Ok(json!({"sessions":state.sessions.keys().map(|id|state.session_view(id)).collect::<Vec<_>>()})),
            |name| state.session_id(&name).map(|id|state.session_view(&id))),
        Request::Stop { session } => {
            let id = state.session_id(&session)?;
            state.stop_session(&id, "stopped");
            Ok(state.session_view(&id))
        }
        Request::Events { session } => {
            let id = session.as_deref().map(|s|state.session_id(s)).transpose()?;
            let events: Vec<&Value> = state.events.iter().filter(|event| id.as_ref().is_none_or(|id| event.get("session").and_then(Value::as_str) == Some(id.as_str()))).collect();
            Ok(json!({"events":events}))
        }
        Request::Inspect { session } => {
            let id = state.session_id(&session)?;
            let environment = state.sessions.get(&id).map(|s| {
                s.profile.credentials.keys().map(|name| json!({"name":name,"source":s.profile.provider,"declared":true,"presence":"resolved_per_operation"})).collect::<Vec<_>>()
            }).unwrap_or_default();
            Ok(json!({"session":state.session_view(&id),"environment":environment,"credential_cache":"none","values_available":false}))
        }
        Request::Signal { session, operation, signal } => {
            if !matches!(signal, 1 | 2 | 3 | 15) {
                return Err(Failure::new("invalid_signal", "Only HUP, INT, QUIT, and TERM can be forwarded."));
            }
            let id = state.session_id(&session)?;
            let op = state.operations.get(&operation).filter(|op|op.session == id)
                .ok_or_else(|| Failure::new("unknown_operation", "Operation history is unavailable; do not replay the operation."))?;
            let mut control = lock(&op.control);
            let pipe = control.as_mut().filter(|_| op.active()).ok_or_else(|| Failure::new("operation_inactive", "Operation is no longer running."))?;
            writeln!(pipe, "{signal}").map_err(|_|Failure::new("operation_inactive", "Operation is no longer running."))?;
            drop(control);
            state.record("signaled", Some(&id), Some(&operation));
            Ok(json!({"status":"signaled"}))
        }
        Request::Run { .. } => Err(Failure::new("invalid_request", "Invalid request.")),
    }
}

fn send(stream: &mut Option<UnixStream>, response: &Response) {
    if let Some(connection) = stream.as_mut()
        && write_frame(connection, response).is_err()
    {
        *stream = None;
    }
}

fn launch(
    state: &Shared,
    session: &str,
    operation: &str,
    argv: &[String],
) -> Result<(Child, ChildStdout), Failure> {
    let mut state = lock(state);
    state.expire(Instant::now());
    let profile = state
        .sessions
        .get(session)
        .filter(|s| s.status == "active" && !state.stopping)
        .ok_or_else(|| Failure::new("session_inactive", "Session is stopped or expired."))?;
    // spawn only starts a guardian; provider resolution happens after this lock is released.
    let worker = execution::spawn(&profile.profile, argv)?;
    if let Some(op) = state.operations.get_mut(operation) {
        op.status = "running";
        *lock(&op.control) = Some(worker.control);
    }
    drop(state);
    Ok((worker.child, worker.output))
}

fn run_operation(
    state: &Shared,
    mut stream: Option<UnixStream>,
    session: &str,
    operation: &str,
    argv: &[String],
) {
    let worker = launch(state, session, operation, argv);
    let (mut child, output) = match worker {
        Ok(worker) => worker,
        Err(error) => {
            lock(state).finish(operation, "failed", None);
            send(&mut stream, &error_response(error));
            return;
        }
    };
    // Acknowledgement also promises that a signal can reach the guardian.
    send(
        &mut stream,
        &Response::Accepted {
            operation: operation.to_owned(),
        },
    );
    let mut output = BufReader::new(output);
    loop {
        match read_frame::<_, Response>(&mut output) {
            Ok(response @ Response::Output { .. }) => send(&mut stream, &response),
            Ok(Response::Finished { exit_code }) => {
                lock(state).finish(
                    operation,
                    if exit_code == 0 {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    Some(exit_code),
                );
                send(&mut stream, &Response::Finished { exit_code });
                break;
            }
            Ok(response @ Response::Error { .. }) => {
                lock(state).finish(operation, "failed", None);
                send(&mut stream, &response);
                break;
            }
            _ => {
                lock(state).finish(operation, "unknown", None);
                send(
                    &mut stream,
                    &error_response(Failure::new(
                        "outcome_unknown",
                        "Worker connection was lost; inspect status and do not replay the operation.",
                    )),
                );
                break;
            }
        }
    }
    let _ = child.wait();
}

fn handle(state: &Shared, mut stream: UnixStream) {
    let Ok(request) = read_frame::<_, Request>(&mut BufReader::new(&mut stream)) else {
        let _ = write_frame(
            &mut stream,
            &error_response(Failure::new("invalid_request", "Invalid request.")),
        );
        return;
    };
    if let Request::Run {
        session,
        operation,
        mut argv,
    } = request
    {
        let reserved = normalize_executable(&mut argv).and_then(|()| {
            let mut state = lock(state);
            state.expire(Instant::now());
            state.reserve(&session, &operation, &argv)
        });
        match reserved {
            Ok(id) => run_operation(state, Some(stream), &id, &operation, &argv),
            Err(error) => {
                let mut state = lock(state);
                state.denied = state.denied.saturating_add(1);
                drop(state);
                let _ = write_frame(&mut stream, &error_response(error));
            }
        }
    } else {
        let result = dispatch(&mut lock(state), request);
        let response = result.map_or_else(error_response, |data| Response::Ok { data });
        let _ = write_frame(&mut stream, &response);
    }
}

fn normalize_executable(argv: &mut [String]) -> Result<(), Failure> {
    let denied = || {
        Failure::new(
            "policy_denied",
            "Executable and arguments must exactly match an approved command.",
        )
    };
    let executable = argv.first_mut().ok_or_else(denied)?;
    if !Path::new(executable).is_absolute() {
        return Err(denied());
    }
    let canonical = fs::canonicalize(&executable).map_err(|_| denied())?;
    canonical
        .to_str()
        .ok_or_else(denied)?
        .clone_into(executable);
    Ok(())
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn listener(runtime: &Path) -> Result<(UnixListener, Flock<File>, SocketGuard), Failure> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(runtime.join("service.lock"))
        .map_err(|_| Failure::new("runtime_error", "Cannot open service lock."))?;
    if !file
        .metadata()
        .map_err(|_| Failure::new("runtime_error", "Cannot inspect service lock."))?
        .is_file()
    {
        return Err(Failure::new(
            "runtime_error",
            "Service lock is not a regular file.",
        ));
    }
    let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| {
        Failure::new(
            "service_running",
            "Another service owns this runtime directory.",
        )
    })?;
    let socket = runtime.join("service.sock");
    match fs::symlink_metadata(&socket) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(Failure::new(
                    "runtime_error",
                    "Socket path is not a socket.",
                ));
            }
            match UnixStream::connect(&socket) {
                Ok(_) => {
                    return Err(Failure::new(
                        "service_running",
                        "A service is already listening.",
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&socket).map_err(|_| {
                        Failure::new("runtime_error", "Cannot remove stale socket.")
                    })?;
                }
                Err(_) => return Err(Failure::new("runtime_error", "Cannot verify stale socket.")),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(Failure::new("runtime_error", "Cannot inspect socket path.")),
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|_| Failure::new("runtime_error", "Cannot bind service socket."))?;
    let guard = SocketGuard(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|_| Failure::new("runtime_error", "Cannot secure service socket."))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| Failure::new("runtime_error", "Cannot configure service socket."))?;
    Ok((listener, lock, guard))
}

struct ClientGuard(Arc<AtomicUsize>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn serve(runtime: &Path) -> Result<(), Failure> {
    let (listener, _lock, _socket) = listener(runtime)?;
    let state = Arc::new(Mutex::new(State::default()));
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))
            .map_err(|_| Failure::new("service_error", "Cannot register service signals."))?;
    }
    let clients = Arc::new(AtomicUsize::new(0));
    loop {
        {
            let mut state = lock(&state);
            state.expire(Instant::now());
            if stopping.load(Ordering::Relaxed) {
                state.shutdown();
            }
            if state.stopping {
                break;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                    drop(stream);
                    continue;
                }
                if stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .is_err()
                    || stream
                        .set_write_timeout(Some(Duration::from_millis(250)))
                        .is_err()
                {
                    continue;
                }
                clients.fetch_add(1, Ordering::Relaxed);
                let guard = ClientGuard(Arc::clone(&clients));
                let state = Arc::clone(&state);
                let _ = thread::Builder::new()
                    .name("client".to_owned())
                    .spawn(move || {
                        let _guard = guard;
                        handle(&state, stream);
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                lock(&state).shutdown();
                break;
            }
        }
    }
    // Guardians observe control EOF even if a client disconnected long ago.
    let deadline = Instant::now() + Duration::from_secs(5);
    while (clients.load(Ordering::Relaxed) > 0
        || lock(&state).operations.values().any(Operation::active))
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CommandRule, Provider};

    fn fixture(started: Instant) -> Result<(State, Vec<String>), Failure> {
        let executable = fs::canonicalize("/bin/echo")?;
        let argv = vec![executable.to_string_lossy().into_owned()];
        let profile = Profile {
            project: std::env::temp_dir(),
            purpose: "private-purpose".into(),
            ttl_seconds: 10,
            timeout_seconds: 5,
            provider: Provider::Fake,
            credentials: BTreeMap::from([("TOKEN".into(), "fake://private-reference".into())]),
            commands: vec![CommandRule {
                executable,
                args: Vec::new(),
            }],
            op_path: None,
            ssh_auth_sock: None,
        };
        let mut state = State::default();
        state.sessions.insert(
            "session".into(),
            Session {
                id: "session".into(),
                name: "work".into(),
                profile,
                started,
                created_at: 1,
                status: "active",
            },
        );
        Ok((state, argv))
    }

    #[test]
    fn reserved_ids_survive_outcomes_and_expiry() -> Result<(), Failure> {
        let now = Instant::now();
        let (mut state, argv) = fixture(now)?;
        state.reserve("work", "operation", &argv)?;
        state.finish("operation", "unknown", None);
        assert!(
            matches!(state.reserve("work", "operation", &argv), Err(error) if error.code == "duplicate_operation")
        );
        state.expire(now + Duration::from_secs(9));
        assert_eq!(state.session_view("session")["status"], "active");
        state.expire(now + Duration::from_secs(10));
        assert_eq!(state.session_view("session")["status"], "expired");
        assert!(
            matches!(state.reserve("work", "new-operation", &argv), Err(error) if error.code == "session_inactive")
        );
        assert!(
            matches!(state.reserve("work", "operation", &argv), Err(error) if error.code == "duplicate_operation")
        );
        assert!(State::default().session_id("work").is_err());
        Ok(())
    }

    #[test]
    fn stop_between_acceptance_and_spawn_denies_execution() -> Result<(), Failure> {
        let (mut state, argv) = fixture(Instant::now())?;
        state.reserve("work", "operation", &argv)?;
        state.stop_session("session", "stopped");
        let state = Arc::new(Mutex::new(state));
        assert!(
            matches!(launch(&state, "session", "operation", &argv), Err(error) if error.code == "session_inactive")
        );
        Ok(())
    }

    #[test]
    fn full_operation_ledger_fits_ipc_and_never_evicts_ids() -> Result<(), Failure> {
        let (mut state, argv) = fixture(Instant::now())?;
        for number in 0..MAX_OPERATIONS {
            let operation = format!("{number:064}");
            state.reserve("work", &operation, &argv)?;
            state.finish(&operation, "succeeded", Some(0));
        }
        assert!(
            matches!(state.reserve("work", "overflow", &argv), Err(error) if error.code == "capacity")
        );
        assert!(
            matches!(state.reserve("work", &format!("{:064}", 0), &argv), Err(error) if error.code == "duplicate_operation")
        );
        let response = Response::Ok {
            data: dispatch(&mut state, Request::Status { session: None })?,
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &response)?;
        // Leave room for every session header and full-length generated session IDs.
        assert!(
            frame.len() + MAX_OPERATIONS * 64 + MAX_SESSIONS * 1024 < crate::protocol::MAX_FRAME
        );
        assert_eq!(state.events.len(), MAX_EVENTS);
        Ok(())
    }

    #[test]
    fn seeded_lifecycle_sequences_preserve_operation_identity() -> Result<(), Failure> {
        // Exercise production transitions with deterministic time and fault outcomes.
        for seed in [1_u64, 7, 42, 2026] {
            let now = Instant::now();
            let (mut state, argv) = fixture(now)?;
            let mut rng = seed;
            let mut expired = false;
            for step in 0..1024_u64 {
                rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let operation = format!("op{}", rng % 32);
                let known = state.operations.contains_key(&operation);
                match rng % 7 {
                    0 => state.finish(&operation, "unknown", None),
                    1 => state.finish(&operation, "succeeded", Some(0)),
                    2 if step > 100 => {
                        state.expire(now + Duration::from_secs(10));
                        expired = true;
                    }
                    _ => {
                        let result = state.reserve("work", &operation, &argv);
                        if known {
                            assert!(
                                matches!(result, Err(error) if error.code == "duplicate_operation")
                            );
                        } else if expired {
                            assert!(
                                matches!(result, Err(error) if error.code == "session_inactive")
                            );
                        } else {
                            result?;
                        }
                    }
                }
                assert!(state.events.len() <= MAX_EVENTS);
                assert!(state.operations.len() <= 32);
                let view = state.session_view("session").to_string();
                assert!(!view.contains("private-purpose"));
                assert!(!view.contains("private-reference"));
                assert!(!view.contains("/bin/echo"));
            }
        }
        Ok(())
    }
}
