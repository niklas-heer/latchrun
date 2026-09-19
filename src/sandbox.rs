//! OS confinement for approved commands. Providers remain outside this boundary.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use std::fmt::Write as _;

use crate::protocol::{Failure, Profile};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Deny,
    Allow,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub read_paths: Vec<PathBuf>,
    #[serde(default)]
    pub write_paths: Vec<PathBuf>,
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
}

/// Keep this object alive until spawning: the Linux filter descriptor is owned here.
pub struct PreparedCommand {
    pub command: Command,
    #[cfg(target_os = "linux")]
    _files: Vec<std::os::fd::OwnedFd>,
}

pub fn validate(policy: &mut SandboxPolicy, project: &Path) -> Result<(), Failure> {
    if !policy.enabled {
        if !policy.read_paths.is_empty()
            || !policy.write_paths.is_empty()
            || !policy.protected_paths.is_empty()
        {
            return Err(invalid());
        }
        return Ok(());
    }
    for (paths, limit) in [
        (&mut policy.read_paths, 64),
        (&mut policy.write_paths, 32),
        (&mut policy.protected_paths, 128),
    ] {
        if paths.len() > limit {
            return Err(invalid());
        }
        for path in paths.iter_mut() {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|s| s.len() > 4096 || s.chars().any(char::is_control))
            {
                return Err(invalid());
            }
            *path = fs::canonicalize(&*path).map_err(|_| invalid())?;
            if path == Path::new("/") {
                return Err(invalid());
            }
        }
        paths.sort();
        paths.dedup();
    }
    if policy
        .protected_paths
        .iter()
        .any(|path| project.starts_with(path))
    {
        return Err(invalid());
    }
    let mut protected = Vec::<PathBuf>::new();
    for path in &policy.protected_paths {
        if !protected.iter().any(|parent| path.starts_with(parent)) {
            protected.push(path.clone());
        }
    }
    policy.protected_paths = protected;
    Ok(())
}

fn invalid() -> Failure {
    Failure::new(
        "invalid_sandbox",
        "Sandbox paths must be existing absolute paths; protected paths require an enabled sandbox and cannot contain the project root.",
    )
}

fn unavailable() -> Failure {
    Failure::new(
        "sandbox_unavailable",
        "The required OS sandbox is unavailable. Install bubblewrap on Linux or use a macOS version providing sandbox-exec; execution was denied.",
    )
}

pub fn prepare(profile: &Profile, argv: &[String]) -> Result<PreparedCommand, Failure> {
    prepare_command(profile, argv, false)
}

/// The caller must already own an isolated synthetic controlling terminal.
pub fn prepare_terminal(profile: &Profile, argv: &[String]) -> Result<PreparedCommand, Failure> {
    prepare_command(profile, argv, true)
}

fn prepare_command(
    profile: &Profile,
    argv: &[String],
    terminal: bool,
) -> Result<PreparedCommand, Failure> {
    let (program, arguments) = argv.split_first().ok_or_else(invalid)?;
    if !profile.sandbox.enabled {
        let mut command = Command::new(program);
        command.args(arguments);
        return Ok(PreparedCommand {
            command,
            #[cfg(target_os = "linux")]
            _files: Vec::new(),
        });
    }
    if profile.sandbox.network == NetworkPolicy::Deny && profile.ssh_auth_sock.is_some() {
        return Err(Failure::new(
            "invalid_sandbox",
            "An SSH agent socket requires sandbox network policy allow.",
        ));
    }
    platform_prepare(profile, argv, terminal)
}

#[cfg(target_os = "macos")]
fn platform_prepare(
    profile: &Profile,
    argv: &[String],
    terminal: bool,
) -> Result<PreparedCommand, Failure> {
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err(unavailable());
    }
    // dyld-support supplies Apple's current runtime bootstrap permissions, but
    // unlike system.sb does not grant access to preferences, XPC agents or syslog.
    let mut rules = String::from(
        "(version 1)\n(deny default)\n(import \"dyld-support.sb\")\n(allow syscall*)\n(allow mach-bootstrap)\n(allow process-fork process-exec)\n(allow signal (target same-sandbox))\n(allow sysctl-read)\n(allow file-read-metadata)\n",
    );
    for path in [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/usr/share",
        "/System/Library",
        "/System/Cryptexes",
        "/System/Volumes/Preboot",
        "/Library/Apple",
    ] {
        allow_path(
            &mut rules,
            "file-read* file-map-executable",
            Path::new(path),
        )?;
    }
    rules.push_str("(allow file-read* (literal \"/private/var/select/sh\") (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/random\") (literal \"/dev/urandom\"))\n(allow file-read-data file-write-data (subpath \"/dev/fd\"))\n(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\"))\n");
    if terminal {
        rules.push_str("(allow file-read* file-write-data file-ioctl (literal \"/dev/tty\"))\n");
    }
    allow_path(
        &mut rules,
        "file-read* file-map-executable",
        &profile.project,
    )?;
    for path in &profile.sandbox.read_paths {
        allow_path(&mut rules, "file-read* file-map-executable", path)?;
    }
    for path in &profile.sandbox.write_paths {
        allow_path(
            &mut rules,
            "file-read* file-write* file-map-executable",
            path,
        )?;
    }
    if profile.sandbox.network == NetworkPolicy::Allow {
        rules.push_str("(allow network*)\n");
        if let Some(socket) = &profile.ssh_auth_sock {
            allow_path(&mut rules, "file-read* file-write*", socket)?;
        }
    }
    for path in &profile.sandbox.protected_paths {
        writeln!(
            rules,
            "(deny file-read* file-write* file-map-executable (subpath {quoted}))\n(deny network-outbound (subpath {quoted}))",
            quoted = sbpl_string(path)?
        )
        .map_err(|_| invalid())?;
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &rules]).args(argv);
    Ok(PreparedCommand { command })
}

