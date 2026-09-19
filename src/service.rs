//! Durable secret-free metadata and in-memory credential sessions. A connection never owns an operation's lifetime.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::BufReader,
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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    execution,
    protocol::{
        Failure, InputMode, Profile, Request, Response, random_id, read_frame, validate_id,
        write_frame,
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
    cache: Cache,
}

#[derive(Default)]
struct Cache {
    values: Option<BTreeMap<String, Vec<u8>>>,
    fetched: Option<Instant>,
    fetched_at: Option<u64>,
    generation: u64,
}
impl Cache {
    fn clear(&mut self) {
        self.values = None;
        self.fetched = None;
        self.fetched_at = None;
        self.generation = self.generation.saturating_add(1);
    }
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
    used_operations: BTreeSet<String>,
    journal_path: Option<PathBuf>,
    journal_failed: bool,
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
        session.cache.clear();
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
        for session in self.sessions.values_mut() {
            if session.cache.fetched.is_some_and(|at| {
                now.saturating_duration_since(at)
                    >= Duration::from_secs(session.profile.cache_ttl_seconds)
            }) {
                session.cache.clear();
            }
        }
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
        self.protect_runtime(&mut profile)?;
        profile.validate()?;
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(Failure::new(
                "capacity",
                "Session capacity reached; review and prune completed history before creating sessions.",
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
            cache: Cache::default(),
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
        if self.used_operations.contains(operation) {
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
        profile.profile.authorize(argv)?;
        if self.used_operations.len() >= 65_536
            || self.operations.len() >= MAX_OPERATIONS
            || self.operations.values().filter(|op| op.active()).count() >= MAX_RUNNING
        {
            return Err(Failure::new(
                "capacity",
                "Operation capacity reached; existing IDs remain reserved.",
            ));
        }
        self.used_operations.insert(operation.to_owned());
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
        self.persist()?;
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
        if self.persist().is_err() {
            self.journal_failed = true;
            self.shutdown();
        }
    }

    fn deny(&mut self, session: &str, operation: &str) {
        self.denied = self.denied.saturating_add(1);
        let id = self.session_id(session).ok();
        let operation = validate_id(operation).is_ok().then_some(operation);
        self.record("denied", id.as_deref(), operation);
        let _ = self.persist();
    }
}

fn error_response(error: Failure) -> Response {
    Response::Error {
        code: error.code,
        message: error.message,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    sessions: Vec<SavedSession>,
    operations: BTreeMap<String, SavedOperation>,
    used_operations: BTreeSet<String>,
    events: VecDeque<Value>,
    sequence: u64,
    denied: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSession {
    id: String,
    name: String,
    created_at: u64,
    ttl_seconds: u64,
    status: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedOperation {
    session: String,
    status: String,
    started_at: u64,
    duration_ms: Option<u64>,
    finished_at: Option<u64>,
    exit_code: Option<i32>,
}

impl State {
    fn protect_runtime(&self, profile: &mut Profile) -> Result<(), Failure> {
        if profile.sandbox.enabled {
            if let Some(path) = self.journal_path.as_ref().and_then(|path| path.parent()) {
                profile
                    .sandbox
                    .protected_paths
                    .push(fs::canonicalize(path)?);
            }
            if matches!(profile.provider, crate::protocol::Provider::File) {
                for reference in profile.credentials.values() {
                    if let Some(path) = reference.strip_prefix("file://") {
                        profile
                            .sandbox
                            .protected_paths
                            .push(fs::canonicalize(path)?);
                    }
                }
            }
            if profile.git_https.is_some() {
                profile.sandbox.read_paths.push(std::env::current_exe()?);
            }
        }
        Ok(())
    }
    fn persist(&mut self) -> Result<(), Failure> {
        let Some(path) = &self.journal_path else {
            return Ok(());
        };
        let snapshot = Snapshot {
            version: 1,
            sessions: self
                .sessions
                .values()
                .map(|s| SavedSession {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    created_at: s.created_at,
                    ttl_seconds: s.profile.ttl_seconds,
                    status: s.status.into(),
                })
                .collect(),
            operations: self
                .operations
                .iter()
                .map(|(id, op)| {
                    (
                        id.clone(),
                        SavedOperation {
                            session: op.session.clone(),
                            status: op.status.into(),
                            started_at: op.started_at,
                            duration_ms: op.duration_ms,
                            finished_at: op.finished_at,
                            exit_code: op.exit_code,
                        },
                    )
                })
                .collect(),
            used_operations: self.used_operations.clone(),
            events: self.events.clone(),
            sequence: self.sequence,
            denied: self.denied,
        };
        let result = crate::journal::save(path, &snapshot);
        if result.is_err() {
            self.journal_failed = true;
            self.shutdown();
        }
        result
    }

    fn recover(runtime: &Path) -> Result<Self, Failure> {
        let path = runtime.join("history.json");
        let mut state = Self {
            journal_path: Some(path.clone()),
            ..Self::default()
        };
        let Some(saved) = crate::journal::load::<Snapshot>(&path)? else {
            return Ok(state);
        };
        if saved.version != 1
            || saved.sessions.len() > MAX_SESSIONS
            || saved.operations.len() > MAX_OPERATIONS
            || saved.used_operations.len() > 65_536
            || saved.events.len() > MAX_EVENTS
        {
            return Err(Failure::new(
                "journal_invalid",
                "Unsupported or oversized history; do not discard operation records without reconciling effects.",
            ));
        }
        state.used_operations = saved.used_operations;
        for id in &state.used_operations {
            validate_id(id)?;
        }
        for session in saved.sessions {
            validate_id(&session.id)?;
            validate_id(&session.name)?;
            state.sessions.insert(
                session.id.clone(),
                Session {
                    id: session.id,
                    name: session.name,
                    profile: Profile {
                        ttl_seconds: session.ttl_seconds,
                        ..Profile::default()
                    },
                    started: Instant::now(),
                    created_at: session.created_at,
                    status: match session.status.as_str() {
                        "stopped" => "stopped",
                        "expired" => "expired",
                        _ => "interrupted",
                    },
                    cache: Cache::default(),
                },
            );
        }
        for (id, op) in saved.operations {
            validate_id(&id)?;
            if !state.sessions.contains_key(&op.session) || !state.used_operations.contains(&id) {
                return Err(Failure::new(
                    "journal_invalid",
                    "Operation history is inconsistent.",
                ));
            }
            state.operations.insert(
                id,
                Operation {
                    session: op.session,
                    status: match op.status.as_str() {
                        "succeeded" => "succeeded",
                        "failed" => "failed",
                        _ => "unknown",
                    },
                    started_at: op.started_at,
                    started: Instant::now(),
                    duration_ms: op.duration_ms.or(Some(0)),
                    finished_at: op.finished_at,
                    exit_code: op.exit_code,
                    control: Arc::new(Mutex::new(None)),
                },
            );
        }
        state.events = saved.events;
        state.sequence = saved.sequence;
        state.denied = saved.denied;
        state.record("recovered", None, None);
        state.persist()?;
        Ok(state)
    }

    fn resume(&mut self, session: &str, mut profile: Profile) -> Result<Value, Failure> {
        let id = self.session_id(session)?;
        self.protect_runtime(&mut profile)?;
        profile.validate()?;
        let entry = self
            .sessions
            .get_mut(&id)
            .ok_or_else(|| Failure::new("unknown_session", "Session not found."))?;
        if entry.status == "active"
            || self
                .operations
                .values()
                .any(|op| op.session == id && op.active())
        {
            return Err(Failure::new(
                "session_active",
                "Stop the current session before replacing its profile.",
            ));
        }
        entry.profile = profile;
        entry.status = "active";
        entry.started = Instant::now();
        entry.created_at = timestamp();
        entry.cache.clear();
        self.record("resumed", Some(&id), None);
        Ok(self.session_view(&id))
    }

    fn refresh(&mut self, session: &str) -> Result<Value, Failure> {
        let id = self.session_id(session)?;
        if let Some(entry) = self.sessions.get_mut(&id) {
            entry.cache.clear();
        }
        self.record("cache_cleared", Some(&id), None);
        Ok(json!({"status":"cleared","session":id}))
    }

    fn prune(&mut self, keep: usize) -> Result<Value, Failure> {
        if keep > MAX_OPERATIONS {
            return Err(Failure::new(
                "retention",
                "Retention exceeds the operation limit.",
            ));
        }
        let mut finished: Vec<_> = self
            .operations
            .iter()
            .filter(|(_, op)| !op.active())
            .map(|(id, op)| (op.finished_at.unwrap_or(op.started_at), id.clone()))
            .collect();
        finished.sort();
        let remove = finished.len().saturating_sub(keep);
        for (_, id) in finished.into_iter().take(remove) {
            self.operations.remove(&id);
        }
        self.sessions.retain(|id, s| {
            s.status == "active" || self.operations.values().any(|op| op.session == *id)
        });
        self.events.clear();
        self.record("history_pruned", None, None);
        Ok(
            json!({"removed":remove,"retained":self.operations.len(),"reserved_ids":self.used_operations.len()}),
        )
    }

    fn inspect(&self, session: &str) -> Result<Value, Failure> {
        let id = self.session_id(session)?;
        let Some(entry) = self.sessions.get(&id) else {
            return Err(Failure::new("unknown_session", "Session not found."));
        };
        let mut environment = vec![
            json!({"name":"PATH","source":"fixed","presence":true,"precedence":0,"value":"/usr/bin:/bin"}),
            json!({"name":"LANG","source":"fixed","presence":true,"precedence":0,"value":"C"}),
        ];
        for (name, value) in &entry.profile.environment {
            let mut item = json!({"name":name,"source":"profile","presence":true,"precedence":1});
            if entry.profile.expose_environment.contains(name) {
                item["value"] = json!(value);
            }
            environment.push(item);
        }
        let expires = entry.cache.fetched_at.map(|at| {
            at.saturating_add(entry.profile.cache_ttl_seconds)
                .min(entry.created_at.saturating_add(entry.profile.ttl_seconds))
        });
        for name in entry.profile.credentials.keys() {
            environment.push(json!({"name":name,"source":entry.profile.provider,"declared":true,"presence":if entry.cache.values.is_some(){"cached"}else{"resolved_per_operation"},"precedence":2,"expires_at":expires}));
        }
        if entry.profile.ssh_auth_sock.is_some() {
            environment.push(
                json!({"name":"SSH_AUTH_SOCK","source":"ssh_agent","presence":true,"precedence":2}),
            );
        }
        Ok(
            json!({"session":self.session_view(&id),"environment":environment,"credential_cache":if entry.profile.cache_ttl_seconds==0{"none"}else{"memory"},"cache_ttl_seconds":entry.profile.cache_ttl_seconds,"cache_expires_at":expires,"values_available":false,"profile_loaded":entry.status=="active","sandbox_enabled":entry.profile.sandbox.enabled}),
        )
    }

    fn cache_resolved(
        &mut self,
        session: &str,
        generation: u64,
        credentials: BTreeMap<String, Vec<u8>>,
    ) {
        self.expire(Instant::now());
        if let Some(entry) = self.sessions.get_mut(session)
            && entry.status == "active"
            && entry.cache.generation == generation
            && entry.profile.cache_ttl_seconds > 0
            && credentials.keys().eq(entry.profile.credentials.keys())
            && credentials.values().map(Vec::len).sum::<usize>() <= 65_536
        {
            entry.cache.values = Some(credentials);
            entry.cache.fetched = Some(Instant::now());
            entry.cache.fetched_at = Some(timestamp());
            self.record("credentials_cached", Some(session), None);
        }
    }
}

fn dispatch(state: &mut State, request: Request) -> Result<Value, Failure> {
    state.expire(Instant::now());
    if state.stopping {
        return Err(Failure::new("service_stopping", "The service is stopping."));
    }
    match request {
        Request::Ping {} => Ok(json!({"status":"running","version":env!("CARGO_PKG_VERSION"),
            "statistics":{"accepted":state.used_operations.len(),"denied":state.denied,
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
        Request::Inspect { session } => state.inspect(&session),
        Request::Refresh { session } => state.refresh(&session),
        Request::Resume { session, profile } => state.resume(&session, profile),
        Request::Prune { keep } => state.prune(keep),
        Request::Input { session, operation, data, eof } => {
            if data.len() > 8192 { return Err(Failure::new("input_limit", "Input frame is too large.")); }
            control(state,&session,&operation,&execution::WorkerControl::Input{data,eof})
        }
        Request::Resize { session, operation, rows, cols } => {
            if rows == 0 || cols == 0 || rows > 1000 || cols > 1000 { return Err(Failure::new("terminal_size", "Invalid terminal size.")); }
            control(state,&session,&operation,&execution::WorkerControl::Resize{rows,cols})
        }
        Request::Signal { session, operation, signal } => {
            if !matches!(signal, 1 | 2 | 3 | 15) {
                return Err(Failure::new("invalid_signal", "Only HUP, INT, QUIT, and TERM can be forwarded."));
            }
            control(state,&session,&operation,&execution::WorkerControl::Signal{signal})
        }
        Request::Run { .. } | Request::Shell { .. } => Err(Failure::new("invalid_request", "Invalid request.")),
    }
}

fn control(
    state: &mut State,
    session: &str,
    operation: &str,
    message: &execution::WorkerControl,
) -> Result<Value, Failure> {
    let id = state.session_id(session)?;
    let op = state
        .operations
        .get(operation)
        .filter(|op| op.session == id && op.active())
        .ok_or_else(|| Failure::new("operation_inactive", "Operation is no longer running."))?;
    let mut control = lock(&op.control);
    let pipe = control
        .as_mut()
        .ok_or_else(|| Failure::new("operation_inactive", "Operation is not ready."))?;
    write_frame(pipe, message)?;
    drop(control);
    if matches!(message, execution::WorkerControl::Signal { .. }) {
        state.record("signaled", Some(&id), Some(operation));
    }
    Ok(json!({"status":"delivered"}))
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
    input: InputMode,
) -> Result<(Child, ChildStdout, u64), Failure> {
    let mut state = lock(state);
    state.expire(Instant::now());
    let profile = state
        .sessions
        .get(session)
        .filter(|s| s.status == "active" && !state.stopping)
        .ok_or_else(|| Failure::new("session_inactive", "Session is stopped or expired."))?;
    // spawn only starts a guardian; provider resolution happens after this lock is released.
    let generation = profile.cache.generation;
    let worker = execution::spawn(&profile.profile, argv, profile.cache.values.clone(), input)?;
    if let Some(op) = state.operations.get_mut(operation) {
        op.status = "running";
        *lock(&op.control) = Some(worker.control);
    }
    drop(state);
    Ok((worker.child, worker.output, generation))
}

fn run_operation(
    state: &Shared,
    mut stream: Option<UnixStream>,
    session: &str,
    operation: &str,
    argv: &[String],
    input: InputMode,
) {
    let worker = launch(state, session, operation, argv, input);
    let (mut child, output, generation) = match worker {
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
        let frame = read_frame::<_, execution::WorkerFrame>(&mut output);
        if let Ok(execution::WorkerFrame::Resolved { credentials }) = frame {
            lock(state).cache_resolved(session, generation, credentials);
            continue;
        }
        match frame.map(|frame| match frame {
            execution::WorkerFrame::Output { response } => response,
            execution::WorkerFrame::Resolved { .. } => Response::Error {
                code: "worker_protocol".into(),
                message: "Invalid worker frame.".into(),
            },
        }) {
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
                if lock(state).journal_failed {
                    send(
                        &mut stream,
                        &error_response(Failure::new(
                            "outcome_unknown",
                            "History could not be committed; inspect operation status.",
                        )),
                    );
                } else {
                    send(&mut stream, &Response::Finished { exit_code });
                }
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
    let request = match request {
        Request::Shell {
            session,
            operation,
            script,
            input,
        } => {
            let result = {
                let state = lock(state);
                state.session_id(&session).and_then(|id| {
                    state
                        .sessions
                        .get(&id)
                        .ok_or_else(|| Failure::new("unknown_session", "Session not found."))
                        .and_then(|s| s.profile.shell_command(&script))
                })
            };
            match result {
                Ok(argv) => Request::Run {
                    session,
                    operation,
                    argv,
                    input,
                },
                Err(error) => {
                    let mut state = lock(state);
                    state.deny(&session, &operation);
                    drop(state);
                    let _ = write_frame(&mut stream, &error_response(error));
                    return;
                }
            }
        }
        request => request,
    };
    if let Request::Run {
        session,
        operation,
        mut argv,
        input,
    } = request
    {
        let reserved = normalize_executable(&mut argv).and_then(|()| {
            let mut state = lock(state);
            state.expire(Instant::now());
            state.reserve(&session, &operation, &argv)
        });
        match reserved {
            Ok(id) => run_operation(state, Some(stream), &id, &operation, &argv, input),
            Err(error) => {
                let mut state = lock(state);
                state.deny(&session, &operation);
                drop(state);
                let _ = write_frame(&mut stream, &error_response(error));
            }
        }
    } else {
        let result = {
            let mut state = lock(state);
            let before = state.sequence;
            let result = dispatch(&mut state, request);
            if before == state.sequence {
                result
            } else {
                state.persist().and(result)
            }
        };
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
    nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_CORE, 0, 0)
        .map_err(|_| Failure::new("service_error", "Cannot disable core dumps."))?;
    let (listener, _lock, _socket) = listener(runtime)?;
    let state = Arc::new(Mutex::new(State::recover(runtime)?));
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))
            .map_err(|_| Failure::new("service_error", "Cannot register service signals."))?;
    }
    let clients = Arc::new(AtomicUsize::new(0));
    loop {
        {
            let mut state = lock(&state);
            let sequence = state.sequence;
            state.expire(Instant::now());
            if stopping.load(Ordering::Relaxed) {
                state.shutdown();
            }
            if state.sequence != sequence && state.persist().is_err() {
                state.shutdown();
            }
            if state.stopping {
                break;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // macOS inherits a listener's nonblocking flag on accepted sockets.
                // Framed writes must complete under the per-connection deadlines.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
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
            ..Profile::default()
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
                cache: Cache::default(),
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
            matches!(launch(&state, "session", "operation", &argv, InputMode::Null), Err(error) if error.code == "session_inactive")
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
