# Bash, Zsh and Fish compatibility

Latchrun accepts an executable and argv from any caller; the caller's quoting determines those bytes before Latchrun sees them. Its configured `--shell` mode is separate: the profile selects a shell executable and options, and the entire script must still match one exact command rule. No shell expansion happens implicitly inside Latchrun.

The public CLI compatibility matrix exercises Bash, Zsh and Fish in both roles:

- Calling Latchrun with literal spaces, both quote characters, dollar signs, command-substitution syntax, backticks, semicolons, globs, backslashes and embedded newlines.
- Delivering a fake credential only to the approved child, redacting its output, and confirming that the caller shell never acquires the credential variable.
- Running an explicitly configured shell script, streaming stdin through EOF, preserving exit 7, and propagating self-SIGTERM as exit 143.
- Running the actual `latchrun run --tty --shell` CLI inside a synthetic caller terminal, verifying terminal file descriptors and redacted output.
- Repeating shell and terminal execution with the native OS sandbox enabled.

These checks cover command invocation and terminal execution. They do not certify every shell version, custom startup file, plugin, completion, login-shell setup or interactive editing feature. Fish scripts use Fish syntax; configuring Fish does not make Bash syntax portable.

## Startup configuration

Tests invoke shells with explicit startup suppression:

| Shell | Profile `shell.args` | Scope |
| --- | --- | --- |
| Bash | `["--noprofile", "--norc", "-c"]` | Suppresses profile/rc files; inherited `BASH_ENV` is also absent because the environment is cleared. |
| Zsh | `["-f", "-c"]` | Disables RCS startup files; system `/etc/zshenv` still runs. |
| Fish | `["--no-config", "-c"]` | Suppresses configuration files. |

These options follow the official [Bash invocation](https://www.gnu.org/software/bash/manual/html_node/Invoking-Bash.html), [Zsh RCS option](https://zsh.sourceforge.io/Doc/Release/Options.html) and [Fish invocation](https://fishshell.com/docs/current/cmds/fish) documentation. They are explicit profile choices; Latchrun does not silently add them to a shell you configure.

Caller-shell tests use disposable HOME/config directories. Child-shell fixtures use disposable `XDG_CONFIG_HOME` and `XDG_DATA_HOME`; child `HOME` is the OS account's home directory under Latchrun's normal environment policy, so the explicit startup suppression above and the disposable XDG directories are what keep user configuration out of the tests. No user startup configuration, real credential provider or external service is needed. Sandboxed non-system shells receive a read grant for their installation prefix so their executable and runtime resources are accessible.

Homebrew shells may load libraries from another package's Cellar directory through an `opt` symlink. When the canonical shell executable is inside `/opt/homebrew/Cellar` (Apple Silicon) or `/usr/local/Cellar` (Intel), the test profile therefore grants read access to that matching Homebrew prefix. System shells do not receive these Homebrew grants. This covers, for example, Fish loading `opt/pcre2/lib/libpcre2-8.0.dylib` outside its own version directory. These are explicit fixture permissions, not permissions automatically added by Latchrun; user profiles must likewise allow any external libraries their chosen shell needs.

## Run the matrix

```sh
cargo test --test shells -- --nocapture
LATCHRUN_REQUIRE_SHELLS=1 cargo test --test shells -- --nocapture
```

The first command tests available shells and prints an explicit `SKIP` for missing shells. The second fails if any of Bash, Zsh or Fish is unavailable. Set absolute executable overrides when using an isolated installation:

```sh
LATCHRUN_TEST_BASH=/absolute/path/to/bash \
LATCHRUN_TEST_ZSH=/absolute/path/to/zsh \
LATCHRUN_TEST_FISH=/absolute/path/to/fish \
LATCHRUN_REQUIRE_SHELLS=1 cargo test --test shells -- --nocapture
```

These examples use Bourne-style environment assignments; Fish users can prefix the same assignments with `env`. Overrides are test configuration, not runtime dependencies or new command permissions. Normal discovery checks `/bin`, `/usr/bin`, `/opt/homebrew/bin` and `/usr/local/bin`. Missing/invalid explicit overrides fail instead of silently selecting another executable. Versions print with `--nocapture` for reproducible test evidence.

Linux Dagger installs Bash, Zsh and Fish and requires the complete matrix. The native macOS workflow runs on both Apple Silicon (`macos-latest`) and Intel (`macos-15-intel`); each installs Fish with Homebrew, uses the system Bash/Zsh, and requires all three. This exercises Homebrew libraries under both installation prefixes. Local application use needs only the shell you choose; the three-shell matrix is a development/CI requirement. Native sandbox tests need working Seatbelt; Linux needs the documented bubblewrap/namespace capabilities. The separate unavailable-sandbox diagnostic mode is not enabled in required CI.

## Verified versions

On 2026-09-19, native macOS arm64 passed the complete matrix with Bash 3.2.57, Zsh 5.9 and Fish 4.9.3, including sandboxed terminal execution. Native Fish was extracted from its official release app archive into ignored scratch storage with its published SHA-256 verified; no global installation or default-shell change was made. The complete shell test took about 3.6 seconds on that host. That is test duration; [command-overhead measurements](latency.md) use a separate release-binary benchmark.

The Linux arm64 Dagger check also passed the complete matrix with Bash 5.2.37, Zsh 5.9 and Fish 4.0.2, including real bubblewrap/PTY enforcement. The full pipeline passed all 78 tests, strict checks and the release build; its shell matrix took about 0.8 seconds. An isolated source-copy Dagger run with an intentionally unavailable required Fish executable failed at the shell test and propagated a nonzero pipeline result, confirming CI cannot silently omit it. These are the versions tested, not a promise that every older/newer shell behaves identically.

Repeat the required matrix when changing quoting, environment setup, PTY allocation, child cleanup or sandbox construction, and record the installed shell versions when interpreting a failure.