#[cfg(target_os = "macos")]
fn allow_path(rules: &mut String, operations: &str, path: &Path) -> Result<(), Failure> {
    writeln!(
        rules,
        "(allow {operations} (subpath {}))",
        sbpl_string(path)?
    )
    .map_err(|_| invalid())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn sbpl_string(path: &Path) -> Result<String, Failure> {
    let value = path.to_str().ok_or_else(invalid)?;
    if value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "linux")]
fn platform_prepare(
    profile: &Profile,
    argv: &[String],
    terminal: bool,
) -> Result<PreparedCommand, Failure> {
    use std::os::fd::AsRawFd;
    if !Path::new("/usr/bin/bwrap").is_file() {
        return Err(unavailable());
    }
    let mut command = Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--cap-drop",
        "ALL",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]);
    if !terminal {
        command.arg("--new-session");
    }
    for path in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
    ] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command
        .arg("--ro-bind")
        .arg(&profile.project)
        .arg(&profile.project);
    for path in &profile.sandbox.read_paths {
        command.arg("--ro-bind").arg(path).arg(path);
    }
    for path in &profile.sandbox.write_paths {
        command.arg("--bind").arg(path).arg(path);
    }
    let mut files = Vec::new();
    if profile.sandbox.network == NetworkPolicy::Allow {
        command.arg("--share-net");
        for path in [
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/ssl/certs",
        ] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        if let Some(socket) = &profile.ssh_auth_sock {
            command.arg("--ro-bind").arg(socket).arg(socket);
        }
    } else {
        let fd = network_filter()?;
        command.args(["--seccomp", &fd.as_raw_fd().to_string()]);
        files.push(fd);
    }
    // Masks follow all grants. Mountpoints cannot be removed or renamed by the
    // child, and directory masks are read-only to prevent replacement contents.
    for path in &profile.sandbox.protected_paths {
        if path.is_dir() {
            command
                .arg("--tmpfs")
                .arg(path)
                .arg("--remount-ro")
                .arg(path);
        } else {
            use nix::fcntl::{FcntlArg, FdFlag, fcntl};
            let empty: std::os::fd::OwnedFd = fs::File::open("/dev/null")
                .map_err(|_| unavailable())?
                .into();
            fcntl(&empty, FcntlArg::F_SETFD(FdFlag::empty())).map_err(|_| unavailable())?;
            command
                .arg("--ro-bind-data")
                .arg(empty.as_raw_fd().to_string())
                .arg(path);
            files.push(empty);
        }
    }
    command
        .arg("--chdir")
        .arg(&profile.project)
        .arg("--")
        .args(argv);
    Ok(PreparedCommand {
        command,
        _files: files,
    })
}

#[cfg(target_os = "linux")]
fn network_filter() -> Result<std::os::fd::OwnedFd, Failure> {
    use nix::{
        fcntl::{FcntlArg, FdFlag, fcntl},
        unistd::{pipe, write},
    };
    // Classic BPF over seccomp_data: reject foreign syscall architectures and
    // the x32 ABI, then deny socket/socketpair with EPERM. No application FFI.
    #[cfg(target_arch = "x86_64")]
    let architecture = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    let architecture = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(unavailable());
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let socket = u32::try_from(nix::libc::SYS_socket).map_err(|_| unavailable())?;
        let socketpair = u32::try_from(nix::libc::SYS_socketpair).map_err(|_| unavailable())?;
        let io_uring_setup =
            u32::try_from(nix::libc::SYS_io_uring_setup).map_err(|_| unavailable())?;
        let instructions: [(u16, u8, u8, u32); 11] = [
            (0x20, 0, 0, 4),
            (0x15, 1, 0, architecture),
            (0x06, 0, 0, 0x8000_0000),
            (0x20, 0, 0, 0),
            (0x45, 0, 1, 0x4000_0000),
            (0x06, 0, 0, 0x8000_0000),
            (0x15, 3, 0, socket),
            (0x15, 2, 0, socketpair),
            (0x15, 1, 0, io_uring_setup),
            (0x06, 0, 0, 0x7fff_0000),
            (0x06, 0, 0, 0x0005_0001),
        ];
        let mut bytes = Vec::with_capacity(instructions.len() * 8);
        for (code, yes, no, value) in instructions {
            bytes.extend_from_slice(&code.to_ne_bytes());
            bytes.extend_from_slice(&[yes, no]);
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        let (read, writer) = pipe().map_err(|_| unavailable())?;
        let count = write(&writer, &bytes).map_err(|_| unavailable())?;
        if count != bytes.len() {
            return Err(unavailable());
        }
        drop(writer);
        fcntl(&read, FcntlArg::F_SETFD(FdFlag::empty())).map_err(|_| unavailable())?;
        Ok(read)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_prepare(_: &Profile, _: &[String], _: bool) -> Result<PreparedCommand, Failure> {
    Err(unavailable())
}
