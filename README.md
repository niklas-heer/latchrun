<p align="center">
  <img src="docs/assets/latchrun-icon.svg" alt="Latchrun icon: a latch framing a play arrow" width="104" height="104">
</p>

<h1 align="center">Latchrun</h1>
<p align="center"><strong>Scoped credentials. Persistent sessions. Controlled execution.</strong></p>
<p align="center">
  <a href="#install">Install</a> ·
  <a href="#try-it-without-a-vault">Quick start</a> ·
  <a href="docs/dashboard.md">Dashboard</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

Give an approved command the credentials it needs, then see what happened. Latchrun is a local Rust service for people and agents working on macOS and Linux. Create a scoped session, run exact approved commands, and reconnect without accidentally repeating an operation.

- **Keep credentials scoped.** Resolve from 1Password, password-store or an existing private file; optionally reuse them in a short-lived memory cache.
- **Work in your terminal.** Use Bash, Zsh or Fish, stream stdin, or allocate an interactive terminal. Git SSH and HTTPS workflows are supported.
- **Inspect the work.** Use the local dashboard, CLI or MCP adapter for session state, outcomes, latency, cache reuse and reported agent activity.
- **Recover deliberately.** Durable operation IDs prevent replay after connection loss or restart. Optional OS sandboxing restricts filesystem and network access.

**v0.1.0 is an early release.** The current [build brief](BUILD_BRIEF.md) is implemented and tested; the CLI, profile and integration APIs may change before 1.0. See the [changelog](CHANGELOG.md) and [release policy](docs/releasing.md).

## Install

```sh
brew install niklas-heer/tap/latchrun
latchrun --version
```

