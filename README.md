# Latchrun

Scoped credentials. Persistent sessions. Controlled execution.

Latchrun is a local Rust service for running approved commands with scoped credentials, reusable work sessions, and secret-safe activity metadata. It supports macOS and Linux, fake credentials for testing, 1Password CLI references, and an existing SSH agent.

**Status: first usable CLI/service implementation.** Sessions, execution, recovery inspection, provider integration, exact command policy, and bounded activity history are implemented. A dashboard and OS sandbox remain future work. The trust boundary is the local user and approved child programs: a child receiving a credential can disclose it, and another process running as the same user can control Latchrun.

See [BUILD_BRIEF.md](BUILD_BRIEF.md) for scope and [the runtime decision](docs/decisions/0003-session-and-execution-contract.md) for the lifecycle and security contract.

## Try it without a vault

After building, run these from the checkout:

```sh
./target/release/latchrun service start
./target/release/latchrun session start demo --profile examples/fake.json
./target/release/latchrun run demo --operation demo-1 -- /usr/bin/printenv TEST_SECRET
./target/release/latchrun session reconnect demo
./target/release/latchrun inspect demo
./target/release/latchrun events demo
./target/release/latchrun session stop demo
./target/release/latchrun service stop
```

The command receives a fake credential and prints `[REDACTED]`. A session can be addressed by its name or returned random ID. Names cannot be reused during one service lifetime. Reusing `demo-1` is rejected; each deliberate new execution needs a new operation ID. Omitting `--operation` creates an ID and prints it on stderr before submission.

`service start` starts a background service; `service serve` runs it in the foreground. `service status` reports counts and health. The default runtime is `/tmp/latchrun-<uid>`; use a short absolute path with `--runtime-dir PATH` before the command, or `LATCHRUN_RUNTIME_DIR`. Its parent must exist, and an existing runtime directory must be owned by you with mode `0700`. Socket mode is `0600`. The runtime contains only an empty lock file and a socket.

## Profiles and policy

Profiles are JSON snapshots loaded at session creation. Editing a file does not alter an existing session. Unknown fields are rejected. [examples/fake.json](examples/fake.json) is immediately runnable; the other examples require your executable, project, socket, and reference paths.

```json
{
  "project": "/tmp",
  "purpose": "Check credential delivery with a fake value",
  "ttl_seconds": 3600,
  "timeout_seconds": 300,
  "provider": "fake",
  "credentials": {"TEST_SECRET": "fake://demo"},
  "commands": [{"executable": "/usr/bin/printenv", "args": ["TEST_SECRET"]}]
}
```

The executable must be an absolute existing executable; its canonical path and **every argument** must match one complete rule. The working directory is fixed to the canonical project directory. There is no implicit shell expansion, prefix matching, arbitrary environment override, or caller-selected working directory. Shells/scripts can be explicitly approved, but their contents, dependencies, hooks, configuration, and filesystem changes remain trusted. Policy does not pin executable contents or restrict what an approved command can do.

Children start with an empty environment, then receive `PATH=/usr/bin:/bin`, `LANG=C`, declared credential variables, and an optional approved `SSH_AUTH_SOCK`. Parent-shell variables are not inherited. Environment names use uppercase ASCII letters, digits, and underscores; dynamic-loader variables, common shell-control variables, and Latchrun/1Password control variables are rejected. This is not a comprehensive restriction on interpreter configuration variables. Do not put actual secret values in profiles, names, arguments, paths, or operation IDs. The fake provider maps `fake://demo` to the deliberately public value `latchrun-fake-demo`.

Sessions default to one hour; commands default to five minutes, including provider lookup. Both limits accept 1–86400 seconds. Session expiry and stop deny further execution and terminate active process groups. No PTY is allocated: stdin is null and stdout/stderr are pipes; interactive execution is unsupported. stdout and stderr stream separately with exact secret-value redaction, including values split across reads; normal exit codes and `128 + signal` propagate to the caller. INT, TERM, and HUP sent to the CLI are forwarded, with forced cleanup after a short grace period.

## 1Password and supported workflow recipes

