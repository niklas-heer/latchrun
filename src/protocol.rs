use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, DirBuilder, File},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const MAX_FRAME: usize = 1_048_576;

#[derive(Debug)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Failure {}

impl From<std::io::Error> for Failure {
    fn from(_: std::io::Error) -> Self {
        Self::new(
            "io",
            "Local I/O failed; check service status and permissions.",
        )
    }
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Fake,
    OnePassword,
    File,
    PasswordStore,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Null,
    Pipe,
    Tty,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHttps {
    pub host: String,
    pub username: String,
    pub token_env: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub project: PathBuf,
    pub purpose: String,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
    pub provider: Provider,
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
    pub commands: Vec<CommandRule>,
    #[serde(default)]
    pub op_path: Option<PathBuf>,
    #[serde(default)]
    pub ssh_auth_sock: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub cache_ttl_seconds: u64,
    #[serde(default)]
    pub provider_path: Option<PathBuf>,
    #[serde(default)]
    pub shell: Option<ShellConfig>,
    #[serde(default)]
    pub sandbox: crate::sandbox::SandboxPolicy,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub expose_environment: Vec<String>,
    #[serde(default)]
    pub git_https: Option<GitHttps>,
}

const fn default_ttl() -> u64 {
    3600
}
const fn default_timeout() -> u64 {
    300
}

impl Profile {
    pub fn validate(&mut self) -> Result<(), Failure> {
        if !(1..=86400).contains(&self.ttl_seconds)
            || !(1..=86400).contains(&self.timeout_seconds)
            || self.purpose.is_empty()
            || self.purpose.len() > 256
            || self.credentials.len() > 32
            || self.commands.is_empty()
            || self.commands.len() > 64
            || self.cache_ttl_seconds > 900
        {
            return Err(invalid_profile());
        }
        self.project = canonical_absolute(&self.project)?;
        if !self.project.is_dir() {
            return Err(invalid_profile());
        }
        for rule in &mut self.commands {
            rule.executable = executable_path(&rule.executable)?;
            validate_args(&rule.args)?;
        }
        for (name, reference) in &self.credentials {
            if name == "TERM"
                || !valid_env(name)
                || reference.len() > 2048
                || reference.contains(['\0', '\n', '\r'])
            {
                return Err(invalid_profile());
            }
            match self.provider {
                Provider::Fake => validate_id(
                    reference
                        .strip_prefix("fake://")
                        .ok_or_else(invalid_profile)?,
                )?,
                Provider::OnePassword => {
                    if !reference.starts_with("op://") || reference.len() <= 5 {
                        return Err(invalid_profile());
                    }
                }
                Provider::File => {
                    let path = reference
                        .strip_prefix("file://")
                        .ok_or_else(invalid_profile)?;
                    if !Path::new(path).is_absolute() {
                        return Err(invalid_profile());
                    }
                }
                Provider::PasswordStore => {
                    let entry = reference
                        .strip_prefix("pass://")
                        .ok_or_else(invalid_profile)?;
                    if entry.is_empty()
                        || entry.starts_with(['-', '/'])
                        || entry.split('/').any(|part| part == ".." || part.is_empty())
                    {
                        return Err(invalid_profile());
                    }
                }
            }
        }
        if matches!(self.provider, Provider::OnePassword) && !self.credentials.is_empty() {
            self.op_path = Some(executable_path(
                self.op_path.as_ref().ok_or_else(invalid_profile)?,
            )?);
        } else if self.op_path.is_some() {
            return Err(invalid_profile());
        }
        if matches!(self.provider, Provider::PasswordStore) {
            self.provider_path = Some(executable_path(
                self.provider_path.as_ref().ok_or_else(invalid_profile)?,
            )?);
        } else if self.provider_path.is_some() {
            return Err(invalid_profile());
        }
        if let Some(shell) = &mut self.shell {
            shell.executable = executable_path(&shell.executable)?;
            validate_args(&shell.args)?;
        }
        self.validate_environment()?;
        if let Some(path) = &mut self.ssh_auth_sock {
            *path = canonical_absolute(path)?;
            if !fs::metadata(path)?.file_type().is_socket() {
                return Err(invalid_profile());
            }
        }
        crate::sandbox::validate(&mut self.sandbox, &self.project)?;
        Ok(())
    }

