use std::{
    env,
    fs::File,
    io::{self, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::protocol::{
    Failure, InputMode, MAX_FRAME, Profile, Request, Response, prepare_runtime, random_id,
    read_frame, validate_id, write_frame,
};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH},
    iterator::Signals,
};

pub fn main_cli() -> Result<i32, Failure> {
    let mut args: Vec<String> = env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().map_err(|_| usage()))
        .collect::<Result<_, _>>()?;
    let runtime = if args.first().is_some_and(|arg| arg == "--runtime-dir") {
        if args.len() < 2 {
            return Err(usage());
        }
        let path = PathBuf::from(args.remove(1));
        args.remove(0);
        path
    } else {
        env::var_os("LATCHRUN_RUNTIME_DIR").map_or_else(
            || PathBuf::from(format!("/tmp/latchrun-{}", nix::unistd::getuid())),
            PathBuf::from,
        )
    };
    dispatch_cli(&runtime, &args)
}

fn dispatch_cli(runtime: &Path, args: &[String]) -> Result<i32, Failure> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["--help" | "-h"] => {
            print_help();
            Ok(0)
        }
        ["--version" | "-V"] => {
            println!("latchrun {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        ["service", "serve"] => {
            prepare_runtime(runtime)?;
            crate::service::serve(runtime)?;
            Ok(0)
        }
        ["service", "start"] => {
            start_service(runtime)?;
            Ok(0)
        }
        ["service", "status"] => show(runtime, &Request::Ping {}),
        ["service", "stop"] => show(runtime, &Request::Shutdown {}),
        ["session", "start", name, "--profile", path] => start_session(runtime, name, path),
        ["dashboard", "serve"] => {
            crate::dashboard::serve(runtime, 0)?;
            Ok(0)
        }
        ["dashboard", "serve", "--port", port] => {
            crate::dashboard::serve(runtime, port.parse().map_err(|_| usage())?)?;
            Ok(0)
        }
        ["agent", "serve"] => {
            crate::agent::serve(runtime)?;
            Ok(0)
        }
        ["session", "refresh", session] => show(
            runtime,
            &Request::Refresh {
                session: (*session).into(),
            },
        ),
        ["session", "resume", session, "--profile", path] => show(
            runtime,
            &Request::Resume {
                session: (*session).into(),
                profile: load_profile(Path::new(path))?,
            },
        ),
        ["history", "prune", "--keep", keep] => show(
            runtime,
            &Request::Prune {
                keep: keep.parse().map_err(|_| usage())?,
            },
        ),
        ["session", "status"] => show(runtime, &Request::Status { session: None }),
        ["session", "status" | "reconnect", session] => {
            validate_id(session)?;
            show(
                runtime,
                &Request::Status {
                    session: Some((*session).into()),
                },
            )
        }
        ["session", "stop", session] => {
            validate_id(session)?;
            show(
                runtime,
                &Request::Stop {
                    session: (*session).into(),
                },
            )
        }
        ["events"] => show(runtime, &Request::Events { session: None }),
        ["events", session] => {
            validate_id(session)?;
            show(
                runtime,
                &Request::Events {
                    session: Some((*session).into()),
                },
            )
        }
        ["inspect", session] => {
            validate_id(session)?;
            show(
                runtime,
                &Request::Inspect {
                    session: (*session).into(),
                },
            )
        }
        ["run", session, rest @ ..] => run(runtime, session, rest),
        _ => Err(usage()),
    }
}

fn start_session(runtime: &Path, name: &str, path: &str) -> Result<i32, Failure> {
    validate_id(name)?;
    show(
        runtime,
        &Request::Start {
            name: name.into(),
            profile: load_profile(Path::new(path))?,
        },
    )
}

fn load_profile(path: &Path) -> Result<Profile, Failure> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAME {
        return Err(Failure::new(
            "invalid_profile",
            "Profile exceeds the size limit.",
        ));
    }
    let mut profile: Profile = serde_json::from_slice(&bytes).map_err(|_| {
        Failure::new(
            "invalid_profile",
            "Profile must be valid JSON with supported fields.",
        )
    })?;
    profile.validate()?;
    Ok(profile)
}

