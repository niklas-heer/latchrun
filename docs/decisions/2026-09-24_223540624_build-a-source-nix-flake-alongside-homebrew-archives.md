+++
schema_version = 1
id = "01M386J36GAYQXY255982244RB"
title = "Build a source Nix flake alongside Homebrew archives"
date = "2026-09-24"
status = "accepted"
tags = ["release"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M2XHZ6GST73HA3J5KRQK5T6G"]
+++
## Decision

Ship a Nix flake at the repository root that builds latchrun from source. `nix profile add github:niklas-heer/latchrun` then installs the latest `main` on aarch64-darwin, aarch64-linux, and x86_64-linux. Niklas requested flakes for his released tools on 2026-09-24. The cross-repository rationale lives in the hub decision "Ship source-built Nix flakes for released CLI tools".

`nix/package.nix` uses `rustPlatform.buildRustPackage` with `cargoLock.lockFile`, which reads the version from `Cargo.toml` and runs `latchrun --version` as its install check. The package build does not repeat the test suite; the Dagger and native macOS checks own it. `.github/workflows/nix.yml` builds the flake when the flake, `Cargo.toml`, or `Cargo.lock` changes.

## Context

The Homebrew decision chose prebuilt archives so that users do not need a Rust toolchain. Nix supplies the toolchain itself, and building from source keeps the flake free of per-release hashes. The locked `nixpkgs-unstable` input provides a rustc that satisfies `rust-version = "1.97.1"`. nixpkgs 26.05 does not. nixpkgs-unstable no longer supports Intel Macs, so Homebrew and the release archives remain the path there.

## Consequences

Nix users compile latchrun locally. Raising `rust-version` beyond the locked nixpkgs toolchain requires `nix flake update`, and the Nix workflow reports the failure. Releases do not update the flake, and release tags created before the flake existed cannot be installed through it.