    fn validate_environment(&self) -> Result<(), Failure> {
        if self.environment.len() > 64 || self.expose_environment.len() > 64 {
            return Err(invalid_profile());
        }
        for (name, value) in &self.environment {
            if !valid_env(name)
                || self.credentials.contains_key(name)
                || value.len() > 4096
                || value.contains('\0')
            {
                return Err(invalid_profile());
            }
        }
        if self
            .expose_environment
            .iter()
            .any(|name| !self.environment.contains_key(name))
        {
            return Err(invalid_profile());
        }
        if let Some(git) = &self.git_https
            && (git.host.is_empty()
                || git.host.len() > 253
                || !git
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
                || git.username.is_empty()
                || git.username.len() > 256
                || git.username.contains(['\n', '\r', '\0'])
                || !self.credentials.contains_key(&git.token_env)
                || self
                    .environment
                    .keys()
                    .chain(self.credentials.keys())
                    .any(|name| name.starts_with("GIT_")))
        {
            return Err(invalid_profile());
        }
        Ok(())
    }

    pub fn shell_command(&self, script: &str) -> Result<Vec<String>, Failure> {
        let shell = self.shell.as_ref().ok_or_else(|| {
            Failure::new("shell_disabled", "No shell is configured for this session.")
        })?;
        let mut argv = vec![
            shell
                .executable
                .to_str()
                .ok_or_else(invalid_profile)?
                .to_owned(),
        ];
        argv.extend(shell.args.clone());
        argv.push(script.to_owned());
        self.authorize(&argv)?;
        Ok(argv)
    }

    pub fn authorize(&self, argv: &[String]) -> Result<(), Failure> {
        let denied = || {
            Failure::new(
                "policy_denied",
                "Executable and arguments must exactly match an approved command.",
            )
        };
        let (executable, arguments) = argv.split_first().ok_or_else(denied)?;
        let path = executable_path(Path::new(executable)).map_err(|_| denied())?;
        if self
            .commands
            .iter()
            .any(|rule| rule.executable == path && rule.args == arguments)
        {
            Ok(())
        } else {
            Err(denied())
        }
    }
}

fn invalid_profile() -> Failure {
    Failure::new(
        "invalid_profile",
        "Invalid profile; check absolute paths, references, environment names, command rules and lifetime limits.",
    )
}

fn canonical_absolute(path: &Path) -> Result<PathBuf, Failure> {
    if !path.is_absolute() {
        return Err(invalid_profile());
    }
    fs::canonicalize(path).map_err(|_| invalid_profile())
}

fn executable_path(path: &Path) -> Result<PathBuf, Failure> {
    let path = canonical_absolute(path)?;
    let metadata = fs::metadata(&path).map_err(|_| invalid_profile())?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(invalid_profile());
    }
    Ok(path)
}

fn validate_args(args: &[String]) -> Result<(), Failure> {
    if args.len() > 256 || args.iter().any(|s| s.len() > 16_384 || s.contains('\0')) {
        return Err(invalid_profile());
    }
    Ok(())
}

fn valid_env(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 128
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && ![
            "PATH",
            "HOME",
            "ENV",
            "BASH_ENV",
            "IFS",
            "SHELLOPTS",
            "BASHOPTS",
            "CDPATH",
            "GLOBIGNORE",
            "SSH_AUTH_SOCK",
            "LANG",
        ]
        .contains(&name)
        && !["LD_", "DYLD_", "OP_", "LATCHRUN_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping {},
    Shutdown {},
    Start {
        name: String,
        profile: Profile,
    },
    Status {
        session: Option<String>,
    },
    Stop {
        session: String,
    },
    Run {
        session: String,
        operation: String,
        argv: Vec<String>,
        #[serde(default)]
        input: InputMode,
    },
    Shell {
        session: String,
        operation: String,
        script: String,
        #[serde(default)]
        input: InputMode,
    },
    Input {
        session: String,
        operation: String,
        data: Vec<u8>,
        eof: bool,
    },
    Resize {
        session: String,
        operation: String,
        rows: u16,
        cols: u16,
    },
    Refresh {
        session: String,
    },
    Resume {
        session: String,
        profile: Profile,
    },
    Prune {
        keep: usize,
    },
    Signal {
        session: String,
        operation: String,
        signal: i32,
    },
    Events {
        session: Option<String>,
    },
    Inspect {
        session: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Ok { data: serde_json::Value },
    Error { code: String, message: String },
    Accepted { operation: String },
    Output { stream: String, data: Vec<u8> },
    Finished { exit_code: i32 },
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, Failure> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Err(Failure::new(
                "disconnected",
                "Connection ended; inspect operation status before deciding whether to retry.",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() >= MAX_FRAME {
            return Err(Failure::new(
                "invalid_frame",
                "IPC frame exceeds the size limit.",
            ));
        }
        bytes.push(byte[0]);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new("invalid_frame", "Malformed IPC message."))
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), Failure> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| Failure::new("encoding", "Message encoding failed."))?;
    if bytes.len() > MAX_FRAME {
        return Err(Failure::new(
            "invalid_frame",
            "IPC frame exceeds the size limit.",
        ));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn validate_id(value: &str) -> Result<(), Failure> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Failure::new(
            "invalid_id",
            "Identifiers must be 1–64 ASCII letters, digits, underscores or hyphens.",
        ));
    }
    Ok(())
}