fn usage() -> Failure {
    Failure::new("usage", "Invalid arguments. Use latchrun --help.")
}

pub fn connect(runtime: &Path) -> Result<UnixStream, Failure> {
    prepare_runtime(runtime)?;
    let socket = runtime.join("service.sock");
    let metadata = socket.symlink_metadata().map_err(|_| unavailable())?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != nix::unistd::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Failure::new(
            "unsafe_socket",
            "Service socket must be private and owned by this user.",
        ));
    }
    let stream = UnixStream::connect(socket).map_err(|_| unavailable())?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn unavailable() -> Failure {
    Failure::new(
        "service_unavailable",
        "Start the local service with latchrun service start. After a crash, previous operation outcomes are unknown; do not automatically replay them.",
    )
}

pub fn request(runtime: &Path, request: &Request) -> Result<Response, Failure> {
    let mut stream = connect(runtime)?;
    write_frame(&mut stream, request)?;
    read_frame(&mut BufReader::new(stream))
}

fn show(runtime: &Path, query: &Request) -> Result<i32, Failure> {
    match request(runtime, query)? {
        Response::Ok { data } => {
            serde_json::to_writer(io::stdout().lock(), &data)
                .map_err(|_| Failure::new("io", "Could not write metadata."))?;
            println!();
            Ok(0)
        }
        Response::Error { code, message } => Err(Failure { code, message }),
        _ => Err(Failure::new("protocol", "Unexpected service response.")),
    }
}

fn start_service(runtime: &Path) -> Result<(), Failure> {
    prepare_runtime(runtime)?;
    if matches!(request(runtime, &Request::Ping {}), Ok(Response::Ok { .. })) {
        show(runtime, &Request::Ping {})?;
        return Ok(());
    }
    let mut child = Command::new(env::current_exe()?)
        .args([
            "--runtime-dir",
            runtime.to_str().ok_or_else(usage)?,
            "service",
            "serve",
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(request(runtime, &Request::Ping {}), Ok(Response::Ok { .. })) {
            show(runtime, &Request::Ping {})?;
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            return Err(Failure::new(
                "service_start",
                "Service failed to start; run service serve to inspect safe diagnostics.",
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::new("service_start", "Service startup timed out."));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn run(runtime: &Path, session: &str, arguments: &[&str]) -> Result<i32, Failure> {
    validate_id(session)?;
    let mut operation = None;
    let mut input = InputMode::Null;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index] {
            "--operation" => {
                index += 1;
                let id = arguments.get(index).ok_or_else(usage)?;
                validate_id(id)?;
                operation = Some((*id).to_owned());
            }
            "--stdin" if input == InputMode::Null => input = InputMode::Pipe,
            "--tty" if input == InputMode::Null => input = InputMode::Tty,
            "--" | "--shell" => break,
            _ => return Err(usage()),
        }
        index += 1;
    }
    let operation = if let Some(id) = operation {
        id
    } else {
        let id = random_id()?;
        eprintln!("latchrun: operation {id}");
        id
    };
    let query = match arguments.get(index..) {
        Some(["--", argv @ ..]) if !argv.is_empty() => Request::Run {
            session: session.into(),
            operation: operation.clone(),
            argv: argv.iter().map(|s| (*s).into()).collect(),
            input,
        },
        Some(["--shell", script]) => Request::Shell {
            session: session.into(),
            operation: operation.clone(),
            script: (*script).into(),
            input,
        },
        _ => return Err(usage()),
    };
    let _terminal = if input == InputMode::Tty {
        Some(crate::terminal::RawMode::enable()?)
    } else {
        None
    };
    let mut stream = connect(runtime)?;
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGWINCH])?;
    let signal_handle = signals.handle();
    write_frame(&mut stream, &query)?;
    stream.set_read_timeout(None)?;
    let mut reader = BufReader::new(stream);
    let result = (|| {
        match read_frame(&mut reader)? {
            Response::Accepted {
                operation: accepted,
            } if accepted == operation => (),
            Response::Error { code, message } => return Err(Failure { code, message }),
            _ => return Err(Failure::new("protocol", "Unexpected service response.")),
        }
        if input != InputMode::Null {
            forward_input(runtime, session, &operation);
        }
        if input == InputMode::Tty {
            resize(runtime, session, &operation);
        }
        let signal_runtime = runtime.to_owned();
        let signal_session = session.to_owned();
        let signal_operation = operation.clone();
        let signal_thread = thread::spawn(move || {
            for signal in signals.forever() {
                if signal == SIGWINCH {
                    resize(&signal_runtime, &signal_session, &signal_operation);
                    continue;
                }
                let _ = request(
                    &signal_runtime,
                    &Request::Signal {
                        session: signal_session.clone(),
                        operation: signal_operation.clone(),
                        signal,
                    },
                );
            }
        });
        let result = receive_output(&mut reader);
        signal_handle.close();
        let _ = signal_thread.join();
        result
    })();
    signal_handle.close();
    result.map_err(|error| {
        if matches!(error.code.as_str(), "disconnected" | "io") {
            Failure::new("outcome_unknown", "Response lost. Reconnect to the session and inspect the operation ID; never automatically replay a command.")
        } else { error }
    })
}

fn resize(runtime: &Path, session: &str, operation: &str) {
    let (rows, cols) = crate::terminal::size();
    let _ = request(
        runtime,
        &Request::Resize {
            session: session.into(),
            operation: operation.into(),
            rows,
            cols,
        },
    );
}
fn forward_input(runtime: &Path, session: &str, operation: &str) {
    let runtime = runtime.to_owned();
    let session = session.to_owned();
    let operation = operation.to_owned();
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0u8; 8192];
        while let Ok(size) = input.read(&mut buffer) {
            let result = request(
                &runtime,
                &Request::Input {
                    session: session.clone(),
                    operation: operation.clone(),
                    data: buffer[..size].to_vec(),
                    eof: size == 0,
                },
            );
            if size == 0 || !matches!(result, Ok(Response::Ok { .. })) {
                break;
            }
        }
    });
}

