# Latchrun build brief

Created and updated 2026-09-19. This document records product scope, implementation status and the next verification work.

## Intent and current state

Latchrun is a local intermediary between a person or AI agent and commands requiring credentials. It reuses explicitly scoped work sessions, makes activity inspectable without exposing credentials, and recovers connections without replaying uncertain side effects.

The owner delegated the full implementation. The original CLI/service milestone and the previously deferred interactive execution, cache, recovery journal, dashboard, agent adapter, additional providers, Git HTTPS helper and OS enforcement are now implemented. [ADR 0004](docs/decisions/0004-full-runtime-and-integrations.md) supersedes the corresponding limitations of [ADR 0003](docs/decisions/0003-session-and-execution-contract.md). [ADR 0005](docs/decisions/0005-durable-usage-analytics.md) records the subsequent durable analytics extension. See [README.md](README.md) for setup and the current CLI/profile contract.

Implemented does not mean that every external tool, provider authorization arrangement or OS version is certified. Automated checks use fake credentials and disposable local resources. Separately authorized read-only workflow checks do not grant authority for future live credential access.

## User workflows

- **Git:** run an approved Git operation using an existing SSH agent or the built-in host-scoped HTTPS helper, without copying a private key/token into the conversation.
- **Homelab and other services:** give an approved command only its declared credentials and explicit environment; optionally reuse resolved values for a bounded session cache lifetime. Enforce remote permissions at the remote service too.
- **Interactive work:** stream stdin or allocate a terminal with resize and signal handling; choose explicit configured-shell execution when needed.
- **Recovery:** inspect accepted work after client loss or service restart, distinguish known outcomes from unknown ones, and explicitly resume with a profile. Never automatically repeat a possibly completed effect.
- **Inspection:** inspect sessions, durations, exit statuses, access decisions and environment provenance through the CLI, local dashboard or stdio agent adapter.
- **Protection:** combine exact command rules with optional OS-enforced readable/writable/protected paths and network denial. State clearly what this confines and what remains trusted.

## Delivered scope and acceptance evidence

| Area | Implemented behavior | Regression evidence |
| --- | --- | --- |
| Local service | Private Unix runtime/socket, background or foreground lifecycle, concurrent callers and bounded framing | Public CLI lifecycle, stale socket, malformed frame and duplicate-ID tests |
| Sessions and policy | Random IDs/names, immutable profile snapshots, exact canonical executable plus complete arguments, TTL, stop, refresh and explicit resume | State-transition and CLI policy/expiry tests, including denial metadata |
| Providers | Fake, official 1Password CLI, private existing file, password-store, explicit SSH-agent socket | Fake values and executable fixtures test environment isolation, lookup shape, failure suppression and cancellation |
| Credential cache | Opt-in 1–900-second memory-only cache, default zero, absolute TTL and generation-safe invalidation | Cache reuse, expiry, refresh and lifecycle regression tests |
| Child supervision | Isolated guardians, null/pipe/TTY input, terminal resize, signal forwarding, timeouts and descendant cleanup | Public runtime tests for pipe/TTY, terminal job groups, daemon death and stalled providers |
| Output | Separate pipe streams or combined terminal stream, bounded exact-value redaction, exit propagation | Every-byte-split redaction tests, terminal prompts/multiline values and large-output regression |
| Recovery | Owner-only atomic metadata journal, durable ID reservation, preserved outcomes, interrupted/unknown recovery, pruning with tombstones | Restart, lost-response, malformed journal, capacity and persistence-failure tests |
| Git | Existing SSH-agent mediation and built-in exact-host HTTPS credential helper without store writes | SSH socket fixture and actual local Git credential-protocol checks with fake tokens |
| Sandbox | Fail-closed macOS Seatbelt/Linux bubblewrap backends; path grants, protected paths and network deny/allow | Native macOS and isolated Linux enforcement tests, descendants, TCP/Unix sockets and sandboxed controlling terminal |
| Dashboard | Loopback browser UI, ephemeral bearer capability, metadata/provenance, confirmed stop/refresh | HTTP authorization/origin/parser/control tests, HTML checks and browser inspection |
| Agent adapter | MCP 2025-11-25 stdio tools for pre-created sessions, bounded redacted results, no automatic replay | Initialize/tool/policy/malformed-frame and fake execution integration tests |
| Durable analytics | Private SQLite history, 1/7/30/90-day windows, latency/cache trends, MCP activity and explicit external reports | Public CLI pruning/restart, report deduplication, MCP outcome/recording failure and storage-path tests |
| Development | Pinned stable Rust/mise, strict Clippy, native macOS coverage and Linux Dagger checks | Repository check/build/CI tasks; routine tests require no vault or remote infrastructure |

