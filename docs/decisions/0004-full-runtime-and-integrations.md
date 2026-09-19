# 0004: Full runtime and integrations

Date: 2026-09-19
Status: Accepted under the owner's delegated full-implementation task; implemented
Supersedes in part: [0003: Session and execution contract](0003-session-and-execution-contract.md)

## Context and acceptance

The first CLI/service milestone established exact command policy, provider isolation, supervised execution and explicit unknown outcomes. Its deliberate deferrals included interactive input, persistent recovery, caching, a dashboard and OS enforcement. The owner subsequently requested the full implementation and delegated implementation choices. This record captures the resulting choices; it does not reinterpret the earlier milestone's validation as evidence for these additions.

Keep one local Rust binary and the same-user trust boundary. Avoid a database, async runtime, remote service or general plugin framework when bounded local threads, files and subprocesses suffice. Extend the existing typed Unix protocol and public CLI tests. New dependencies serve concrete terminal needs: portable-pty and terminal_size; nix also supplies safe terminal/resource interfaces. Unsafe application code remains forbidden.

## Decision

### Explicit access lifetime and providers

Keep a fresh provider lookup per operation by default. Add an opt-in, session-scoped in-memory credential cache with an absolute TTL of at most 900 seconds. Reuse does not extend its deadline. Refresh, stop, expiry, resume and restart clear it; generation checks prevent late provider responses from restoring invalidated credentials. Resolve in a guardian, send fresh values to the service only for an enabled cache, and never forward those frames to clients or journals.

Add an existing-file provider and password-store adapter alongside fake and official 1Password CLI providers. Open file credentials without following a final symlink and verify the opened regular file's ownership and private permissions. Bound all secret/provider output and reject empty/NUL values. The password-store adapter accepts validated entry names and uses only the first line. Providers receive a minimal environment and fixed failure diagnostics. No authorization keepalive, password-manager lock detection or remote revocation is introduced.

Non-secret environment variables must be explicit profile declarations. Only a separate allowlist can expose their values in inspection. Credential names cannot overlap them. Existing SSH-agent mediation remains available. A built-in Git HTTPS helper matches one configured HTTPS host and obtains the token from a declared credential variable over Git's private helper pipe; it does not store tokens or grant repository-specific remote permissions.

### Interactive supervised execution

Support null stdin, streamed pipe input, and a synthetic PTY with initial size/resize updates and terminal restoration. Input EOF is separate from guardian-control EOF: the former closes command input, the latter cancels the command after daemon death. Bounded input queues fail closed on overflow. A configured shell is an explicit argv convenience and must still match an exact approved executable/argument rule.

Retain independent guardians, operation/session deadlines, signal forwarding and TERM/KILL cleanup. Terminal job control creates additional process groups, so cleanup covers the guardian-owned terminal session as well as the initial command group. Redaction preserves only possible secret prefixes between reads so interactive prompts are not needlessly withheld; terminal CRLF representations of multiline values are also redacted. Deliberate encoding or unrelated secrets remain outside this safeguard.

### Durable metadata without replay

Use an owner-only, bounded, atomic and fsynced JSON snapshot journal. Reserve operation IDs durably before launching work. Persist session/operation status, times, outcomes, events, counters and used-ID tombstones; exclude profiles, paths, purpose, provider references, credentials, caches, environment values, argv and output. Invalid journals and persistence failures fail closed.

On restart, retain completed outcomes and reserved IDs, mark formerly active sessions interrupted and unfinished operations unknown, and require explicit resume with a newly validated profile. Resume retains identity/history, resets session TTL/cache and launches nothing. Detail pruning preserves reserved-ID tombstones. Retain at most 128 sessions, 1024 operation details, 65536 reserved IDs, 512 events, 32 active operations and 64 connected service clients. Capacity exhaustion denies new work; no silent tombstone eviction admits duplicate work.

A client disconnect does not cancel accepted work. Neither reconnect nor crash recovery retries it. The caller must reconcile unknown external effects before deliberately choosing a new ID. A local journal cannot make a remote side effect transactional.

