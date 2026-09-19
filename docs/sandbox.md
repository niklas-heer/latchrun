# OS sandbox profiles

An enabled sandbox confines the approved command and its descendants. Provider lookup and the service run outside this boundary so credentials can be resolved before delivery. Exact command authorization still happens before execution. A backend failure denies execution; Latchrun never silently runs the command without its requested sandbox.

Add this object to a profile, substituting existing absolute paths:

```json
{
  "sandbox": {
    "enabled": true,
    "network": "deny",
    "read_paths": ["/absolute/shared-input"],
    "write_paths": ["/absolute/project/build"],
    "protected_paths": ["/absolute/project/.env"]
  }
}
```

The project is readable by default, with no project writes unless explicitly granted. `write_paths` also grant reads. OS executable/library roots are readable; other user data is inaccessible by default. Grant additional toolchain installations explicitly, for example `/opt/homebrew` on macOS. A sandbox does not automatically grant read access to an executable outside the default runtime roots: include its installation in `read_paths`. The service adds its runtime directory and file-provider credential files to the protected paths before starting the child, preventing a broad parent-directory grant from exposing its control socket or stored metadata.

The limits are 64 readable paths, 32 writable paths, and 128 protected paths, including automatically added entries. Paths must exist when the profile is validated; symlinks are canonicalized, `/` cannot be granted, and a protected path cannot contain the project root. Consequently a sandboxed project inside the service's runtime directory is rejected. Nonexistent output files should be created beneath an existing granted directory. A disabled sandbox cannot carry path grants or protected paths, preventing a profile from suggesting protection that is not enforced.

`network` defaults to `deny`. Denial includes host loopback TCP and filesystem Unix sockets, including sockets within granted directories. An SSH agent socket requires `network: "allow"`. That setting grants general network access, not host or port filtering. Remote authorization remains the responsibility of the credential and remote service. Do not grant directories containing privileged Unix sockets casually when allowing network access.

## macOS

Latchrun invokes the system `/usr/bin/sandbox-exec` with a generated Seatbelt profile. It starts from deny-default, grants process/runtime bootstrap operations and selected OS runtime reads, then adds project/path grants. Protected paths explicitly deny reading, writing, executable mapping, and outgoing Unix-socket connections, including through symlinks. Filesystem metadata outside readable roots can be inspected; file contents cannot. Network operations remain denied unless enabled.

The profile imports Apple's installed `dyld-support.sb` for OS-version-specific dynamic-loader bootstrap. It does not import `system.sb`, which would additionally grant system-agent IPC. Path strings are escaped as SBPL data; user paths cannot inject policy expressions. No profile contains secret values.

Apple's installed `sandbox-exec(1)` manual marks the command **deprecated**. The installed Apple SBPL files also identify themselves as private interfaces subject to change. This backend is tested on the current host, but future macOS releases can remove or change it. Missing or rejected policies fail closed. Apple's supported sandbox model for packaged apps is [App Sandbox](https://developer.apple.com/documentation/xcode/configuring-the-macos-app-sandbox); it is not a drop-in mechanism for launching arbitrary CLI programs with per-session profiles.

## Linux

Install `bubblewrap` so `/usr/bin/bwrap` exists. The kernel and host policy must permit user, mount, PID, IPC, UTS, and network namespaces. Nested containers often disable the required namespace/mount operations. The backend supports Linux x86-64 and AArch64; network-denied execution on another architecture fails closed.

Latchrun builds a fresh mount namespace with read-only OS runtime roots, a private `/tmp`, a new `/proc` and `/dev`, and the explicit project/read/write mounts. It drops capabilities, disables further user namespaces, and ties sandbox lifetime to its parent. Pipes use a new session; terminal commands retain their separately created synthetic controlling terminal. This uses the isolation primitives described by [bubblewrap's maintainers](https://github.com/containers/bubblewrap#sandboxing). Programs requiring their own nested user namespaces are intentionally incompatible. Older bubblewrap releases missing required flags fail closed.

Protected directories become empty, read-only mounts. Protected files become empty, read-only regular files. Consequently reads may see an empty object instead of returning permission denied; the original contents are unavailable and writes cannot alter the original. The masks are mounted after grants and cannot be removed by a child. A path excluded by the fresh filesystem is absent rather than permission denied.

Network denial combines a separate network namespace with a small seccomp filter denying `socket`, `socketpair`, and `io_uring_setup`. The filter validates the syscall architecture, rejects x32 syscall numbers, and survives descendant execution. This prevents filesystem Unix sockets from bypassing network namespace isolation. Filters are delivered through anonymous descriptors; no secret or temporary policy file is involved. See the [Linux kernel's seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html) for inheritance and architecture checks. Other syscalls are not generally filtered; this is not a minimal-syscall sandbox.

Network-allowed execution shares the host network and includes ordinary resolver/CA-certificate files. An explicitly selected SSH socket is mounted into the child. No other home or runtime directory is automatically mounted.

## Boundaries and tests

These are path-based policies, not data classification. Existing hardlink aliases or copies in readable locations remain readable. Do not place another copy of a protected secret in an allowed root. Same-user processes outside the sandbox are trusted and can modify profile inputs or race filesystem changes before launch; sandboxing an approved child does not isolate Latchrun from its own user. Avoid granting writable toolchain/runtime roots or broad trees that contain sensitive configuration.

Delivered credentials remain available to the command. A child can encode them into its output or allowed files; `network: "allow"` also permits exfiltration. Output redaction is a separate, exact-value safeguard. There is no domain allowlist, remote credential revocation, protection against kernel vulnerabilities, or whole-machine confinement of the calling AI agent.

`tests/sandbox.rs` exercises the public CLI with isolated fake files and local listeners. It verifies readable and writable roots, read-only project behavior, protected files/directories, symlink escape attempts, inherited restrictions, and TCP/Unix network allow and deny. It also verifies that granting a writable parent of the service runtime does not permit a connection back to the service socket. Normal tests require functioning enforcement. For a deliberate unavailable-backend check only, `LATCHRUN_TEST_SANDBOX_UNAVAILABLE=1` verifies refusal and no command side effect; required CI does not set it.

The Dagger Linux check installs bubblewrap and grants root capabilities only to the check execution so nested namespace tests can run. This capability is a property of the CI container, not the Latchrun service's normal requirements. Run this pipeline only with trusted checkout contents in a disposable container engine/VM: privileged container execution is not a security boundary against the engine host. Native macOS checks exercise Seatbelt directly. Routine validation uses no live vault or remote infrastructure.