pub fn random_id() -> Result<String, Failure> {
    use fmt::Write as _;
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut result = String::with_capacity(32);
    for byte in bytes {
        write!(result, "{byte:02x}")
            .map_err(|_| Failure::new("random", "Could not create an identifier."))?;
    }
    Ok(result)
}

pub fn prepare_runtime(path: &Path) -> Result<(), Failure> {
    let fail = || {
        Failure::new(
            "unsafe_runtime",
            "Runtime directory must be an absolute, private directory owned by this user (mode 0700), and must not itself be a symlink.",
        )
    };
    if !path.is_absolute() || path.as_os_str().len() > 80 {
        return Err(fail());
    }
    if !path.exists() {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(fail()),
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| fail())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(fail());
    }
    // /tmp and /var are platform symlinks on macOS; validate the final directory
    // and resolve parents before socket operations. Same-uid processes are trusted.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Result<Profile, Failure> {
        Ok(Profile {
            project: std::env::current_dir()?,
            purpose: "test".into(),
            ttl_seconds: 60,
            timeout_seconds: 30,
            provider: Provider::Fake,
            credentials: BTreeMap::from([("TOKEN".into(), "fake://test".into())]),
            commands: vec![CommandRule {
                executable: "/bin/echo".into(),
                args: vec!["approved".into()],
            }],
            op_path: None,
            ssh_auth_sock: None,
            ..Profile::default()
        })
    }

    #[test]
    fn policy_rejects_prefixes_extra_arguments_and_relative_executables() -> Result<(), Failure> {
        let mut profile = profile()?;
        profile.validate()?;
        profile.authorize(&["/bin/echo".into(), "approved".into()])?;
        for argv in [
            vec!["/bin/echo"],
            vec!["/bin/echo", "approved", "extra"],
            vec!["echo", "approved"],
            vec!["/bin/echo", "sensitive-denied-argument"],
        ] {
            let result =
                profile.authorize(&argv.into_iter().map(str::to_owned).collect::<Vec<_>>());
            assert!(
                matches!(result, Err(error) if error.code == "policy_denied" && !error.message.contains("sensitive-denied-argument"))
            );
        }
        Ok(())
    }

    #[test]
    fn dangerous_environment_names_and_cross_provider_references_fail_closed() -> Result<(), Failure>
    {
        for name in [
            "PATH",
            "HOME",
            "BASH_ENV",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "OP_SERVICE_ACCOUNT_TOKEN",
            "SSH_AUTH_SOCK",
            "LATCHRUN_RUNTIME_DIR",
            "BAD=NAME",
        ] {
            let mut profile = profile()?;
            profile.credentials = BTreeMap::from([(name.into(), "fake://test".into())]);
            assert!(profile.validate().is_err());
        }
        let mut profile = profile()?;
        profile
            .credentials
            .insert("TOKEN".into(), "op://vault/item/field".into());
        assert!(profile.validate().is_err());
        Ok(())
    }

    #[test]
    fn malformed_and_oversized_frames_never_echo_payload() {
        for payload in [
            b"sensitive-malformed-input\n".to_vec(),
            vec![b'x'; MAX_FRAME + 1],
            b"{\"type\":\"ping\",\"unknown\":\"sensitive-malformed-input\"}\n".to_vec(),
        ] {
            let result = read_frame::<_, Request>(&mut payload.as_slice());
            assert!(
                matches!(result, Err(error) if !error.message.contains("sensitive-malformed-input"))
            );
        }
        assert!(read_frame::<_, Request>(&mut b"{\"type\":\"ping\"}".as_slice()).is_err());
    }

    #[test]
    fn framing_preserves_following_messages() -> Result<(), Failure> {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Request::Ping {})?;
        write_frame(&mut buffer, &Request::Shutdown {})?;
        let mut input = buffer.as_slice();
        assert!(matches!(
            read_frame::<_, Request>(&mut input)?,
            Request::Ping {}
        ));
        assert!(matches!(
            read_frame::<_, Request>(&mut input)?,
            Request::Shutdown {}
        ));
        assert!(input.is_empty());
        Ok(())
    }
}