The evidence above describes checked behaviors, not a security audit or a universal compatibility claim. Use the repository checks for the current checkout rather than relying on a historical test count. Runtime source, public integration tests and the linked guides define the concrete contract.

## Accepted boundaries

The service user and profile author are trusted. A private Unix socket restricts other ordinary users, but another process under the same user can supply profiles and control Latchrun. The MCP tool surface only exposes operator-created sessions; it does not constrain an agent that independently has unrestricted shell access. Statistics cover mediated commands, adapter-observed MCP calls and explicit external reports; they do not automatically observe all agent activity.

Exact command matching limits selection, not arbitrary behavior inside an approved program. Shells, hooks, interpreters, executable contents and configuration remain trusted. An enabled sandbox restricts the approved child and descendants, while providers run outside it. Filesystem policies are path-based: readable copies/hardlink aliases and same-user processes outside the sandbox remain outside their protection. Network allow is general access, not endpoint filtering. macOS relies on a deprecated OS interface; Linux requires available namespace capabilities and a compatible bubblewrap. Requested enforcement never silently falls back to unsandboxed execution. See [sandbox.md](docs/sandbox.md).

A child receiving a credential can disclose it through transformed output, allowed files or allowed network traffic. Redaction matches known values and terminal newline variants; it cannot classify arbitrary secrets or reverse deliberate encoding. No credential values, provider references, profiles, arguments or raw command output enter the recovery journal. Only explicit non-secret values may be exposed by environment inspection. Memory zeroization and swap protection are not promised.

Session lifetime, operation timeout, provider authorization and credential-cache TTL are separate. The cache defaults to zero and never persists across service restart. Provider lock is not detected for cached or delivered values; there is no authorization keepalive. Stop/expiry prevents new work and terminates supervised children, but cannot recall delivered credentials or revoke remote authority.

Guardian control-pipe closure handles ordinary service death independently of service shutdown handlers. Terminal job groups are included. A guardian killed directly can lose cleanup/deadline enforcement, and hostile unsandboxed children can escape process groups/sessions. Unknown outcomes require deliberate reconciliation; no transport or restart automatically repeats a command.

Journaled operation IDs survive restart and detail pruning. Profiles and caches do not: resume requires an explicit profile. Bounded capacity fails closed, and losing/deleting the journal loses deduplication history. The journal provides local recovery, not a distributed transaction with an external service. Separate SQLite analytics retain metadata indefinitely across detail pruning and restart; explicit external reports are self-reported and deduplicate by event identity.

## Operational acceptance and next work

The requested implementation scope is complete. The next milestone is continued compatibility and operational verification, not a missing dashboard or sandbox implementation:

1. Re-run native macOS and Linux checks when changing terminal, sandbox, IPC or recovery behavior; retain both OS paths in CI. Linux sandbox tests need an isolated container engine with the documented namespace/root capabilities.
2. Validate the official provider/app authorization setup on each intended deployment OS using separately authorized targets. Fake provider fixtures do not establish that a user's live vault, GPG agent or SSH agent is configured correctly.
3. Recheck macOS sandbox behavior after OS upgrades and Linux behavior after kernel/bubblewrap changes. Fail closed when a required capability disappears.
4. Measure warm command overhead separately from provider lookup/unlock, process startup, redaction and command runtime before setting performance targets. Test runtime is not a latency benchmark.
5. Treat remote hosting, stronger same-user caller isolation, endpoint-specific network proxies, automatic replay, a password vault and automatic observation of unrelated agent tools as separate future scope requiring explicit requirements.

## Guides and decisions

- [README and workflow examples](README.md)
- [Sandbox policy and platform limitations](docs/sandbox.md)
- [Dashboard authorization and controls](docs/dashboard.md)
- [Agent setup and outcome handling](docs/agent-integration.md)
- [Durable analytics and activity reporting](docs/analytics.md)
- [0004: Full runtime and integrations](docs/decisions/0004-full-runtime-and-integrations.md)

Contributors do not need private infrastructure or access to a live vault to develop or run the standard checks.
