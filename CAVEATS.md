# Caveats and Known Limits

Read this before using Lasper on a machine that contains data you care about.

Lasper is alpha software. It manages host-level systemd resources and may run operations as root through the elevated daemon. The current goal is to stabilize a useful `systemd-nspawn` workflow, not to provide a production container platform yet.

## General Safety

- Prefer testing with disposable containers and backups of important host data.
- Review bind mounts carefully. A writable bind mount can give container root write access to host files.
- Avoid running untrusted containers with host GPU, display, or broad filesystem mounts.
- Lasper-created resources should remain compatible with native systemd tools, but some provisioning paths are still experimental.
- Removing an image through systemd's `RemoveImage` operation also attempts to delete every same-name `.nspawn` settings file in the applicable systemd search paths, including `/etc/systemd/nspawn/`, regardless of who created it. Lasper's confirmation states this separately from its optional cleanup of NVIDIA state and known unit drop-ins.

## Elevated Daemon

`lasper -e` keeps the TUI unprivileged and starts a dedicated root daemon for that Lasper process. The control and FD-passing sockets live in a private directory, use `0600` permissions, and require exact PID/UID peer checks; the TUI also verifies that the control peer is root, and a per-session token protects each FD request. Privileged RPC traffic does not use the child process's stdin or stdout pipes.

This isolates the daemon interfaces from independent local processes, and the daemon monitors the launching TUI so it can terminate tracked child operations when that session disappears. It does not protect against code already executing inside the Lasper TUI process. The daemon's normal interface is typed rather than a generic command/file RPC, but those typed operations still carry the host authority needed to manage containers. See [SECURITY.md](SECURITY.md).

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

NVIDIA passthrough depends on `nvidia-container-toolkit` and the driver's CDI output. WSL and non-standard driver layouts often require host-mirror mode and explicit library-cache refresh inside the container.

Wayland/X11 passthrough exposes host display sockets to the container. Lasper writes helper environment files such as `.wayland-env` and uses bind mounts to make sockets visible inside the container. UID mapping, `PrivateUsers`, and host compositor policy can still prevent GUI applications from working.

Disabling `PrivateUsers` may make display access easier, but it weakens container isolation. Use it only when you understand the trade-off.

## Storage and Image Paths

Lasper's normal managed image location is `/var/lib/machines/<name>` or `/var/lib/machines/<name>.raw`. Disk-image handling is being tightened around raw images and content validation. Treat unusual image formats and direct image imports as experimental until the storage model is fully typed and validated in the daemon.

Tar rootfs imports are extracted by the host's `tar` implementation with `TAR_OPTIONS` ignored. GNU tar 1.35 or newer is recommended: releases before 1.34 lack protection against archive-created symbolic-link traversal, while 1.34 lacks the hard-link confinement added in 1.35. Lasper warns when it detects an older or unrecognized implementation but continues for distribution compatibility. Do not import untrusted Tar archives on those hosts.

## Native Tools Remain Useful

Lasper is meant to make systemd-nspawn easier to operate, not to hide systemd from you. When debugging, the native tools are still the best source of truth:

```bash
machinectl list
machinectl list-images
machinectl status <name>
journalctl -M <name>
systemctl status systemd-nspawn@<name>.service
```
