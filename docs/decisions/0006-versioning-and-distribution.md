# 0006: Semantic versions, generated notes and Homebrew distribution

Date: 2026-09-19
Status: Accepted

## Decision

The owner requested the first release at **0.1.0**, Semantic Versioning, Conventional Commits, a refreshed README/icon, and distribution through the existing `niklas-heer/homebrew-tap` repository. Publish versioned GitHub release archives and a matching formula in that tap. Cargo registry publication remains disabled.

Use [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html), with `v`-prefixed Git tags. The documented public surface includes CLI commands and exit behavior, profile fields, JSON/MCP interfaces, and persisted-state compatibility. While major version is zero, fixes use patch increments; new features and breaking public changes use minor increments. Explicitly identify breaking changes and upgrade requirements. Version 1.0.0 will establish a stable public contract.

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) and pinned git-cliff configuration to generate changelogs and release notes from Git history. Generation rejects nonconforming commits. Bump suggestions are advisory; a maintainer updates Cargo metadata and reviews the release before tagging. No commit automatically publishes a release.

Build macOS Apple Silicon/Intel binaries natively and Linux ARM64/x86-64 binaries through Dagger. A tag workflow produces artifacts for review; a maintainer verifies the complete set, publishes the GitHub release, and then updates and tests the Homebrew formula using the exact artifact checksums. Use the existing tap's binary distribution convention instead of requiring each user to compile Rust.

## Consequences

Published versions and their artifacts are immutable. Fix a bad release with a new version, not a moved tag or replaced binary. Each archive includes documentation, the project license and dependency license notices. The initial binaries are not Developer ID signed or notarized. Document their platform floors and backend prerequisites rather than claiming universal macOS/Linux compatibility.

The tap is a separate repository and receives its own checked Conventional Commit. Publishing the source repository alone does not update Homebrew. The release procedure and its verification commands live in [releasing.md](../releasing.md).
