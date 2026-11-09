+++
schema_version = 1
id = "01M71A6RJ8ANBP9CY407AP4MAQ"
title = "Sessions can be granted unattended access through explicit options"
date = "2026-11-09"
status = "accepted"
tags = []
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Decision

`session start` and `session resume` accept two explicit, per-session options for leaving agents to work without a person present:

- `--unattended DURATION` sets the session lifetime and the credential cache to one duration, up to the existing 24-hour session limit, and resolves every credential before the command returns. If resolution fails, the session is stopped and the command fails.
- `--allow any-args` permits any arguments to the profile's listed executables; `--allow any-command` permits any absolute executable. The default remains exact matching.

Both are carried in a `grant` on the session's profile snapshot so guardians enforce the same policy, but a profile file that sets `grant` is rejected. Without `--unattended`, the credential cache keeps its 900-second limit.

## Context

Niklas wanted to go to bed and leave agents with enough access to keep working, with the duration and permissiveness controlled by arguments.

Raising the cache limit alone does not achieve that. Credentials were resolved lazily by the first command's guardian, so an agent's first command at night would still open a 1Password prompt nobody could answer. Resolving at session start moves the prompt to the moment the operator is present and turns a bad provider state into an immediate failure. One duration also removes the combination where the cache outlives or falls short of the session.

Alternatives considered:

- Raising the 900-second limit for all profiles would lengthen credential exposure for sessions that never asked for it.
- Accepting these settings in profile files would let a checked-in file grant broad access indefinitely. The decision to relax policy belongs to the person starting that session.
- Extending or keeping alive 1Password authorization is outside Latchrun's contract and is governed by the provider.

## Consequences

Resolved credentials stay in the service's memory for the whole grant, and locking 1Password does not remove them. `session stop`, `session refresh`, expiry and a service restart do.

`--allow any-command` gives every executable an agent names the session's credentials, so it is only as safe as the agent and the credentials' remote scope. An enabled sandbox still applies. Command policy was never a boundary against a malicious approved program.

Unattended sessions also expire by wall clock, because the monotonic clock can stop during machine sleep and must not extend a grant past the deadline reported to the operator. Attended sessions keep the monotonic-only behavior.

Agents can still clear the cache with `latchrun_refresh`, after which the next lookup needs the provider again. SSH agent signatures are not cached and follow the agent's own approval policy. Service restarts do not restore a grant; `session resume` requires the options again.

Automated coverage uses the public CLI with file and fake providers. A live 1Password prompt at session start was not exercised by those tests.
