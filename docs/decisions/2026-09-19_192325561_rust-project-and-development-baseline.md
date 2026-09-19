+++
schema_version = 1
id = "01M2XHZ6FS4TWZ94F2J10H3M2B"
title = "Rust project and development baseline"
date = "2026-09-19"
status = "accepted"
tags = ["rust"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
Visibility and licensing are superseded by [0002: Public repository and MIT license](2026-09-19_192325569_public-repository-and-mit-license.md). The rest of this baseline remains in effect.

## Context and decision

The owner selected **Latchrun** and requested a new repository, setup, and a build brief before implementation in a later session. Create a private repository under niklas-heer with main as its default branch. Use stable Rust and a dependency-free CLI scaffold.

Use mise for pinned tools and local tasks, Dagger with the Dang SDK for Linux CI, and native macOS checks. This follows the owner's established development preferences. Pin Rust 1.97.1 and Dagger 0.21.9 to match the verified local baseline. Use formatting, compilation, strict Clippy, tests as they are added, and release-build checks.

The build brief captures the product direction; its detailed architecture and security policies remain proposals until subsequent decisions establish them.

## Consequences

Contributors can build from this checkout without personal skills or private hub access. Tool pins and CI configuration must stay aligned. No runtime dependencies, credential access, web stack, release automation, public license, or publishable package is introduced by this setup.
