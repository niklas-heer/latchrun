#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;
use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Shell {
    name: &'static str,
    path: PathBuf,
    options: &'static [&'static str],
}
impl Shell {
    fn installed() -> Vec<Self> {
        let mut shells = Vec::new();
        for (name, options) in [
            ("bash", &["--noprofile", "--norc"][..]),
            ("zsh", &["-f"][..]),
            ("fish", &["--no-config"][..]),
        ] {
            let key = format!("LATCHRUN_TEST_{}", name.to_ascii_uppercase());
            let configured = env::var_os(&key).map(PathBuf::from);
            let path = configured.clone().or_else(|| {
                ["/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"]
                    .iter()
                    .map(|directory| Path::new(directory).join(name))
                    .find(|path| path.is_file())
            });
            let Some(path) = path else {
                assert!(
                    env::var_os("LATCHRUN_REQUIRE_SHELLS").is_none(),
                    "required {name} is missing; set {key} to an absolute executable"
                );
                eprintln!(
                    "SKIP {name}: unavailable; set {key} or install it, then require the full matrix with LATCHRUN_REQUIRE_SHELLS=1"
                );
                continue;
            };
            assert!(path.is_absolute() && path.is_file(), "invalid {key}");
            let version = Command::new(&path).arg("--version").output().unwrap();
            assert!(version.status.success(), "cannot execute {name}");
            eprintln!(
                "shell matrix: {}",
                String::from_utf8_lossy(&version.stdout).trim()
            );
            shells.push(Self {
                name,
                path: path.canonicalize().unwrap(),
                options,
            });
        }
        assert!(!shells.is_empty(), "no supported test shell is installed");
        shells
    }
    fn quote(&self, text: &str) -> String {
        if self.name == "fish" {
            format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
        } else {
            format!("'{}'", text.replace('\'', "'\\''"))
        }
    }
    fn caller(&self, service: &Service, arguments: &[&str]) -> Output {
        let command = std::iter::once(env!("CARGO_BIN_EXE_latchrun"))
            .chain(["--runtime-dir", service.root.to_str().unwrap()])
            .chain(arguments.iter().copied())
            .map(|arg| self.quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let script = if self.name == "fish" {
            format!(
                "set -q TEST_SECRET; and exit 81; {command}; set result $status; set -q TEST_SECRET; and exit 82; exit $result"
            )
        } else {
            format!(
                "test -z \"${{TEST_SECRET+x}}\" || exit 81; {command}; result=$?; test -z \"${{TEST_SECRET+x}}\" || exit 82; exit \"$result\""
            )
        };
        Command::new(&self.path)
            .args(self.options)
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &service.project)
            .env("XDG_CONFIG_HOME", &service.project)
            .env("ZDOTDIR", &service.project)
            .current_dir(&service.project)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

struct Service {
    root: PathBuf,
    project: PathBuf,
    daemon: Child,
}
impl Service {
    fn start() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/lr-shells-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.with_extension("project");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&project).unwrap();
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
            .command(&["service", "status"])
            .output()
            .unwrap()
            .status
            .success()
        {
            assert!(Instant::now() < deadline, "service readiness timed out");
            thread::sleep(Duration::from_millis(10));
        }
        service
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latchrun"));
        command
            .env_clear()
            .arg("--runtime-dir")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn session(
        &self,
        shell: &Shell,
        name: &str,
        scripts: &[String],
        sandbox: bool,
        literals: &[&str],
    ) {
        let options = shell
            .options
            .iter()
            .copied()
            .chain(["-c"])
            .collect::<Vec<_>>();
        let mut commands = scripts
            .iter()
            .map(|script| {
                let mut args = options.clone();
                args.push(script);
                json!({"executable":shell.path,"args":args})
            })
            .collect::<Vec<_>>();
        commands.push(json!({"executable":"/usr/bin/printenv","args":["TEST_SECRET"]}));
        commands.push(json!({"executable":"/usr/bin/printf","args":std::iter::once("%s\n").chain(literals.iter().copied()).collect::<Vec<_>>()}));
        let mut read_paths = Vec::new();
        if sandbox {
            read_paths.push(shell.path.parent().unwrap().parent().unwrap().to_path_buf());
            for prefix in ["/opt/homebrew", "/usr/local"] {
                if shell.path.starts_with(Path::new(prefix).join("Cellar")) {
                    read_paths.push(PathBuf::from(prefix));
                }
            }
            read_paths.retain(|path| path != Path::new("/"));
        }
        let profile = json!({"project":self.project,"purpose":"isolated shell compatibility","provider":"fake",
            "credentials":{"TEST_SECRET":"fake://shell"},"timeout_seconds":5,
            "environment":{"XDG_CONFIG_HOME":self.project,"XDG_DATA_HOME":self.project},
            "shell":{"executable":shell.path,"args":options},"commands":commands,
            "sandbox":{"enabled":sandbox,"read_paths":read_paths}});
        let path = self.project.join(format!("{name}.json"));
        fs::write(&path, profile.to_string()).unwrap();
        let result = self
            .command(&[
                "session",
                "start",
                name,
                "--profile",
                path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        successful(&result, shell.name);
    }
    fn tty(&self, session: &str, script: &str) -> String {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_latchrun"));
        command.env_clear();
        command.env("TERM", "xterm-256color");
        command.args([
            "--runtime-dir",
            self.root.to_str().unwrap(),
            "run",
            session,
            "--tty",
            "--shell",
            script,
        ]);
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let reading = thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                }
            }
            output
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("shell terminal timed out");
            }
            thread::sleep(Duration::from_millis(10));
        };
        drop(pair.master);
        let output = String::from_utf8(reading.join().unwrap()).unwrap();
        assert!(status.success(), "{output}");
        output
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.command(&["service", "stop"]).output();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.project);
    }
}
fn successful(output: &Output, shell: &str) {
    assert!(
        output.status.success(),
        "{shell}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn supported_shells_preserve_argv_credentials_pipe_exit_tty_and_sandbox() {
    let literals = [
        "two words",
        "quote'and\"double",
        "$HOME $(touch should-not-exist)",
        "`printf no`; *",
        "back\\slash",
        "line\nbreak",
    ];
    for shell in Shell::installed() {
        let service = Service::start();
        let secret = "printf '%s\\n' \"$TEST_SECRET\"".to_owned();
        let pipe = "cat; printf 'eof\\n'".to_owned();
        let exit = "exit 7".to_owned();
        let signal = format!(
            "kill -TERM {}",
            if shell.name == "fish" {
                "$fish_pid"
            } else {
                "$$"
            }
        );
        let tty = "test -t 0 && test -t 1 || exit 91; printf 'tty-ok\\n'; printf '%s\\n' \"$TEST_SECRET\"".to_owned();
        service.session(
            &shell,
            "work",
            &[
                secret.clone(),
                pipe.clone(),
                exit.clone(),
                signal.clone(),
                tty.clone(),
            ],
            false,
            &literals,
        );
        let mut arguments = vec!["run", "work", "--", "/usr/bin/printf", "%s\n"];
        arguments.extend(literals);
        let result = shell.caller(&service, &arguments);
        successful(&result, shell.name);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            format!("{}\n", literals.join("\n"))
        );
        assert!(!service.project.join("should-not-exist").exists());
        let result = shell.caller(
            &service,
            &["run", "work", "--", "/usr/bin/printenv", "TEST_SECRET"],
        );
        successful(&result, shell.name);
        assert_eq!(result.stdout, b"[REDACTED]\n");
        let result = service
            .command(&["run", "work", "--shell", &secret])
            .output()
            .unwrap();
        successful(&result, shell.name);
        assert_eq!(result.stdout, b"[REDACTED]\n");
        let mut command = service
            .command(&["run", "work", "--stdin", "--shell", &pipe])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        command
            .stdin
            .take()
            .unwrap()
            .write_all(b"pipe-data\n")
            .unwrap();
        let result = command.wait_with_output().unwrap();
        successful(&result, shell.name);
        assert_eq!(result.stdout, b"pipe-data\neof\n");
        let result = shell.caller(&service, &["run", "work", "--shell", &exit]);
        assert_eq!(result.status.code(), Some(7), "{}", shell.name);
        let result = service
            .command(&["run", "work", "--shell", &signal])
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(143),
            "{}: {}",
            shell.name,
            String::from_utf8_lossy(&result.stderr)
        );
        let output = service.tty("work", &tty);
        assert!(output.contains("tty-ok"));
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("latchrun-fake-shell"));
        sandbox_smoke(&service, &shell, &secret, &tty);
    }
}

fn sandbox_smoke(service: &Service, shell: &Shell, secret: &str, tty: &str) {
    service.session(
        shell,
        "sandbox",
        &[secret.to_owned(), tty.to_owned()],
        true,
        &[],
    );
    let result = service
        .command(&["run", "sandbox", "--shell", secret])
        .output()
        .unwrap();
    if env::var_os("LATCHRUN_TEST_SANDBOX_UNAVAILABLE").is_some() {
        assert!(!result.status.success());
    } else {
        successful(&result, shell.name);
        assert_eq!(result.stdout, b"[REDACTED]\n");
        let output = service.tty("sandbox", tty);
        assert!(output.contains("tty-ok"));
        assert!(output.contains("[REDACTED]"));
    }
}
