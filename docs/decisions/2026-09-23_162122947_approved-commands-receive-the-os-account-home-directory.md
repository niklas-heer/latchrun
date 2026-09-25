+++
schema_version = 1
id = "01M37H4QW35BBZPPYTCNKFCRQW"
title = "Approved commands receive the OS account home directory"
date = "2026-09-23"
status = "accepted"
tags = ["runtime", "environment"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M2XHZ6G7J6019K1D32XPGJ6Z", "01M2XHZ6GDSMGWDK3D5DE9KC26", "01M3CZ3EDSHS68RAH0RDAFYFBT"]
+++
Status: Accepted and implemented on 2026-09-23
Amended in part by: [Approved commands keep existing local CLI logins](2026-09-25_190132729_approved-commands-keep-existing-local-cli-logins.md); approved commands now also receive fixed `USER` and `LOGNAME`.
Amends in part: [0003: Session and execution contract](2026-09-19_192325575_session-and-execution-contract.md), which stated that only the provider receives `HOME` and that command environments contain only fixed `PATH`/`LANG` and explicit injections. Everything else in that record stands.

## Decision

Approved commands receive `HOME` set to the current OS account's home directory, read from the account database in the guardian. It joins `PATH=/usr/bin:/bin` and `LANG=C` as a fixed variable: profiles cannot declare `HOME` as a non-secret variable or credential, and inspection lists it with source `fixed`. When the account has no home entry, `HOME` is absent; the caller's ambient value is never inherited. Credential providers keep receiving the same value and keep failing closed without it.

## Context

Many command-line tools locate their configuration, token caches and helper binaries through `HOME` and fail without it. The Google Workspace CLI could not run through Latchrun on a second machine for exactly this reason on 2026-09-23. Because `HOME` is a reserved name, a profile author had no supported way to provide it.

Alternatives considered. Inheriting the caller's `HOME` would reintroduce ambient environment inheritance, which the execution contract rejects because the caller's shell state is not part of the approved policy. Letting profiles set `HOME` would allow a profile to retarget a tool's configuration and startup files, so the name stays reserved. Keeping `HOME` absent preserved the smallest environment but made ordinary tools unusable, which conflicts with Latchrun's purpose of running real credentialed commands.

## Consequences

Approved commands can find per-user configuration and caches. Tools that read user startup or configuration files under the home directory now do so, for example Zsh sources `~/.zshenv` even for `-c` commands; approving a command has always meant approving that tool's behavior and configuration, and this makes that explicit. The optional sandbox is unchanged: reading or writing under the home directory still requires explicit grants, so a sandboxed tool that stores its configuration there needs a matching read grant. Hermetic shell tests rely on explicit startup suppression and disposable XDG directories rather than on an absent `HOME`. A per-profile home override would be a deliberate extension of the profile contract and needs its own record.
