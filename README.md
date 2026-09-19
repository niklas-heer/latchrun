# Latchrun

Scoped credentials. Persistent sessions. Controlled execution.

Latchrun is a planned local Rust service for running commands with scoped credential access, reusable work sessions, and understandable activity history. The first provider will be 1Password; the initial workflows are Git and homelab administration.

**Status: project scaffold.** The binary currently supports only help and version output. Credential retrieval, sessions, execution, sandboxing, and the dashboard are not implemented. Do not rely on this scaffold to protect secrets or commands.

Start with [BUILD_BRIEF.md](BUILD_BRIEF.md) for the product scope, security boundaries, milestones, and next-session handoff. See [the baseline decision](docs/decisions/0001-project-baseline.md) for setup choices.

## Development

Install [mise](https://mise.jdx.dev/getting-started.html), then from this checkout:

```sh
mise trust
mise install
mise run check
mise run build
./target/release/latchrun --help
```

The project pins stable Rust 1.97.1, including rustfmt, Clippy, Rust Analyzer, and rust-src. There are no Rust dependencies. Cargo.lock is tracked. Use `mise run fmt` to format and `mise run test` to run tests. The scaffold currently has no behavioral tests; add public-CLI tests as session behavior is implemented. Documentation tests become applicable if a library target is introduced.

## CI

`mise run ci` runs the Linux pipeline through Dagger 0.21.9 and its Dang SDK. Start a compatible container engine first. On macOS, Colima with its Docker runtime is supported by the development workflow; native Apple Container requires separate Dagger compatibility setup. Plain native checks do not need a container engine.

GitHub Actions runs the Dagger pipeline on Linux and native checks on macOS. Both check formatting, compilation, Clippy, tests, the release build, and help output. CI needs no 1Password credentials or Dagger Cloud token. The workflow's actions are pinned to commits.

Keep Rust pins aligned in Cargo.toml, rust-toolchain.toml, and mise.toml. Keep the Dagger pin aligned in mise.toml, dagger.json, and the workflow. The Dagger build context explicitly includes source and configuration; extend its allowlist when adding tests or compile-time inputs.

## Contributing

Read [AGENTS.md](AGENTS.md). Use fake credentials for tests; never put real secrets in arguments, fixtures, output, or tracked files. Disposable experiments belong in ignored `scratch/`.

No public license has been selected.