Homebrew installs a prebuilt binary for macOS 15+ (Apple Silicon or Intel) and Linux with glibc 2.39+ (ARM64 or x86_64); no Rust toolchain is needed. You can also download archives and checksums from [GitHub Releases](https://github.com/niklas-heer/latchrun/releases). See [release platform requirements](docs/releasing.md#build-and-publish) for details.

For a source build, clone this repository and follow [Development and CI](#development-and-ci). Credential providers and sandbox backends are optional dependencies: install the tools for the workflows you enable. Latchrun does not start a service at login.

## Try it without a vault

Download the public fake profile, then start a session:

```sh
curl --fail --location --output latchrun-demo.json \
  https://raw.githubusercontent.com/niklas-heer/latchrun/v0.1.0/examples/fake.json
latchrun service start
latchrun session start demo --profile latchrun-demo.json
latchrun run demo --operation demo-1 -- /usr/bin/printenv TEST_SECRET
latchrun session reconnect demo
latchrun inspect demo
latchrun dashboard serve
```

The command receives the public fake credential `latchrun-fake-demo`; its output is `[REDACTED]`. Open the private URL printed by `dashboard serve` to explore sessions and analytics. That command runs in the foreground; press Ctrl-C to close the dashboard, then clean up:

```sh
latchrun session stop demo
latchrun service stop
```

From a source checkout, use `examples/fake.json` directly and replace `latchrun` with `./target/release/latchrun` if it is not on PATH. Address sessions by name or their returned random ID. Every deliberate execution needs a new operation ID: reusing `demo-1` is rejected, including after service restart. Omitting `--operation` generates an ID and prints it on stderr before submission.

## Compatibility and measured latency

Bash, Zsh and Fish are tested as both caller shells and explicitly configured child shells, including stdin, exit status, PTYs and sandboxed execution:

| Platform tested | Bash | Zsh | Fish |
| --- | --- | --- | --- |
| macOS arm64 | 3.2.57 | 5.9 | 4.9.3 |
| Linux arm64, Dagger | 5.2.37 | 5.9 | 4.0.2 |

These are verified versions, not a certification of every shell or startup configuration. See [shell compatibility](docs/shells.md) for the required CI matrix and limits.

On an Apple M4 Mac, the release benchmark measured **53.5 ms median added latency** for a warm service running a command without credentials, with normal journal and analytics persistence. That is one host and 100 paired measurements, not a universal latency promise. Provider lookup, unlocking, sandboxing and workload can add time. See the [full baseline and reproducible benchmark](docs/latency.md).

## Safety and scope

The OS user and profile authors remain trusted. Exact command matching controls which command may run; an approved program, shell, hook or configuration can still perform arbitrary behavior. A command receiving a credential can disclose it. Optional sandboxing restricts its filesystem and network access, and exact-value output redaction is a safeguard with documented limits.

Latchrun does not confine every process or agent running as your user, revoke remote credentials, or automatically observe unrelated tools. See the [sandbox guide](docs/sandbox.md), [implementation contract](docs/decisions/2026-09-19_192325581_full-runtime-and-integrations.md) and [analytics scope](docs/analytics.md).

## Service and local storage

`service start` starts a background service; `service serve` runs it in the foreground. `service status` reports health and counts. The default runtime is `/tmp/latchrun-<uid>`; select another with `--runtime-dir PATH` before the command, or `LATCHRUN_RUNTIME_DIR`. Use an absolute path no longer than 80 bytes whose parent exists. An existing runtime directory must be owned by you, mode `0700`, and not a symlink. The socket and metadata journal are owner-only. The runtime contains the lock, socket, `history.json`, and transient journal files during atomic writes. Durable usage analytics live separately in `analytics.sqlite3`; see [data paths and retention](docs/analytics.md#storage-and-lifecycle). Keep the runtime between restarts to retain operation-ID protection; `/tmp` can be cleared by the OS.

## Profiles and exact command policy

Profiles are JSON snapshots loaded at session creation or explicit resume. Editing the file does not alter an active session. Unknown fields are rejected. [examples/fake.json](examples/fake.json) is immediately runnable:

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

An executable must be an existing absolute executable. Its canonical path and **every argument** must match one complete command rule. The working directory is the canonical project directory. There is no prefix matching, implicit shell evaluation, caller-selected working directory, or ambient environment inheritance. Executable contents, scripts, hooks and configuration are not pinned; approve their behavior as well as their names.

Children receive `HOME` from the current OS user, `PATH=/usr/bin:/bin`, `LANG=C`, the profile's explicit non-secret `environment`, declared credentials, and an optional approved `ssh_auth_sock`. `HOME` comes from the account database, never from the caller's environment; it is fixed by Latchrun, cannot be declared by a profile, and is absent when the account has no home entry. `inspect` lists it as a fixed variable. The home path does not grant additional filesystem access under the optional sandbox. `expose_environment` selects only declared non-secret values to show in inspection and the dashboard; it cannot expose credential values. Environment names use uppercase ASCII letters, digits and underscores. Loader, shell-control and internal control names are reserved, and secret/non-secret declarations cannot overlap. Do not put real secrets in profiles, arguments, names, paths or operation IDs.

Session TTL defaults to 3600 seconds; operation timeout defaults to 300 seconds and includes credential lookup. Both accept 1–86400 seconds. Stop and expiry deny new execution and terminate active commands. Session refresh clears credentials cached for future runs; it does not extend the session TTL or recall credentials already delivered.

## Providers and credential lifetime

Each profile chooses one provider for its declared credentials:

| Provider | Reference and configuration | Behavior |
| --- | --- | --- |
| `fake` | `fake://demo` | Returns the public test value `latchrun-fake-demo`. |
| `one_password` | `op://vault/item/field`, explicit absolute `op_path` | Uses official `op read --no-newline`; unlock/sign in through the official CLI/app. |
| `file` | `file:///absolute/path` | Reads an existing same-user regular file, with no symlink or group/other permissions; preserves all bytes, including a trailing newline. |
| `password_store` | `pass://entry`, explicit absolute `provider_path` to `pass` | Runs `pass show entry`; uses only the first line and discards notes. Traversal, absolute entries and option-like entries are rejected. |

The 1Password adapter follows the official [read command](https://www.1password.dev/cli/reference/commands/read). External providers receive a minimal environment with HOME from the OS account; ambient `OP_*` tokens and parent-shell configuration are not inherited. Provider stdout is private and bounded, stderr is discarded, and failures return fixed instructions. Each external reference has a 30-second timeout within the operation deadline. Empty values, NUL bytes and total credentials above 64 KiB are rejected. The file provider reads operator-managed files; Latchrun does not create a credential store.

`cache_ttl_seconds` defaults to **0**, meaning a fresh lookup for every operation. Set 1–900 to opt into a session-scoped, in-memory cache. Its absolute deadline starts at resolution and is not extended by reuse. Refresh, stop, expiry, resume and service restart clear it; a late lookup cannot repopulate a cleared generation. Cached values are never journaled or returned by inspection. Core dumps are disabled for the service, guardians and their children; memory zeroization and protection from OS swap are not promised.

Latchrun does not keep provider authorization alive or detect password-manager lock for cached/already delivered values. Provider lock and rotation affect fresh lookups according to provider policy. Stopping a child does not revoke its credential at the remote service.

## Interactive input and shells

Default stdin is null. `--stdin` streams input through a bounded pipe; `--tty` allocates a terminal, forwards resize events, and restores the caller's terminal settings on exit. TTY execution merges stdout/stderr and requires a client terminal. Normal runs stream them separately. Exit codes and `128 + signal` propagate; INT, TERM and HUP are forwarded. Cancellation escalates from TERM to KILL after a short grace period.

A profile can configure `shell: {"executable":"/bin/sh","args":["-c"]}`. `--shell SCRIPT` appends SCRIPT as one argument and applies the same exact command rules. Configuring a shell does not authorize arbitrary scripts.

The runnable [interactive example](examples/fake-interactive.json) demonstrates a 60-second fake cache, an exposed non-secret variable, pipe input and an explicitly approved shell:

```sh
latchrun session start interactive --profile examples/fake-interactive.json
printf 'hello\n' | latchrun run interactive --stdin -- /bin/cat
latchrun run interactive --shell 'printf "%s\n" "$APP_MODE" "$TEST_SECRET"'
latchrun run interactive --tty --shell 'printf "Name: "; read name; printf "hello %s\n" "$name"'
latchrun session refresh interactive
latchrun session stop interactive
```

Bash, Zsh and Fish are covered as caller shells and configured child shells, including pipe input, exit status, PTY and sandbox execution. See [shell compatibility and the required CI matrix](docs/shells.md) for tested versions and limits.

Exact known secret values are redacted across read boundaries, including terminal newline conversion. Encoded or partial secrets, values split between stdout and stderr, and other credentials discovered by a child are outside that safeguard. No command output is retained in history.

## Sandbox and workflow recipes

Set `sandbox.enabled` to opt into OS enforcement. The project is readable by default; declare writable directories, additional readable installations and protected paths. Network defaults to deny, including host loopback and filesystem Unix sockets. `network: "allow"` grants general network access, not a host allowlist. A missing or incompatible backend fails closed.

macOS uses Apple's deprecated `/usr/bin/sandbox-exec`; Linux uses `/usr/bin/bwrap`, namespaces and a network-denial seccomp filter. Platform behavior and host requirements differ. Read [the sandbox guide](docs/sandbox.md) before adapting [examples/fake-sandbox.json](examples/fake-sandbox.json). Runtime control files, the analytics data directory and file-provider credential files are automatically protected. Providers run outside the command sandbox. Same-user processes outside it, readable hardlink aliases/copies, and remote credential authority remain outside its protection.

- **Git SSH:** adapt [examples/git-ssh.json](examples/git-ssh.json) with the project, executable, repository and existing [1Password SSH-agent socket](https://www.1password.dev/ssh/agent). Latchrun supplies the socket without extracting private keys. The child can use the identities that agent permits; this is not a per-key capability. A sandboxed SSH workflow needs network allow.
- **Git HTTPS:** adapt [examples/git-https.json](examples/git-https.json). `git_https` selects an exact HTTPS host, username and declared `token_env`. A built-in helper supplies the token over Git's private credential pipe; it resets inherited helpers, disables interactive Git prompts, and performs no credential-store writes. Matching is host-scoped, not repository-scoped. The token must be a nonempty UTF-8 single line. `GIT_*` profile variables are disallowed with this integration. Git hooks/configuration remain trusted.
- **Object storage:** adapt [examples/homelab-s3.json](examples/homelab-s3.json) for a read-only S3-compatible account and an absolute AWS CLI executable. The approved command reads only its declared access-key variables. Apply remote bucket permissions independently of local command policy.

Except for the fake examples, these are templates with fictional references and paths. Install optional tools yourself and adapt paths to verified installations. Automated tests use fake credentials and executable fixtures; real provider or network checks require separately authorized targets.

## Recovery, history and inspection

A client disconnect does not stop an accepted operation. Execution continues under its deadlines, and output is discarded after the connection fails. `session reconnect SESSION` inspects status and outcomes; it never replays output or a command. Treat a lost response as uncertain, then inspect status and reconcile any remote effect before choosing a new operation ID.

Operation IDs are durably reserved before launch. The owner-only journal stores bounded session/operation metadata and used-ID tombstones, never profiles, provider references, credentials, caches, environment values, argv or output. Unsafe, corrupt or inconsistent journals and persistence failures fail closed. After service restart, previously active sessions are `interrupted`, and unfinished operations are `unknown`. Completed outcomes and all reserved IDs remain known. Reactivation requires an explicit profile:

```sh
latchrun session resume demo --profile examples/fake.json
latchrun session reconnect demo
latchrun history prune --keep 100
```

Resume applies a new validated profile and TTL to an inactive session, clears its cache, preserves its identity/history, and executes nothing. Pruning removes older finalized operation details and empty inactive sessions but preserves used-ID tombstones. A retained session name cannot be reused until its old session is pruned. Deleting the journal/runtime loses deduplication protection; never use deletion as an automatic recovery or retry procedure.

Limits are 128 retained sessions, 1024 operation details, 65536 reserved operation IDs, 512 events, 32 active operations and 64 connected service clients. Capacity exhaustion denies new work. Prune completed details when needed; ID tombstones remain bounded and are never silently evicted.

Independent guardians observe service control-pipe closure and clean up ordinary descendants, including terminal job groups. Killing a guardian itself can orphan its command and remove deadline enforcement. A hostile unsandboxed child can escape process groups/sessions; process cleanup is not a sandbox. Such uncertain outcomes require explicit reconciliation and possibly OS cleanup.

`inspect` reports environment declarations, provenance, expiry and explicitly exposed non-secret values without fetching credentials. Status/events omit project paths, purpose, commands, arguments, references and output. Names and IDs are visible metadata. Durable analytics cover mediated commands, automatically observed MCP calls and explicitly reported external tool activity. They do not automatically observe all tools on the machine.

## Durable analytics

`latchrun stats [--days 1|7|30|90]` reports command outcomes, latency percentiles, cache hit rates, time buckets and agent/tool usage. The default window is seven days. Analytics survive service restart and `history prune` in a private SQLite database; no command arguments, output, credentials or references are recorded. `latchrun data path` shows its location.

The default data directory is `$XDG_CONFIG_HOME/latchrun` or the OS account's `~/.config/latchrun`. Use `--data-dir PATH` or `LATCHRUN_DATA_DIR` to override it. An explicit runtime also becomes the data directory unless overridden, keeping test/custom runtimes isolated. Rows are retained indefinitely; historical activity absent from the journal cannot be reconstructed. Each data directory permits one active daemon; concurrent services need separate data directories. Stop/restart older daemons to enable the new analytics protocol; `service start` does not replace an already running daemon. See [upgrade and recovery details](docs/analytics.md#upgrading-a-running-service).

The MCP adapter records advertised tool calls automatically. Other integrations can report non-sensitive metadata through `activity record --id ID --agent NAME --tool NAME --duration-ms N --outcome success|error`. Identical reports deduplicate; changed payloads under an existing ID fail. External reports are self-reported, not independently observed behavior. See [analytics definitions, storage and reporting](docs/analytics.md).

## Dashboard and agent integration

Run `latchrun dashboard serve` and open the private startup URL. The loopback-only dashboard shows historical outcome charts, latency percentiles, cache reuse and reported agent activity alongside live sessions, events and environment provenance, with authenticated stop/refresh controls. Its ephemeral URL capability authorizes those controls; do not share it or expose the listener through a proxy. See [dashboard usage and browser authorization](docs/dashboard.md).

Run `latchrun agent serve` as a stdio MCP server for an agent. It implements MCP 2025-11-25 tools for status, events, inspection, analytics, stop, refresh and exact execution in operator-created sessions. It does not create profiles/sessions, expose credentials, or support interactive input. See [agent setup and retry rules](docs/agent-integration.md).

The private socket and dashboard capability protect against other ordinary users or unauthorized browser origins. Neither isolates hostile processes running as the service user. There is no remote service, automatic command replay, automatic startup at login, or whole-machine confinement of the calling agent.

## Development and CI

Install [mise](https://mise.jdx.dev/getting-started.html), then:

```sh
mise trust
mise install
mise run check
mise run build
./target/release/latchrun --help
```

Stable Rust 1.97.1, rustfmt, Clippy, Rust Analyzer and rust-src are pinned; Cargo.lock is tracked. Production code forbids unsafe Rust. Serde handles typed bounded IPC, nix handles safe Unix interfaces, signal-hook forwards signals, portable-pty/terminal_size support terminals, and rusqlite with bundled SQLite stores durable usage metadata. Use `mise run fmt` to format and `mise run test` for fake-only CLI, lifecycle, recovery, provider, terminal, dashboard, agent and sandbox tests. Native sandbox tests need a functioning OS backend; install bubblewrap on Linux.

`mise run bench` measures release-build CLI overhead against direct execution with fake providers, caching, persistence and the native sandbox. See [latency methodology and measured baseline](docs/latency.md). Dashboard command durations are not a measurement of added CLI overhead.

`mise run ci` runs Linux checks through Dagger 0.21.9 and its Dang SDK. Start a compatible container engine first; Colima with Docker is supported on macOS. The Linux check installs bubblewrap and enables container root capabilities for nested sandbox tests: use a trusted checkout and disposable engine/VM. This CI capability is not required by the normal service. See [sandbox test requirements](docs/sandbox.md#boundaries-and-tests).

GitHub Actions retains native macOS and Linux Dagger coverage for formatting, compilation, Clippy, tests, release build and help output. CI needs no vault credentials or Dagger Cloud token. Actions are pinned to commits. Keep Rust pins aligned in Cargo.toml, rust-toolchain.toml and mise.toml; keep Dagger aligned in mise.toml, dagger.json and the workflow. Extend the explicit Dagger build-context allowlist when adding compile-time inputs.

Read [AGENTS.md](AGENTS.md). Never put real secrets in fixtures, output or tracked files. Disposable experiments belong in ignored `scratch/`.

## Releases and contributions

Releases use Semantic Versioning and a [maintained changelog](CHANGELOG.md). During the `0.x` series, minor releases may change public interfaces; review the notes before upgrading. Commit messages follow Conventional Commits, such as `feat: add a provider` or `fix: preserve terminal exit status`. See [release and compatibility policy](docs/releasing.md) for the tagging and publication process.

## License

[MIT](LICENSE). Binary archives also include [dependency license notices](THIRD_PARTY_LICENSES.txt). See the initial [publication review](docs/publication-review.md) and [publication decision](docs/decisions/2026-09-19_192325569_public-repository-and-mit-license.md).
