# Security

Lasper manages host-level container resources and may execute operations as root. Treat the binary, its configuration, and the terminal session running it as privileged administration tools.

## Reporting Security Vulnerabilities

Do not report a suspected security vulnerability through a public GitHub issue. Send a private report by email to **setdownkinder@foxmail.com** instead. This is especially important for issues that could allow a container escape, unauthorized host-file access, privilege escalation, an elevated-daemon authentication bypass, or unauthorized display access.

Please include the affected Lasper version or commit, host distribution and systemd version, the smallest reliable reproduction, the expected and observed impact, and any relevant logs after removing secrets and personal data. Do not publish the details until the maintainers have had an opportunity to assess the report and coordinate disclosure.

For ordinary bugs, feature requests, and questions that do not expose a security risk, use the project's normal public issue tracker.

## Elevated Daemon Model

`lasper -e` keeps the TUI at the invoking user's privilege level and starts one dedicated root daemon for that Lasper process. The daemon is not a system-wide singleton:

- starting Lasper in multiple shells creates independent daemon instances;
- different users receive independent daemon instances; and
- running without `-e` does not start the elevated daemon. Operations then use the caller's permissions and the host's systemd and polkit policy.

Each daemon session uses:

- a private, randomly named temporary directory;
- mutually authenticated control and FD-passing Unix sockets owned by the invoking user with mode `0600`;
- kernel `SO_PEERCRED` checks for the launching TUI PID and UID, with the TUI also requiring a root daemon peer;
- a random per-session token negotiated only after the control connection is authenticated and required on FD-passing requests;
- a Linux pidfd monitor that terminates the daemon and its tracked child process groups when the launching TUI exits;
- bounded protocol frames, authenticated FD setup timeouts, a concurrent FD-connection limit, and rate-limited authentication diagnostics; and structured request messages instead of delimiter-based command strings.

The daemon exits when Lasper shuts it down normally or when its launching TUI exits. A crashed or forcibly terminated session can still leave a temporary directory behind, but its random path, directory permissions, peer checks, and session token prevent it from becoming a reusable daemon endpoint. Each daemon writes a uniquely named, root-owned `0600` session log under `/var/lib/lasper/logs`; a session is capped at 8 MiB, and startup cleanup keeps the retained logs within a fixed budget while leaving active logs untouched.

The default privilege launcher is sudo. `--elevate=run0` uses polkit to start the daemon in a separate transient system service on systemd 256 or newer. The same RPC authentication and frontend pidfd monitor apply. Killing the run0 client alone does not stop that service. If the daemon fails to exit normally, Lasper requests a service stop without interactive authentication; host policy can reject that request, in which case Lasper reports that cleanup was not confirmed and identifies the service.

## Remaining Trust Boundary

The authentication above isolates independent local processes and Lasper sessions. It does not protect against code already executing inside the launching Lasper process.

The elevated daemon does not expose arbitrary host `program + argv` execution or unrestricted absolute-path file operations as normal RPCs. Machine lifecycle, storage, configuration, provisioning, inspection, and terminal setup use typed requests with validated names, bounded paths, fixed host executables, and explicit operation results. A selected-user shell may carry one bounded absolute guest executable and its arguments; that value is usable only as the command of the named guest user through machine1 or the fixed `machinectl shell` transport.

The daemon still carries broad host authority by design. A compromised TUI process can request any typed operation that Lasper itself is allowed to perform. The typed protocol is a boundary-hardening measure, not a trust boundary against compromised application code.

The namespace shell fallback is available only when Lasper itself is root or uses the authenticated elevated daemon. It uses a fixed `nsenter` argument set and a discovered `/bin/bash` or `/bin/sh`; no caller-supplied PID, host executable, fallback shell path, or `nsenter` argument crosses the daemon protocol. It snapshots the leader start time, namespace identities, and root identity, then rechecks them immediately before spawn. This closes ordinary PID reuse and replacement cases, but a narrow post-check/pre-exec race remains; eliminating that race requires a descriptor-preserving launcher or cgroup-based ownership and is not claimed here.

## Display Access

Wayland and X11 socket binds are startup configuration, not ad-hoc host forwarding. Lasper validates the host endpoint and the guest projection before a session uses it; changing a bind requires a machine restart. A shell session receives only the selected display environment and does not receive a persistent host helper file.

Wayland is the preferred integration for untrusted guests. Its probe requires the current host socket to be declared as a bind in the machine's startup configuration, and the guest projection must pass identity and socket checks. Automatic interactive shell probing may fall back to a terminal-only session; explicit display selection and `lasper launch` remain strict.

