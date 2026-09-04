# Caveats and Known Limits

Read this before using Lasper on a machine that contains data you care about.

Lasper remains early-stage software. It manages host-level systemd resources and may run operations as root through the elevated daemon. The current goal is a useful `systemd-nspawn` workflow, not a production container platform.

## General Safety

- Prefer testing with disposable containers and backups of important host data.
- Review bind mounts carefully. A writable bind mount can give container root write access to host files.
- Avoid running untrusted containers with host GPU, display, or broad filesystem mounts.
- Lasper-created resources should remain compatible with native systemd tools, but some provisioning paths are still experimental.
- Removing an image through systemd's `RemoveImage` operation also attempts to delete every same-name `.nspawn` settings file in the applicable systemd search paths, including `/etc/systemd/nspawn/`, regardless of who created it. Lasper's confirmation states this separately from its optional cleanup of NVIDIA state and known unit drop-ins.

## Elevated Daemon

`lasper -e` keeps the TUI unprivileged and starts a dedicated root daemon for that Lasper process. The control and FD-passing sockets live in a private directory, use `0600` permissions, and require exact PID/UID peer checks; the TUI also verifies that the control peer is root, and a per-session token protects each FD request. Privileged RPC traffic does not use the child process's stdin or stdout pipes.

This isolates the daemon interfaces from independent local processes, and the daemon monitors the launching TUI so it can terminate tracked child operations when that session disappears. It does not protect against code already executing inside the Lasper TUI process. The daemon's normal interface is typed rather than a generic command/file RPC, but those typed operations still carry the host authority needed to manage containers. See [SECURITY.md](SECURITY.md).

The elevated daemon requires Linux `pidfd_open` support to pin and monitor the launching TUI process. If the kernel returns `ENOSYS`, `lasper -e` fails closed instead of weakening session monitoring. Running `sudo lasper` remains a compatibility option on such hosts, but it places the complete TUI inside the root trust boundary.

## OCI Images

OCI support is experimental. Most Docker/Podman images are application images, not system images. They often lack `systemd`, a system bus, a normal login setup, or a bootable init process.

Lasper pulls OCI input through systemd 260+'s `importctl pull-oci` and stores it as a systemd-managed `.mstack` application image. This is intentionally not a root filesystem acquisition path: application images may not contain a system manager, login setup, or bootable init process, and they should not be treated as native `systemd-nspawn` system-container images. Registry publisher signatures are not verified yet, so use trusted registries and immutable digests when provenance matters.

An mstack image must run with `PrivateUsers=managed` or `PrivateUsers=no`. Lasper's system-scoped OCI import path uses `PrivateUsers=no`: systemd only sets its internal `IMPORT_FOREIGN_UID` import flag for user-scoped imports, while `PrivateUsers=managed` allocates a transient user namespace and expects directory layers prepared in systemd's foreign UID/GID range. Changing a Lasper-imported root-owned mstack to `managed` can make its files appear as `nobody` and leave its writable layer unusable. `PrivateUsers=no` disables user-namespace isolation and is logged as a security downgrade. Lasper preserves systemd's generated OCI execution settings and promotes the complete configuration to `/etc/systemd/nspawn/` without losing `Boot=`, `Parameters=`, `User=`, `Environment=`, or `WorkingDirectory=`. Starting an OCI application shows a warning but deliberately delegates the final decision to systemd instead of rejecting unusual or externally prepared configurations in advance. Importing OCI applications still requires root or `lasper -e` because promotion writes the system configuration directory. Externally prepared foreign-owned mstack images may use `managed`, but they also require working `systemd-nsresourced` and `systemd-mountfsd` services, host user-namespace/BPF-LSM support, and private networking.

For a running container without a system bus, the integrated terminal can use `nsenter` when Lasper is root or was started with `lasper -e`. This opens a root shell inside the container's user and other namespaces; it bypasses the normal container login service and therefore is not attempted in plain unprivileged mode. Namespace attachment only provides an entry point: it does not repair incorrect mstack ownership, make a read-only filesystem writable, install a shell, or configure networking.

