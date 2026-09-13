# Security

Lasper manages host-level container resources and may execute operations as root. Treat the binary, its configuration, and the terminal session running it as privileged administration tools.

## Elevated Daemon Model

`lasper -e` keeps the TUI at the invoking user's privilege level and starts one dedicated root daemon for that Lasper process. The daemon is not a system-wide singleton:

- Starting Lasper in multiple shells creates independent daemon instances, even when the shells belong to the same user.
- Different users also receive independent daemon instances.
- Running without `-e` does not start the elevated daemon. Operations then use the caller's permissions and the host's systemd/polkit policy.

Each daemon session uses:

- A private, randomly named temporary directory.
- Mutually authenticated control and FD-passing Unix sockets owned by the invoking user with mode `0600`.
- Kernel `SO_PEERCRED` checks: the daemon requires the exact launching TUI PID/UID, while the TUI requires a root daemon peer.
- A random per-session token negotiated only after the control connection is authenticated and required on FD-passing requests.
- A Linux pidfd monitor that terminates the daemon and its tracked child process groups when the launching TUI exits; direct command children also receive a kernel parent-death signal. Session pidfds pin the tracked leader for liveness checks, while process-group escalation still uses a separately revalidated numeric group ID and is not an atomic replacement for cgroup ownership.
- Bounded protocol frames, authenticated FD setup timeouts, a concurrent FD-connection limit, and rate-limited authentication diagnostics.
- Structured request messages instead of delimiter-based command strings.

The daemon exits when Lasper shuts it down normally or when its launching TUI exits. A crashed or forcibly terminated session can still leave a temporary directory behind, but the random path, directory permissions, peer checks, and session token prevent it from becoming a reusable daemon endpoint. Each daemon writes a uniquely named, root-owned `0600` session log under `/var/lib/lasper/logs`; a session is capped at 8 MiB, and startup cleanup retains up to eight sessions within a 64 MiB budget while leaving locked logs from active daemons untouched.

## Remaining Trust Boundary

The authentication above isolates independent local processes and Lasper sessions. It does not protect against code already executing inside the launching Lasper process.

The elevated daemon no longer exposes arbitrary host `program + argv` execution or unrestricted absolute-path file operations as normal RPCs. Machine lifecycle, storage, configuration, provisioning, inspection, and terminal setup use typed requests with validated names, bounded paths, fixed host executables, and explicit operation results. A selected-user shell may additionally carry one bounded absolute guest executable and its argv; that value is usable only as the command of the named guest user through machine1 or the fixed `machinectl shell` transport. The daemon still carries broad host authority by design, so a compromised TUI process can request any typed operation that Lasper itself is allowed to perform; the migration is a boundary hardening measure, not a trust boundary against compromised application code.

Terminal requests are a closed set of variants. A default attachment contains a validated machine name and bounded terminal dimensions. A selected-user shell additionally contains a validated guest username, an allowlisted terminal environment, optional verified Wayland socket evidence, and an optional bounded guest command. The daemon reads the leader from machined's bounded, no-symlink runtime registration; if the container has no system-bus socket, a default attachment may execute a fixed `nsenter` argument set and a discovered `/bin/bash` or `/bin/sh` instead of `machinectl login`. This namespace path opens a root shell inside the container and bypasses its login authentication, so it is available only when Lasper itself is root or uses the authenticated elevated daemon. No caller-supplied PID, host executable, fallback shell path, or nsenter argument crosses the daemon protocol. The PTY backend closes inherited unknown file descriptors, so the fallback cannot carry pre-opened namespace descriptors into `nsenter`; it snapshots the leader start time, namespace identities, and root identity, then rechecks them immediately before spawn. That closes ordinary PID reuse and replacement cases, but a narrow post-check/pre-exec race remains; eliminating that race requires a descriptor-preserving launcher or cgroup-based ownership and is not claimed here.

The remaining security work is about ownership and failure semantics: long operations need durable progress and recovery reporting, and every rollback must distinguish resources created by Lasper from resources that pre-dated the operation. Native systemd tools remain the source of truth for resources that Lasper did not create.

Prefer `lasper -e` over running the complete interface with `sudo lasper`. Running the whole TUI as root also elevates terminal parsing, clipboard integration, configuration parsing, and all UI code.

## Root Filesystem Post-Configuration Policy