X11 authorization is opt-in and tied to an explicit display selection. When the X server has access control enabled, Lasper records the exact ACL entry it adds together with the display, socket revision, X server generation, and machine instance. Lifecycle cleanup revokes only that recorded entry. Entries that pre-date Lasper, were created by another process, or cannot be matched to the current server are never guessed at or removed. If observation is incomplete, cleanup fails closed and leaves the entry for explicit review.

X11 protocol access is broad: a trusted guest can generally observe or interfere with other clients on that X server. An X11 grant should therefore be limited to guests the user already trusts. The ACL record lives in the invoking user's runtime state and is not a durable trust-list setting.

The terminal clipboard path uses OSC 52 rather than a native Wayland clipboard client. Terminal emulators and multiplexers may apply their own policy to OSC 52; that policy is outside Lasper's host authorization boundary.

## Root Filesystem Post-Configuration Policy

Lasper may configure a newly acquired root filesystem by invoking its own tools through `systemd-nspawn -D ROOTFS --settings=no`. This includes account creation, network service enablement, and NVIDIA library-cache or environment updates. These commands run with Lasper's host authority and may execute binaries, loaders, libraries, and maintainer-generated state supplied by the guest filesystem. `--settings=no` prevents image-adjacent `.nspawn` settings from changing this helper invocation; it does not make guest programs trusted or place the operation behind a separate security boundary.

Treat every non-clone root filesystem that receives post-configuration as privileged input, whether it came from a Tar archive, a Raw image, or a native bootstrap provider. Exact clones skip this setup phase, but intentionally preserve the source guest's hostname, machine ID, SSH host keys, accounts, application data, and other identity-bearing state.

## OCI Image Policy

OCI application imports use systemd's typed `importctl pull-oci` operation (systemd 260 or newer). Registry transport and authentication are provided by HTTPS, but publisher signatures are not verified by Lasper yet. Systemd owns the resulting `.mstack` image under `/var/lib/machines`; Lasper does not treat it as a bootable root filesystem or rewrite it through a generic extraction path.

For new OCI imports, Lasper copies systemd's generated image-adjacent `.nspawn` configuration into the trusted administrator search path, adds an ownership marker, preserves generated execution fields, and refuses to overwrite an administrator file it does not own. The system-scoped import path uses `PrivateUsers=no` because systemd does not apply its foreign-UID import mode to `importctl --system`. This is a security downgrade because container root is not isolated from host UID 0 by a user namespace. Existing trusted foreign-owned mstack configurations may use `PrivateUsers=managed` only when their directory layers and systemd resource services satisfy the required host constraints.

## Tar Image Policy

Tar rootfs extraction runs with the authority required to populate a managed container root. Lasper ignores `TAR_OPTIONS` and reports when the executing host does not provide verified GNU tar 1.35+ extraction protections, but older or unrecognized implementations remain allowed for distribution compatibility. Treat such imports as trusted-input operations. Neither a hardened extraction path nor switching to a Raw or native-bootstrap source removes the separate post-configuration trust boundary described above.

## Dependency Audit

The current lockfile has no direct dependency with a published RustSec vulnerability. `serde_yml` was replaced with the maintained `serde_yaml_ng` parser for NVIDIA CDI YAML input; the previous `libyml` and `serde_yml` advisories are therefore no longer present.

`cargo audit` currently reports three transitive warnings from `ratatui 0.29`:

| Advisory | Dependency path | Current assessment |
| --- | --- | --- |
| `RUSTSEC-2024-0436` | `ratatui -> paste` | `paste` is unmaintained, but the advisory reports no known vulnerability. |
| `RUSTSEC-2026-0002` | `ratatui -> lru` | The advisory concerns `LruCache::iter_mut`; the ratatui layout cache does not call that API. |
| `RUSTSEC-2026-0253` | `ratatui -> lru` | Exploitation requires `LruCache::pop` with a key whose destructor panics; the ratatui layout cache does not call that API. |

These are tracked as transitive dependency maintenance work, not ignored as direct application vulnerabilities. A future ratatui upgrade should remove them when it can be made without destabilizing the supported TUI and compiler matrix.

## Native Tools Remain Useful

Lasper is meant to make systemd-nspawn easier to operate, not to hide systemd from you. When debugging, native tools remain the best source of truth:

```bash
machinectl list
machinectl list-images
machinectl status <name>
journalctl -M <name>
systemctl status systemd-nspawn@<name>.service
```
