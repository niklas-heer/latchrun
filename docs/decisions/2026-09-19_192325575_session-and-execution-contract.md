+++
schema_version = 1
id = "01M2XHZ6G7J6019K1D32XPGJ6Z"
title = "Session and execution contract"
date = "2026-09-19"
status = "accepted"
tags = ["architecture"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M2XHZ6FS4TWZ94F2J10H3M2B", "01M2XHZ6GDSMGWDK3D5DE9KC26"]
+++
Status: Accepted and implemented under the owner's delegated full-implementation task
Supersedes: the dependency-free scaffold status in [0001](2026-09-19_192325561_rust-project-and-development-baseline.md)
Superseded in part by: [0004: Full runtime and integrations](2026-09-19_192325581_full-runtime-and-integrations.md); the original milestone and evidence below are preserved as history.

## Decision

Implement the first usable CLI/service release described in the build brief. Keep its explicit later dashboard and OS sandbox scope deferred. Treat the OS user, profile authors, and approved commands as trusted. The local Unix socket uses a private owner-only runtime directory and socket; it is not a capability boundary against processes running as that user. Any such caller may start/stop sessions or supply another profile. Agent instructions are not an enforcement mechanism.

A session has a random 128-bit ID, a unique human name, a canonical project directory, a purpose, an immutable profile snapshot, and a monotonic expiry (one hour by default, maximum one day). Stop and expiry deny further work and close active worker controls. Command policy compares canonical absolute executable paths and complete argument vectors. It does not protect files or network endpoints, pin executable bytes, prevent script/config changes, or restrict remote credential authority.

Resolve declared fake or official 1Password CLI references independently for each operation. Cache lifetime is zero between operations; the underlying provider decides whether authorization is still valid. Existing SSH agents are mediated by explicitly supplying their socket path, without extracting keys. No provider keepalive, password-manager lock detection, or ambient service-account token inheritance is introduced. Already delivered credentials cannot be recalled. Provider stdout is bounded and private; failure messages are fixed. Suppress provider stderr. Only the provider receives HOME from the user's OS account; command environments contain only fixed PATH/LANG and explicitly approved injections.

One short-lived guardian process owns each command. The service sends references over an anonymous pipe, and the guardian resolves values and injects the approved child's environment. Core dumps are disabled in that guardian and its children. A pipe closure on service death triggers process-group termination; a watchdog escalates to KILL after 200 ms. Normal parent completion also removes ordinary remaining descendants. This avoids reliance on a daemon cleanup handler surviving a crash. A malicious child may escape its group; OS isolation is a separate future requirement. No PTY is allocated, stdin is null, and stdout/stderr are pipes; interactive execution is not part of this version. An inherited controlling terminal is not an isolation boundary. If a guardian itself is killed or crashes, its child can survive without deadline enforcement; the service records an unknown outcome, and explicit OS/user cleanup may be needed.

Reserve caller-visible operation IDs before execution. Retain records for the service lifetime and reject every duplicate accepted ID, regardless of outcome. A lost client response does not cancel or replay work. Reconnect inspects state; it cannot replay output. Worker loss produces an unknown outcome. A service restart loses all state, so previous IDs/outcomes are unknown and the caller must reconcile effects externally before deliberately retrying. In-memory state was chosen over a persistent journal to avoid adding an unproven recovery protocol and sensitive metadata retention to the initial lifecycle.

Stream stdout/stderr with bounded exact-value redaction across read boundaries. Never retain output or resolved credentials in the daemon ledger. This prevents ordinary accidental exact-value disclosure; it does not stop encoding, discovery of other secrets, file writes, network exfiltration, or a malicious approved child. Unix same-user memory/process inspection is outside the boundary. Secrets are not claimed to be zeroized or protected from OS swap.

Retain at most 512 metadata events, 128 sessions, 1024 accepted operation IDs, 32 active operations, and 64 client connections. Do not evict IDs to admit new work. Protocol frames are newline-delimited tagged JSON, limited to 1 MiB. Profiles reject unknown fields. CLI errors and malformed-IPC errors never echo input. Session metadata omits command arguments, paths, purpose, output, and references; names and IDs are explicitly non-secret identifiers. Inspection reports declarations and provenance without fetching values.

Use serde/serde_json for serialization, nix for safe Unix interfaces, and signal-hook for signal handling. Keep unsafe application code forbidden and the pinned stable toolchain unchanged. The standard library covers threading, subprocesses, sockets, filesystem access, and time; no async runtime, database, HTTP stack, CLI framework, or simulation framework is necessary for this release.

## Evidence and consequences

Public CLI tests use isolated runtimes, fake credentials, and fixture provider executables. Redaction tests explore every byte split in a binary stream. Production lifecycle transitions run deterministic sequences with seeds 1, 7, 42, and 2026 (4096 transitions), plus expiry and stop-before-launch regressions. These checks support the modeled behaviors, not unmodeled attacks or a production reliability claim.

Verification on 2026-09-19: native macOS `mise run check` and `mise run build` passed; Linux arm64 `mise run ci` passed through Dagger/Colima. Both platforms passed 10 unit and 16 public CLI tests, including concurrent duplicate IDs, lost responses, daemon process-group death, active expiry, stalled provider cancellation, SSH socket injection, and shutdown cleanup. The CLI suite took about 2.5 seconds on each platform; this is test runtime, not command-overhead performance. The documented fake profile also passed a native release-binary smoke check. The existing GitHub workflow retains native macOS and Linux CI coverage.

The official [1Password read reference](https://www.1password.dev/cli/reference/commands/read) was checked on 2026-09-19 for the no-newline lookup contract. Live vault, Git, and homelab smoke checks require a separately authorized target and were not performed. Example workflows are recipes, not claims of external integration verification. The build brief records remaining acceptance work explicitly.