The OCI wizard exposes three systemd network modes. Host writes `Private=no`, `VirtualEthernet=no`, and `ResolvConf=bind-host`. Isolated writes `Private=yes`, `VirtualEthernet=no`, and `ResolvConf=off`. Veth writes `Private=yes`, `VirtualEthernet=yes`, and `ResolvConf=off`. Veth creates the link but does not configure an address inside a `Boot=no` application payload; the payload needs its own network setup. Images imported by older Lasper releases are not silently migrated because Lasper cannot infer the intended user-namespace or network policy.

## Networking

Veth and bridge-style networking rely on systemd networking behavior inside the container and host NAT/firewall behavior outside it. Lasper may attempt to enable `systemd-networkd` during system-container setup. `ResolvConf=` controls how nspawn handles the resolver file, but the file shape alone does not determine whether DNS works: host testing confirms that Lasper's veth and bridge system containers can resolve through the host-provided DNS path even when the container resolver file is a stub. A `Boot=no` OCI application without a network manager must still configure its own address when using Veth; that is a link/address limitation rather than a resolver-file diagnosis.

If networking fails, check the host firewall, the container's network services, and native systemd status with tools such as `machinectl`, `networkctl`, and `journalctl`.

## NVIDIA and Display Passthrough

NVIDIA passthrough is derived from `nvidia-container-toolkit` CDI output and materialized as explicit device, executable, library, firmware, and metadata binds in the machine's `.nspawn` configuration. It therefore depends on the installed host driver and toolkit output remaining compatible with the container. Driver upgrades, WSL, and non-standard library layouts may require regenerating the passthrough configuration and refreshing the container's library cache.

Wayland access is configured per guest user and may include multiple discovered host sockets. During deployment Lasper writes explicit binds from those sockets to `/run/lasper/wayland/<uid>/<display>` in the machine's `.nspawn` configuration. These are startup-time nspawn mounts: Lasper does not add a bind to an already running container, so a new or changed grant takes effect only after the machine is started or restarted. Lasper does not currently configure X11 passthrough or write persistent display-environment helper files.

For `lasper shell`, `lasper launch`, and the TUI's selected-user shell, Lasper revalidates the chosen host socket, checks that the matching startup bind is declared, probes the guest user's effective identity and access to the projected socket, and injects `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` only into that session. A failed probe does not repair a missing or stale bind. Host compositor policy, socket replacement, DAC permissions, and incompatible guest graphics libraries can still prevent a GUI client from connecting.

Wayland grants use an idmapped bind with the default, `yes`, or `pick` user-namespace modes and a non-idmapped bind with `PrivateUsers=no`. They are rejected for `PrivateUsers=managed` and `PrivateUsers=identity`. Choosing `PrivateUsers=no` weakens container isolation and should be an explicit trade-off rather than a generic display workaround.

## Storage and Image Paths

Lasper's normal managed image location is `/var/lib/machines/<name>` or `/var/lib/machines/<name>.raw`. Directory, Btrfs subvolume, and managed raw disk-image targets have distinct lifecycle behavior. Treat unusual image formats and externally managed image paths as experimental.

Tar rootfs imports are extracted by the host's `tar` implementation with `TAR_OPTIONS` ignored. GNU tar 1.35 or newer is recommended: releases before 1.34 lack protection against archive-created symbolic-link traversal, while 1.34 lacks the hard-link confinement added in 1.35. Lasper warns when it detects an older or unrecognized implementation but continues for distribution compatibility. Do not import untrusted Tar archives on those hosts.

Remote Tar/Raw sources use Lasper's custom acquisition path, not `importctl pull-tar`. Lasper invokes `curl` from the host `PATH` with startup configuration disabled, restricts transfers and redirects to HTTP/HTTPS, bounds redirect count and artifact size, and passes only the host `PATH`, stable locale, and explicit proxy variables. Caller-selected curl configuration and certificate override paths are not inherited by the root daemon. Acquired Tar bytes are materialized into the Directory, Subvolume, or DiskImage backend selected in the wizard; systemd-importd's verification, read-only image, and storage semantics therefore do not apply to this path.

## Native Tools Remain Useful

Lasper is meant to make systemd-nspawn easier to operate, not to hide systemd from you. When debugging, the native tools are still the best source of truth:

```bash
machinectl list
machinectl list-images
machinectl status <name>
journalctl -M <name>
systemctl status systemd-nspawn@<name>.service
```
