# Versions and releases

Latchrun starts at **0.1.0** and follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html). Git tags add a `v` prefix, such as `v0.1.0`. Major version zero means the public API is still developing.

The public contract covers documented CLI flags and exit behavior, profile fields, JSON/MCP interfaces, and persisted-state compatibility. Before 1.0, use a patch increment for compatible fixes and a minor increment for features or breaking changes. Announce breaking changes and any migration requirements. Do not silently discard a user's history or replay commands during an upgrade. Stop the service before replacing its binary, then restart it; an already running daemon does not upgrade itself.

## Commits and generated notes

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```text
feat(cache): add a new cache policy
fix(terminal): preserve the child's exit status
docs: explain provider setup
feat(profile)!: change a required profile field
```

A breaking change can also use a `BREAKING CHANGE:` footer. Keep the description meaningful to a reviewer; use the body for context. Squash PRs with a Conventional Commit title, or ensure each retained commit follows the convention. Release preparation commits use `chore(release): ...` and are omitted from notes unless they declare a breaking change.

The project pins **git-cliff 2.14.2** in `mise.toml`. `cliff.toml` groups commits and requires the convention instead of silently dropping unrecognized messages. It preserves breaking changes even when a group would otherwise be skipped. No GitHub token or credential provider is needed to generate notes.

```sh
mise install
mise exec git-cliff -- git-cliff --bumped-version
mise run changelog -- --tag v0.1.0
```

The bump command only suggests a version. Review it, update `Cargo.toml` and the root package entry in `Cargo.lock`, and use that version in the changelog command. The initial tag is explicitly `v0.1.0`; features and breaking changes keep later pre-1.0 releases on the minor-version track. Release notes and the [changelog](../CHANGELOG.md) are generated from actual commits, not inferred from changed files.

## Dependency notices

`THIRD_PARTY_LICENSES.txt` records license texts and attribution for the locked normal/build dependency graph across the four release targets, plus the bundled SQLite source disclaimer. `about.toml` selects the targets and license alternatives. Review and regenerate it whenever `Cargo.lock` changes:

```sh
mise exec github:EmbarkStudios/cargo-about@0.9.2 -- python3 scripts/generate-licenses.py
mise exec github:EmbarkStudios/cargo-about@0.9.2 -- python3 scripts/generate-licenses.py --check
```

This optional maintenance step needs Python 3.9+ and cargo-about 0.9.2; ordinary builds use the checked-in bundle. If cargo-about has no prebuilt download for the host (including Intel macOS at this version), install the pinned tool with `cargo install --locked cargo-about --version 0.9.2`, then run the same Python script directly. The script reads actual crate license files and SQLite's upstream disclaimer, records the lockfile hash, and fails when expected source metadata changes. Review new dependencies and license choices instead of blindly extending the accepted list.

## Build and publish

1. Start from a clean, reviewed checkout with complete Git history and tags. Review new tracked files for credentials and private infrastructure details. Refresh dependency notices when the lockfile changes.
2. Run `mise run check`, `mise run build` and `mise run ci`. Native macOS checks and Linux Dagger checks must pass. CI requires Bash, Zsh and Fish; local overrides are described in [shells.md](shells.md).
3. Generate and review `CHANGELOG.md` for the chosen version, then commit it with `chore(release): prepare vX.Y.Z`. Commit release implementation changes first so the generated notes include them. Push the reviewed main branch and confirm its checks pass.
4. Create an annotated `vX.Y.Z` tag on that exact commit and push it. Do not move or reuse published tags. The release workflow checks version/target consistency, builds native archives and uploads Actions artifacts. It does not publish a GitHub release automatically.
5. Download the artifacts from the successful tag workflow. Require all four target archives and verify every entry in `SHA256SUMS`. Check archive contents, dependency notices, executable version and a fake-provider smoke run on the native platforms. The workflow tests each target before uploading it.
6. At the tagged commit, run `mise run release-notes` to generate the current tag's notes. Create a draft GitHub release with those notes, the four archives and `SHA256SUMS`. Review the draft and artifact list, then publish it. Do not replace published binaries; issue another version if a fix is needed.
7. In [niklas-heer/homebrew-tap](https://github.com/niklas-heer/homebrew-tap), update `Formula/latchrun.rb` to the released version, exact URLs and verified SHA-256 values. Run Homebrew style/audit and the formula's fake-provider functional test on a real installation. Commit and push the tap update, then verify `brew install niklas-heer/tap/latchrun` resolves the published formula.

For the initial release, after tagging and a successful artifact run, the publication commands are:

```sh
gh run download RUN_ID --name release-assets --dir scratch/release-v0.1.0
mise run release-notes > scratch/release-notes-v0.1.0.md
```

From the downloaded directory, run `shasum -a 256 --check SHA256SUMS` (or `sha256sum --check SHA256SUMS` on Linux). After reviewing the notes and archives, create and inspect the draft:

```sh
gh release create v0.1.0 --verify-tag --draft --title 'Latchrun 0.1.0' \
  --notes-file scratch/release-notes-v0.1.0.md \
  scratch/release-v0.1.0/*.tar.gz scratch/release-v0.1.0/SHA256SUMS
gh release view v0.1.0
gh release edit v0.1.0 --draft=false --latest
```

`RUN_ID` is the successful tag workflow's ID; use the corresponding version for later releases. Publishing is deliberate and requires authority for that release. Never use `--clobber` to replace a published asset.

GitHub release archives are named `latchrun-vX.Y.Z-TARGET.tar.gz`:

| Target | Platform |
| --- | --- |
| `aarch64-apple-darwin` | macOS 15+, Apple Silicon |
| `x86_64-apple-darwin` | macOS 15+, Intel |
| `aarch64-unknown-linux-gnu` | Linux with glibc 2.39+, ARM64 |
| `x86_64-unknown-linux-gnu` | Linux with glibc 2.39+, x86-64 |

Archives contain the executable at their root, README/documentation, the MIT license and dependency license notices. Linux sandboxing requires a working bubblewrap installation and namespace permissions; credential providers require their own tools and authorization. Provider tools are not bundled. The initial macOS binaries are not Developer ID signed or notarized. Cargo registry publication remains disabled; supported distribution is GitHub Releases and the Homebrew tap.

macOS builds explicitly set and verify deployment target 15.0. Linux builds use the pinned Dagger image, report the binary's required GLIBC symbol versions, and smoke-test the extracted archives on Ubuntu 24.04 runners with glibc 2.39. These are glibc builds, not musl/Alpine binaries. Older systems can try a source build but are outside the prebuilt compatibility claim.

The release workflow, packaging script and [accepted distribution decision](decisions/2026-09-19_192325593_semantic-versions-generated-notes-and-homebrew-distribution.md) define the process. Each tag workflow records its actual platform checks; a source build or successful checksum alone is not platform verification.