### OS enforcement and interfaces

Add opt-in fail-closed sandboxing: macOS Seatbelt through the installed sandbox-exec, and Linux bubblewrap with namespaces, path mounts, capability removal and a network-denial seccomp filter. The project is readable by default; writes require explicit grants. Protected paths override grants and include the service runtime and file-provider credential files. Deny network by default, including filesystem Unix sockets; allow means general network access. Providers remain outside the command sandbox.

Keep terminal sandbox construction in an exec helper after PTY allocation. The PTY library closes unrelated descriptors during launch; constructing Linux anonymous seccomp/mask descriptors afterward preserves them for bubblewrap. Terminal sandbox setup retains the synthetic controlling terminal, while pipe execution creates a separate session. Native macOS and isolated Linux tests cover this distinction.

Add an independent loopback dashboard with an ephemeral 256-bit capability, strict host/origin checks, bounded HTTP parsing, no secret/output endpoints, and confirmed stop/refresh controls. It displays only the service's safe metadata and explicit non-secret exposure policy. No remote dashboard deployment is supported.

Add a stdio MCP 2025-11-25 adapter exposing status, events, inspection, stop, refresh and exact execution in operator-created sessions. It returns bounded redacted output; it cannot create/resume sessions or change profiles. Interactive input and cancellation notifications remain CLI concerns. Statistics describe mediated operations, not all agent tools.

## Verification and reusable findings

The implementation is exercised by public CLI/provider/runtime/recovery/dashboard/agent/sandbox integration suites and deterministic production-state tests. On 2026-09-19, native macOS `mise run check` and `mise run build`, and Linux `mise run ci` through Dagger passed with 58 tests on each platform, including actual namespace/PTY enforcement on Linux. These repository tasks remain the release gates. Tests use no live vault or remote infrastructure. Separately authorized read-only workflow checks are operational evidence only for their particular setup; no private targets or output belong in this record.

Two platform findings affected the implementation on 2026-09-19: accepted local sockets must explicitly enter blocking mode before framed writes on the tested macOS path, and a socket inode can appear before the listener is ready. A large-output regression and readiness polling through the public status command cover those cases. Recheck when changing IPC setup or test-service startup.

Linux sandboxed terminal tests verify anonymous filter/mask descriptor inheritance through the exec helper and controlling-terminal behavior. Recheck on portable-pty, bubblewrap or kernel upgrades. macOS sandbox-exec is deprecated and SBPL is a private interface: retain native enforcement tests across OS upgrades and fail closed on incompatibility. Linux Dagger enforcement tests need container root capabilities; run trusted checkouts in an isolated disposable engine/VM.

## Consequences and remaining limits

These additions complete the requested scope without creating a credential vault or cloud service. The journal makes local recovery useful and deduplication durable, at the cost of bounded metadata retention and explicit pruning. Optional caching trades fewer provider lookups for a bounded period during which provider lock/rotation is not rechecked. Terminal execution and platform enforcement add dependencies and OS-specific compatibility work.

The user, profile author and same-user processes outside the sandbox remain trusted. Exact argv does not pin executable contents, hooks or configuration. Filesystem policy is path-based, so allowed copies/hardlink aliases remain readable. Network allow is not endpoint filtering. Injected credentials can still be disclosed by approved children and cannot be recalled or remotely revoked by stop. Core dumps are disabled, but zeroization and swap protection are not promised.

Guardians handle ordinary daemon death independently of daemon shutdown handlers. A guardian killed directly can orphan a child; malicious unsandboxed children can escape groups/sessions. Deleting the runtime journal destroys deduplication history. These limitations require explicit operational reconciliation rather than automatic replay or a claim of whole-machine agent confinement.

Implementation/user contracts: [README](../../README.md), [build brief](../../BUILD_BRIEF.md), [sandbox](../sandbox.md), [dashboard](../dashboard.md), and [agent integration](../agent-integration.md).
