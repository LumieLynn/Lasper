# Caveats and Known Limits

Read this before using Lasper on a machine that contains data you care about.

Lasper is alpha software. It manages host-level systemd resources and may run operations as root through the elevated daemon. The current goal is to stabilize a useful `systemd-nspawn` workflow, not to provide a production container platform yet.

## General Safety

- Prefer testing with disposable containers and backups of important host data.
- Review bind mounts carefully. A writable bind mount can give container root write access to host files.
- Avoid running untrusted containers with host GPU, display, or broad filesystem mounts.
- Lasper-created resources should remain compatible with native systemd tools, but some provisioning paths are still experimental.

## Elevated Daemon

`lasper -e` keeps the TUI unprivileged and starts a dedicated root daemon for that Lasper process. The FD-passing socket is protected by a private directory, `0600` permissions, exact PID/UID peer checks, and a per-session token.

This protects the FD interface from independent local processes. It does not protect against code already executing inside the Lasper TUI process. The daemon's normal interface is now typed rather than a generic command/file RPC, but those typed operations still carry the host authority needed to manage containers. See [SECURITY.md](SECURITY.md).

## OCI Images

OCI support is experimental. Most Docker/Podman images are application images, not system images. They often lack `systemd`, a system bus, a normal login setup, or a bootable init process.

Lasper pulls OCI input through systemd 260+'s `importctl pull-oci` and stores it as a systemd-managed `.mstack` application image. This is intentionally not a root filesystem acquisition path: application images may not contain a system manager, login setup, or bootable init process, and they should not be treated as native `systemd-nspawn` system-container images. Registry publisher signatures are not verified yet, so use trusted registries and immutable digests when provenance matters.

## Networking

Veth and bridge-style networking rely on systemd networking behavior inside the container and host NAT/firewall behavior outside it. Lasper may attempt to enable `systemd-networkd` and `systemd-resolved` during setup, but service enablement warnings do not always mean the container cannot be used.

If networking fails, check the host firewall, the container's network services, and native systemd status with tools such as `machinectl`, `networkctl`, and `journalctl`.

## NVIDIA and Display Passthrough

NVIDIA passthrough depends on `nvidia-container-toolkit` and the driver's CDI output. WSL and non-standard driver layouts often require host-mirror mode and explicit library-cache refresh inside the container.

Wayland/X11 passthrough exposes host display sockets to the container. Lasper writes helper environment files such as `.wayland-env` and uses bind mounts to make sockets visible inside the container. UID mapping, `PrivateUsers`, and host compositor policy can still prevent GUI applications from working.

Disabling `PrivateUsers` may make display access easier, but it weakens container isolation. Use it only when you understand the trade-off.

## Storage and Image Paths

Lasper's normal managed image location is `/var/lib/machines/<name>` or `/var/lib/machines/<name>.raw`. Disk-image handling is being tightened around raw images and content validation. Treat unusual image formats and direct image imports as experimental until the storage model is fully typed and validated in the daemon.

## Native Tools Remain Useful

Lasper is meant to make systemd-nspawn easier to operate, not to hide systemd from you. When debugging, the native tools are still the best source of truth:

```bash
machinectl list
machinectl list-images
machinectl status <name>
journalctl -M <name>
systemctl status systemd-nspawn@<name>.service
```