Set `provider` to `one_password`, `op_path` to the absolute official `op` executable, and credential values to `op://vault/item/field` references. Each operation resolves only its declared references with [`op read --no-newline`](https://www.1password.dev/cli/reference/commands/read). Authentication belongs to the official CLI and desktop app; unlock/sign in there before running. Provider stdout never goes to the caller, provider stderr is discarded, and failure returns a fixed actionable message. Lookup has a 30-second per-reference bound and a 64 KiB total secret limit. Ambient `OP_*` tokens are not inherited. Provider integration is tested with an executable fixture; no live vault has been accessed for automated validation.

There is **no secret cache between operations**. Repeated runs reuse session policy and whatever authorization 1Password itself currently allows. Latchrun neither polls to keep authorization alive nor detects a password-manager lock for already running commands. A delivered credential remains in the child until that process ends; stopping Latchrun does not revoke a remote credential.

- **Git over SSH:** adapt [examples/git-ssh.json](examples/git-ssh.json) with your project, Git executable, repository, and [1Password SSH-agent socket](https://www.1password.dev/ssh/agent). Run the exact approved `git ls-remote` command. The existing SSH agent handles keys and prompts; Latchrun never reads the private key. No `op_path` is needed when `credentials` is empty. SSH receives access to that agent's permitted identities, not a key-specific capability. HTTPS authentication requires a separately approved credential-aware helper and is not implemented by this SSH recipe.
- **Homelab object storage:** adapt [examples/homelab-s3.json](examples/homelab-s3.json) for a read-only S3-compatible account and the AWS CLI. Run its exact `s3 ls` command; the AWS CLI reads the declared access-key environment variables. Grant only the necessary bucket permissions at the remote service. The example uses a fictional endpoint and references. Neither this network workflow nor the Git workflow has been exercised against private infrastructure here.

Install optional workflow tools yourself and use their verified absolute paths. Routine development and tests do not require 1Password, Git credentials, AWS CLI, or network infrastructure.

## Recovery and observability

`session reconnect` is status inspection. A client disconnect does not stop an accepted operation; output is discarded after the connection fails, and execution continues under the session/command deadlines. Reconnect shows the operation's status, duration, and exit code. Duplicate operation IDs are rejected across all sessions for the entire service lifetime, including failed and unknown outcomes. No command is automatically replayed.

A service crash loses all session/history state. Surviving independent workers observe the closed control pipes and terminate ordinary descendants. Killing or crashing a guardian itself can orphan its command and remove its deadline enforcement; the service reports an unknown outcome, and OS/user cleanup may be necessary. Restart removes a stale socket only while holding the service lock. Old session IDs become unknown; consult the target system before deliberately repeating a command whose effect may have happened. A new service cannot deduplicate IDs from a prior service lifetime.

`inspect` lists declared credential names and sources, never values or references. No credential is fetched for inspection. Status/events omit purpose, project paths, executables, arguments, output, and references; names/IDs are user-visible metadata. Events retain the newest 512 entries in memory. The service admits at most 128 sessions, 1024 operation records, 32 active operations, and 64 connected clients. On capacity exhaustion it denies new work rather than forgetting operation IDs. Restart after reviewing completed operations to reset history.

The private socket protects against other ordinary OS users, not hostile same-user callers. Exact command checks are not a filesystem/network sandbox or a protected-path boundary. Process-group cleanup covers ordinary descendants; hostile programs can escape groups or leak secrets via encoding, files, or network traffic. Only exact known values are redacted from output, not arbitrary secrets an approved program can discover. There is no persistent output log, raw-secret endpoint, dashboard, remote service, automatic retry, or automatic startup at login.

## Development

Install [mise](https://mise.jdx.dev/getting-started.html), then from this checkout:

```sh
mise trust
mise install
mise run check
mise run build
./target/release/latchrun --help
```

The project pins stable Rust 1.97.1, including rustfmt, Clippy, Rust Analyzer, and rust-src. Cargo.lock is tracked. Runtime dependencies are Serde/serde_json for bounded typed IPC and profiles, nix for safe Unix process/lock/resource operations, and signal-hook for safe signal handling. Production code forbids unsafe Rust. Use `mise run fmt` to format and `mise run test` for fake-only CLI, redaction, and deterministic lifecycle tests. No library target or documentation tests are currently needed.

## CI

`mise run ci` runs the Linux pipeline through Dagger 0.21.9 and its Dang SDK. Start a compatible container engine first. On macOS, Colima with its Docker runtime is supported by the development workflow; native Apple Container requires separate Dagger compatibility setup. Plain native checks do not need a container engine.

GitHub Actions runs the Dagger pipeline on Linux and native checks on macOS. Both check formatting, compilation, Clippy, tests, the release build, and help output. CI needs no 1Password credentials or Dagger Cloud token. The workflow's actions are pinned to commits.

Keep Rust pins aligned in Cargo.toml, rust-toolchain.toml, and mise.toml. Keep the Dagger pin aligned in mise.toml, dagger.json, and the workflow. The Dagger build context explicitly includes source and configuration; extend its allowlist when adding tests or compile-time inputs.

## Contributing

Read [AGENTS.md](AGENTS.md). Use fake credentials for tests; never put real secrets in arguments, fixtures, output, or tracked files. Disposable experiments belong in ignored `scratch/`.

## License

Licensed under the [MIT License](LICENSE). See the [publication review](docs/publication-review.md) for the initial content inventory and the [publication decision](docs/decisions/0002-publication-and-license.md) for the rationale.