fn receive_output(reader: &mut BufReader<UnixStream>) -> Result<i32, Failure> {
    loop {
        match read_frame(reader)? {
            Response::Output { stream, data } => match stream.as_str() {
                "stdout" => {
                    let mut out = io::stdout().lock();
                    out.write_all(&data)?;
                    out.flush()?;
                }
                "stderr" => {
                    let mut out = io::stderr().lock();
                    out.write_all(&data)?;
                    out.flush()?;
                }
                _ => return Err(Failure::new("protocol", "Invalid output stream.")),
            },
            Response::Finished { exit_code } if (0..=255).contains(&exit_code) => {
                return Ok(exit_code);
            }
            Response::Error { code, message } => return Err(Failure { code, message }),
            _ => return Err(Failure::new("protocol", "Unexpected execution response.")),
        }
    }
}

fn print_help() {
    println!(
        "Latchrun — scoped credentials and persistent local work sessions.\n\n\
Usage: latchrun [--runtime-dir PATH] COMMAND\n\n\
  service start | serve | status | stop\n\
  session start NAME --profile FILE\n\
  session status [SESSION] | reconnect SESSION | stop SESSION\n\
  run SESSION [--operation ID] [--stdin | --tty] -- /absolute/executable [ARG ...]\n\
  run SESSION [--operation ID] [--stdin | --tty] --shell SCRIPT\n\
  session refresh SESSION | resume SESSION --profile FILE\n\
  dashboard serve [--port PORT]\n\
  agent serve\n\
  history prune --keep COUNT\n\
  inspect SESSION\n\
  events [SESSION]\n\
  --help | --version\n\n\
Profiles are JSON. Commands and arguments require an exact allowlist match.\n\
Runs default to null stdin; --stdin streams input and --tty allocates a terminal.\n\
Reconnect inspects status; it never replays commands or retained output.\n\
Sessions expire after 1 hour by default. Credential caching is explicit and bounded.\n\
Enable an OS sandbox explicitly in a profile to enforce filesystem/network policy.\n\
See README.md for profiles, recovery and the 1Password/SSH workflows."
    );
}
