# Security

Lasper manages host-level container resources and may execute operations as root. Treat the binary, its configuration, and the terminal session running it as privileged administration tools.

## Elevated Daemon Model

`lasper -e` keeps the TUI at the invoking user's privilege level and starts one dedicated root daemon for that Lasper process. The daemon is not a system-wide singleton:

- Starting Lasper in multiple shells creates independent daemon instances, even when the shells belong to the same user.
- Different users also receive independent daemon instances.
- Running without `-e` does not start the elevated daemon. Operations then use the caller's permissions and the host's systemd/polkit policy.

Each daemon session uses:

- A private, randomly named temporary directory.
- A Unix socket owned by the invoking user with mode `0600`.
- Kernel `SO_PEERCRED` checks for the exact PID and UID of the launching TUI.
- A random per-session token delivered through the daemon's stdin bootstrap channel and required on privileged requests.
- Structured request messages instead of delimiter-based command strings.

The daemon exits when Lasper shuts it down normally. A crashed or forcibly terminated session can leave a temporary directory behind, but the random path, directory permissions, peer checks, and expired session token prevent it from becoming a reusable daemon endpoint.

## Remaining Trust Boundary

The authentication above isolates independent local processes and Lasper sessions. It does not protect against code already executing inside the launching Lasper process.

The daemon currently exposes broad command and path-based file RPCs. A compromised TUI process can therefore exercise a large part of the daemon's root authority. Command allowlisting and path authorization remain required defense-in-depth work before a stable 0.3 release.

Prefer `lasper -e` over running the complete interface with `sudo lasper`. Running the whole TUI as root also elevates terminal parsing, clipboard integration, configuration parsing, and all UI code.

## OCI Image Policy

OCI imports currently accept unsigned images for the individual `skopeo copy` operation. Lasper writes the permissive policy into the operation's private staging directory and passes it explicitly to `skopeo`; it does not create or replace `/etc/containers/policy.json`.

Users should select trusted registries and immutable image digests when image provenance matters.

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
| `RUSTSEC-2026-0097` | `zbus -> rand` | Exploitation requires a custom logger that calls `thread_rng` from inside its logging callback; Lasper's logger does not do this. |

These warnings should still be removed through compatible ratatui and zbus upgrades rather than treated as permanent exemptions.
