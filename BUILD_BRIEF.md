# What we want to build

Created 2026-09-19. This is the handoff for the next development session.

## Intent and current state

Build **Latchrun**, a focused local intermediary between a person or AI agent and commands requiring credentials. Reuse access throughout a work session, recover connections predictably, and make activity inspectable without exposing secrets.

The owner accepted the project name, Rust direction, and creation of a separate repository. This document preserves the intended product and recommended implementation sequence. Detailed protocols, cache policy, sandbox backend, and UI choices remain proposals.

Current implementation: help/version CLI scaffold, pinned tooling, and Linux/macOS CI. No credential access or background service exists yet.

## User workflows

- Git: perform an authorized Git operation without copying a token or private key into the agent conversation. Prefer the existing 1Password SSH agent for SSH authentication; handle HTTPS separately.
- Homelab: run a command with only its required credential, reusing an explicitly scoped session rather than repeatedly resolving the secret.
- Recovery: reconnect after an agent/client disconnect; if the service died, reacquire authorization as necessary and distinguish interrupted work from work safe to retry.
- Inspection: see active sessions, command status and duration, access decisions, and environment provenance.
- Protection: prevent obvious destructive mistakes and add enforceable restrictions with clearly stated limits.

## Product scope

### First usable release

- One Rust binary with a CLI and a local per-user service.
- Named work sessions scoped to project and purpose, with start, status, reconnect, and stop behavior.
- 1Password as the first provider, initially through its official CLI. Persist references and non-secret configuration only.
- Explicit credential lifetime and cache rules; resolve on demand and inject only into approved child processes.
- Explicit executable/argument execution, correct working directory, exit status, signals, and child cleanup.
- Structured activity metadata, secret-safe diagnostics, and fail-closed errors when access cannot be authorized.
- macOS and Linux support, with platform-specific enforcement documented and tested.

### Later, after the lifecycle is proven

- Authenticated local web dashboard: sessions, commands, duration, failures, access history, and policy denials.
- Environment inspector: name, presence, provider/source, precedence, expiry, and allowlisted non-secret values. No raw-secret endpoint.
- Configurable shell per profile; explicit shell mode rather than implicit string evaluation.
- Agent adapters and short integration instructions. Statistics initially cover mediated commands; whole-agent tool usage needs an explicit adapter.
- Further credential-provider adapters, selected for real use cases.
- OS-enforced filesystem/network restrictions and narrowly scoped operation proxies where practical. Basic command checks may arrive earlier but must not be presented as a sandbox.

## Security and lifecycle requirements

1. A child given a secret environment variable can read and disclose it. Redaction cannot prevent intentional encoding, file writes, or network leakage. Promise reduced exposure with an explicit trust boundary, not absolute secrecy.
2. Keep secrets out of the parent shell and AI process where possible. Never place values in command arguments, debug output, crash reports, environment snapshots, or persistent event records. Metadata and command arguments also need filtering.
3. A private Unix socket limits access by other users but does not isolate hostile processes running as the same user. Define client authorization and capability scope before trusting arbitrary callers.
4. Separate service lifetime, work-session lifetime, 1Password authorization, and cached-secret lifetime. Decide expiry and behavior on password-manager lock explicitly. Do not silently keep provider authorization alive.
5. Resume connections, not arbitrary side effects. Assign operation IDs and represent outcome-unknown states. Never automatically replay a possibly completed deployment, deletion, or push.
6. Stop must deny new access and manage the child process tree. Already delivered credentials cannot be recalled; short-lived credentials or upstream revocation may be needed.
7. Credentials grant remote power independently of local command policy. Limit homelab permissions at the remote service too.
8. Command-name matching, aliases, and agent instructions are convenience layers. Hooks, scripts, interpreters, executable resolution, and configuration can change what a command does.
9. A future dashboard should bind to loopback, authenticate requests, protect browser origins/state-changing requests, and avoid raw output retention by default.
10. CI and routine tests must not access a live vault or homelab. Real credential smoke checks are separate, authorized, and must not disclose values.

## Implementation sequence

### 1. Establish the contract with a fake provider

Write an ADR defining session identity, trusted callers, credential scope, expiry, and crash semantics. Design a small provider interface and CLI/IPC error contract. Keep actual command syntax provisional until this contract is reviewed in implementation.

Deliver a vertical slice: start a fake session, invoke a harmless child with a fake credential, inspect safe metadata, and stop. No real vault required.

Acceptance evidence:
- The intended child receives the fake credential; the caller and activity records do not.
- An out-of-scope request is denied; malformed IPC fails without leaking input.
- Repeated authorized calls reuse the session; stop prevents subsequent access.
- Exit status, stderr/stdout behavior, signal forwarding, and cleanup are observable through CLI tests.
- Test artifacts and child processes are isolated and cleaned up.

### 2. Prove lifecycle and recovery

Cover reconnect, concurrent callers, expiry, stale sockets, service crash, child crash, and lost responses. Model time and faults deterministically where useful; retain failing seeds/traces. Establish outcome-unknown behavior and ensure mutating commands are never blindly replayed.

### 3. Add 1Password and one real workflow

Use official provider integration; prefer SSH-agent mediation for Git over SSH. Resolve only declared references. Reauthentication failures must explain the required user action without dumping provider output or credentials. Validate one authorized Git workflow and one narrowly scoped homelab workflow.

### 4. Add policy and observability

Implement explainable deterministic rules, protected paths, retention controls, and statistics. Evaluate platform-specific sandboxing with explicit bypass and limitation tests. Build the local dashboard after the event schema and access boundary stabilize.

## Open decisions for the next session

- Is the initial threat model accidental agent mistakes, untrusted child code, or both? Enforcement and credential delivery differ substantially.
- What identifies a session and authorizes a caller? What can an agent alter in project policy?
- What is the default cached-secret TTL, and what happens when 1Password locks or a key rotates?
- Should the service persist safe session metadata across restart, or initially keep all session state in memory?
- Which exact Git and homelab commands form the first supported workflows?
- Which macOS/Linux restrictions are realistic for the first usable release?

## Performance and scope discipline

Keep normal policy checks deterministic and local. Measure warm command overhead separately from provider unlock/fetch, process startup, redaction, and command runtime. Do not claim latency targets are achieved before measuring them.

Do not build a password vault, cloud service, general terminal replacement, broad plugin framework, or full agent analytics platform in the first release. The dashboard and additional providers should follow demonstrated needs.

## Relevant prior work

These are reference points, not adopted dependencies:

- [1Password process-scoped injection](https://www.1password.dev/cli/reference/commands/run)
- [1Password authorization model](https://www.1password.dev/cli/app-integration-security)
- [1Password SSH agent](https://www.1password.dev/ssh/agent)
- [Lade](https://github.com/zifeo/lade), for overlapping temporary-access workflows
- [Anthropic sandbox runtime](https://github.com/anthropics/sandbox-runtime), for OS enforcement and documented limitations

The previous investigation informed this brief; contributors do not need access to the private orchestration hub.