Lasper may configure a newly acquired root filesystem by invoking its own tools through `systemd-nspawn -D ROOTFS --settings=no`. This includes account creation, network service enablement, and NVIDIA library-cache or environment updates. These commands run with Lasper's host authority and may execute binaries, loaders, libraries, and maintainer-generated state supplied by the guest filesystem. `--settings=no` prevents image-adjacent `.nspawn` settings from changing this helper invocation; it does not make guest programs trusted or place the operation behind a separate security boundary.

Treat every non-clone root filesystem that receives post-configuration as privileged input, whether it came from a Tar archive, a Raw image, or a native bootstrap provider. Native bootstrap limits acquisition to the selected distribution's package sources, but the resulting filesystem still enters the same post-configuration boundary. Exact clones skip this setup phase, but intentionally preserve the source guest's hostname, machine ID, SSH host keys, accounts, application data, and other identity-bearing state.

## OCI Image Policy

OCI application imports use systemd's typed `importctl pull-oci` operation (systemd 260 or newer). Registry transport and authentication are provided by HTTPS, but publisher signatures are not verified by Lasper yet. systemd owns the resulting `.mstack` image under `/var/lib/machines`; Lasper does not treat it as a bootable root filesystem or rewrite it through a generic extraction path.

For new OCI imports, Lasper copies systemd's generated image-adjacent `.nspawn` configuration into the trusted administrator search path, adds an ownership marker, preserves generated execution fields, and refuses to overwrite an administrator file it does not own. The system-scoped import path fixes `PrivateUsers=no` because systemd does not apply its foreign-ID import mode to `importctl --system`; this is logged as a security downgrade because container root is not isolated from host UID 0 by a user namespace. Lasper also writes an explicit Host, Isolated, or Veth network policy so the `systemd-nspawn@.service` template cannot silently supply a different network mode. Existing trusted mstack configurations may use `PrivateUsers=managed` only when their directory layers are owned in systemd's foreign UID/GID range and the effective network is private; `systemd-nsresourced` and `systemd-mountfsd` must also be operational.

Users should select trusted registries and immutable image digests when image provenance matters.

## Tar Image Policy

Tar rootfs extraction runs with the authority required to populate a managed container root. Lasper ignores `TAR_OPTIONS` and reports when the executing host does not provide verified GNU tar 1.35+ extraction protections, but older or unrecognized implementations remain allowed for distribution compatibility. Treat such imports as trusted-input operations. GNU tar 1.35+ hardens the extraction phase, but neither a hardened extraction path nor switching to a Raw or native-bootstrap source removes the separate post-configuration trust boundary described above.

## RustSec Exceptions

CI fails on RustSec vulnerabilities except for the following temporary, documented exceptions:

| Advisory | Dependency path | Rationale |
| --- | --- | --- |
| `RUSTSEC-2026-0194` | `arboard -> wl-clipboard-rs -> wayland-scanner -> quick-xml` | Build-time XML parser DoS; Lasper does not parse attacker-provided XML through this dependency. |
| `RUSTSEC-2026-0195` | `arboard -> wl-clipboard-rs -> wayland-scanner -> quick-xml` | Build-time namespace parser DoS; the affected parser processes Wayland protocol definitions supplied by the dependency. |

`wayland-scanner 0.31.10` requires `quick-xml ^0.39`, while both advisories are fixed in `quick-xml 0.41.0`. Removing the dependency would disable native Wayland clipboard support. These exceptions must be removed when the compatible Wayland dependency chain upgrades.

Cargo audit also reports these informational transitive warnings:

| Advisory | Dependency path | Current exposure |
| --- | --- | --- |
| `RUSTSEC-2024-0436` | `ratatui -> paste` | The proc-macro crate is unmaintained; no vulnerability is reported. |
| `RUSTSEC-2026-0002` | `ratatui -> lru` | The issue affects `LruCache::iter_mut`; ratatui 0.29 uses cache lookup, insertion, and resize operations instead. |
| `RUSTSEC-2026-0253` | `ratatui -> lru` | Exploitation requires `LruCache::pop` with a key whose destructor panics; ratatui 0.29 does not call that cache API. |
| `RUSTSEC-2026-0097` | `zbus -> rand` | Exploitation requires a custom logger that calls `thread_rng` from inside its logging callback; Lasper's logger does not do this. |

These warnings should still be removed through compatible ratatui and zbus upgrades rather than treated as permanent exemptions.
