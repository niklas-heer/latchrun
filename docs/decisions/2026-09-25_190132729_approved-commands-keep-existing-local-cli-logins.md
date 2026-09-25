+++
schema_version = 1
id = "01M3CZ3EDSHS68RAH0RDAFYFBT"
title = "Approved commands keep existing local CLI logins"
date = "2026-09-25"
status = "accepted"
tags = ["runtime", "environment", "sandbox"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M37H4QW35BBZPPYTCNKFCRQW", "01M2XHZ6GDSMGWDK3D5DE9KC26"]
+++
Status: Accepted at the owner's request that existing local logins keep working through Latchrun. The fixed `USER` and `LOGNAME` shipped in v0.1.3; their inspection listing, the Keychain grant and the macOS trust rules were implemented on 2026-09-25.
Amends in part: [Approved commands receive the OS account home directory](2026-09-23_162122947_approved-commands-receive-the-os-account-home-directory.md), whose fixed account variables now also include `USER` and `LOGNAME`, and the macOS sandbox described in [0004: Full runtime and integrations](2026-09-19_192325581_full-runtime-and-integrations.md). Everything else in those records stands.

## Decision

Latchrun adds to the authentication a tool already has and must not break it. A command approved through Latchrun should find the same stored login it finds in the user's shell.

- Approved commands receive `USER` and `LOGNAME` from the account database, alongside `HOME`. Like `HOME`, they are fixed: profiles cannot declare them, the caller's values are never inherited, `inspect` lists them with source `fixed`, and they are absent when the account has no entry.
- The optional sandbox gains an explicit `keychain` setting. On macOS it permits the Security framework's `com.apple.SecurityServer` and `com.apple.securityd.xpc` services and reading and writing under the account's `~/Library/Keychains`. It requires an enabled sandbox and is rejected on other platforms. `inspect` reports it as `sandbox_keychain`.
- On macOS, `network: "allow"` also permits certificate trust evaluation through `com.apple.trustd` and `com.apple.trustd.agent`, and reading `/private/etc/ssl`, matching the resolver and CA files the Linux backend already mounts.

## Context

Checked on 2026-09-25 with `gog` 0.21.0 on macOS, which keeps its refresh tokens in the login Keychain. Unsandboxed, `gog auth list --check` already worked through Latchrun. Under the sandbox, the Keychain lookup failed with `One or more parameters passed to a function were not valid. (-50)` and `gog auth status` silently omitted the account. After granting only the two Security services and `~/Library/Keychains`, the lookup worked but the token refresh failed with `x509: OSStatus -26276`, because Go verifies certificates through `trustd`. `/usr/bin/curl` failed earlier still, unable to read `/private/etc/ssl/openssl.cnf`. So `network: "allow"` on macOS did not permit HTTPS for these common clients.

The Google Workspace CLI (`gws`) was not installed on the test machine; its behavior comes from its source at `googleworkspace/cli`, fetched on 2026-09-25. It encrypts credentials with a key stored in the OS keyring under service `gws-cli` and an account taken from `USER`, falling back to `unknown-user`. On macOS, when that entry is missing it generates a new random key, stores it, and deletes its legacy `.encryption_key` file. Reading that code, before v0.1.3 supplied `USER` the first `gws` command through Latchrun would have looked for the wrong entry, created and stored a new key, and removed the legacy key file; that and every later run could not have decrypted the user's saved credentials. That is the surprise this decision rules out: a login that works in the shell and breaks, or is damaged, through Latchrun.

Alternatives considered. Inheriting the caller's environment would reintroduce ambient inheritance, which the execution contract rejects. Letting profiles declare `USER` would let a profile retarget which account's Keychain items a tool uses, and would be a workaround each author must discover; like `HOME`, the value is derived instead. Enabling Keychain access for every sandboxed command would silently widen the sandbox for profiles written against the earlier, narrower policy; an explicit opt-in keeps the sandbox's default deny while making the capability available. Importing Apple's `system.sb` would grant many unrelated system services. Adding `PATH` entries so script launchers such as the npm `gws` launcher can find `node` was not chosen: the fixed `PATH` is part of the approved policy, and approving the tool's native binary works without widening it.

## Consequences

Unsandboxed tools that key stored logins on `HOME`, `USER` or `LOGNAME` behave as in the shell. Profiles that previously declared `USER` or `LOGNAME` are now rejected at session start; `0.x` releases may change the profile contract, and the rejection is explicit rather than silent.

A sandboxed command with `keychain` can use any Keychain item that the Keychain's own access control lets it use outside Latchrun, and can add or change items. Keychain prompts and item access control remain the only per-item protection; Latchrun does not narrow them. A command allowed network access on macOS can now complete HTTPS verification. Both grants depend on Apple's private SBPL service names, which future macOS releases can change; tests exercise them through offline `security` lookups of an absent item and trust evaluation of the system CA bundle, so a change fails CI instead of failing users silently.

Linux Secret Service access over the session D-Bus is not provided: approved commands receive neither `XDG_RUNTIME_DIR` nor `DBUS_SESSION_BUS_ADDRESS`, and the sandbox denies Unix sockets unless network is allowed. Tools that store their login there, rather than in files under `HOME`, need a separate decision.
