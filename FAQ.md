# FAQ

## What is `systemd-nspawn`?

`systemd-nspawn` runs lightweight system containers that use native systemd machine and image management. It is closer to a small operating-system environment than to a Docker application container. The upstream `systemd-nspawn(1)` documentation remains the authoritative reference for container behavior.

## Is Lasper a Docker or Podman replacement?

No. Lasper manages `systemd-nspawn` machines, images, and host integrations. OCI support is an experimental application-image path implemented through systemd's `importctl`; it is not a general OCI runtime.

## What is the difference between an image and a machine?

An image is persistent storage that systemd can use as a container root. A machine is a running instance of an image, with its own process namespace, runtime state, and systemd registration. Lasper keeps the two lists separate so an image is not presented as a stopped machine.

## Should I run `lasper`, `lasper -e`, or `sudo lasper`?

Use `lasper -e` for normal privileged management. The TUI remains owned by the invoking user while a dedicated root daemon performs operations that need host privilege. The process can still pass the user's display environment to a terminal session.

Plain `lasper` uses the caller's permissions and the host's systemd and polkit policy. `sudo lasper` is supported for compatibility, but it runs the complete TUI as root and therefore places terminal parsing, configuration parsing, and other UI code inside the root trust boundary. The `elevate` configuration option can make `lasper -e` the default for the TUI.

## Does each Lasper session start its own elevated daemon?

Yes. Each `lasper -e` process starts an independent daemon session. There is no system-wide Lasper daemon singleton, so two shells owned by the same user still have separate control sockets and lifecycle state.

## What is an image source?

The creation wizard can acquire a root filesystem from several sources:

- native bootstrap tools such as `pacstrap`, `debootstrap`, or `dnf5`;
- local or remote Tar archives;
- local or remote Raw images;
- exact clones of existing managed images; and
- experimental OCI application images imported through systemd 260 or newer.

The source determines how storage is acquired. After acquisition, Lasper may configure the new root filesystem with tools from that filesystem, so all non-clone sources still enter the same post-configuration trust boundary.

## How do I create a container with native bootstrap?

Install the bootstrap tool and the distribution keyring before opening the wizard. Arch requires `pacstrap` and `archlinux-keyring`; Debian and Ubuntu require `debootstrap` and a suitable archive configuration; Fedora and other RPM-based distributions require `dnf5` and repository metadata. The selected mirror and package sources are part of the image creation input, so verify them before starting a long deployment.

To create an Arch Linux image on a non-Arch host, install `pacman` as well. Configure Arch repositories in `/etc/pacman.conf` and choose mirrors in `/etc/pacman.d/mirrorlist`. The [archiso pacman.conf template](https://gitlab.archlinux.org/archlinux/archiso/-/blob/master/configs/releng/pacman.conf) is one example.

To create a Fedora image on a non-RPM host, configure Fedora repositories for `dnf5`. Lasper uses the host's repository configuration during bootstrap. Repository sections can go in `/etc/yum.repos.d/fedora.repo` or `/etc/dnf/dnf.conf`, as described in the [DNF5 configuration reference](https://dnf5.readthedocs.io/en/latest/dnf5.conf.5.html). For example:

```ini
[fedora]
name=Fedora $releasever - $basearch
metalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever&arch=$basearch
enabled=1
pkg_gpgcheck=1
gpgkey=file:///usr/share/distribution-gpg-keys/fedora/RPM-GPG-KEY-fedora-$releasever-primary

[updates]
name=Fedora $releasever - $basearch - Updates
metalink=https://mirrors.fedoraproject.org/metalink?repo=updates-released-f$releasever&arch=$basearch
enabled=1
pkg_gpgcheck=1
gpgkey=file:///usr/share/distribution-gpg-keys/fedora/RPM-GPG-KEY-fedora-$releasever-primary
```

The example uses a local signing key from `distribution-gpg-keys`. If that file is not installed, `gpgkey` can instead point to a key URL such as `https://fedoraproject.org/fedora.gpg`. Check the key's fingerprint against [Fedora's published keys](https://fedoraproject.org/security/).

Save bootstrap profiles in `lasper.toml` to reuse the same settings. See [CONFIGURATION.md](CONFIGURATION.md#bootstrap-selection) for examples.

## How do I remove an image?

Select a stopped image and press `D`. Lasper asks for confirmation, then uses systemd's image removal operation. Systemd may remove same-name `.nspawn` settings files from its search paths as part of that operation. Lasper's separate cleanup option only removes its known NVIDIA state and unit drop-ins; it does not remove unrelated administrator files.

If an image is removed with `machinectl remove` instead, Lasper's NVIDIA state and drop-ins may remain and must be cleaned up separately.

## How does Lasper handle OCI images?

Lasper delegates OCI pulls to `importctl pull-oci` on systemd 260 or newer and keeps the result as a systemd-managed `.mstack` application image. OCI images normally use `Boot=no` and run their application entrypoint directly; they are not expected to provide systemd, a system bus, or a login service.

`Boot=yes` is useful only after the image has been prepared as a bootable system-container root. System-scoped OCI imports use `PrivateUsers=no` because systemd does not apply its foreign-UID import layout to that path. This weakens UID isolation and should be treated as a security trade-off. An externally prepared foreign-owned image may use `PrivateUsers=managed`, but that requires the relevant systemd resource services, host user-namespace support, and private networking.

OCI registry signatures are not verified by Lasper yet. Use trusted registries and immutable image digests when provenance matters.

## What happens when a container has no system bus?

The native login path (`Space` then `l`) normally uses systemd's machine login interface. In root mode or `lasper -e`, a running container without a usable system bus can use a fixed `nsenter` fallback through the machined leader PID. This opens a root shell inside the container's namespaces and bypasses the container's normal login authentication. It does not install a shell, repair image ownership, configure networking, or make a read-only filesystem writable.

Plain unprivileged mode does not use this fallback.

## Why does Veth or bridge networking not work?

Veth creates a host-side and guest-side link, but it does not by itself assign addresses or configure forwarding. Check the container's network services, the host's firewall and NAT rules, and native systemd state with `machinectl`, `networkctl`, and `journalctl -M <name>`.

For `Boot=no` OCI applications, a Veth link normally has no guest network manager to request an address. Host, Isolated, and Veth therefore provide different network behavior and are not interchangeable connectivity presets. Lasper does not manage an administrator-owned bridge or firewall policy as a complete network-orchestration layer.

## Can I use a custom container directory?

Not as a first-class Lasper setting. Managed images are expected under `/var/lib/machines/<name>` or `/var/lib/machines/<name>.raw`, as selected by systemd-machined. Advanced users can arrange a compatible path through a systemd-recognized link, but unusual storage layouts are outside the currently supported storage paths.

## How do NVIDIA CDI files work?

Lasper can generate a CDI document with `nvidia-ctk` or read an existing host CDI document such as `/etc/cdi/nvidia.yaml`. The selected document is parsed and translated into explicit `.nspawn` device, library, firmware, executable, and symlink binds. Lasper validates the projected host paths before changing the machine configuration.

The existing-document path follows the host toolkit's output. After a driver or toolkit update, regenerate the host CDI document when necessary and refresh the machine's NVIDIA projection. In plain unprivileged mode, a root-owned state file may be unreadable; Lasper then leaves the existing projection unchanged instead of claiming that it reconciled the state.

## How do Wayland and X11 display access work?

Both integrations use startup-time `.nspawn` binds. Lasper does not create a runtime bind for a shell session, and a bind change takes effect only after the machine is started or restarted.

Opening a display-enabled shell does not mount a host socket at runtime, write a guest-side helper environment file, or change the guest user's identity. After the required checks pass, the shell request carries the terminal environment and the selected guest-facing `WAYLAND_DISPLAY` or `DISPLAY` value. The display checks are routed through the same selected machine-session transport as the shell itself, either machine1 D-Bus or systemd tools. Lasper does not change the container's PAM or login configuration to enable display access.

### Wayland

The automatic shell probe resolves the current host `WAYLAND_DISPLAY` through `XDG_RUNTIME_DIR` and uses it only when that exact host socket is declared as a startup bind. Before the shell opens, a fixed probe runs as the selected guest user and reports a bounded result: whether the projected target exists, is a Unix socket, is usable by that user, and still has the expected device and inode. Lasper compares that evidence with the host socket and the machine's startup configuration. This probe is read-only and does not grant any additional access. `--wayland=DISPLAY` selects a display strictly, while `--no-wayland` requests a terminal-only session. An automatic probe failure in interactive `lasper shell` can fall back once to a terminal-only shell; explicit selection and `lasper launch` remain strict.

### X11

X11 is always explicit: use `--with-x11` for the current local `DISPLAY`, `--with-x11=:N` for a specific display, or the corresponding TUI action. The selected display must resolve to a supported pathname socket that is already declared in the machine's startup configuration. Its probe checks both the projected bind and the guest's X11 client path, and compares their socket identities with the current host endpoint before the shell opens.

The shell still receives only the selected display environment and terminal variables. However, when X11 access control is enabled, `--with-x11` may additionally add a recorded numeric `localuser` ACL entry for the projected guest identity. That host-side authorization records the display, socket revision, X server generation, and machine instance, and cleanup revokes only an entry that matches the record. If the X server has disabled access control, Lasper does not add an ACL entry; it also never removes pre-existing or externally managed entries. X11 clients receive broad access to the selected X server, so Wayland is the safer default for untrusted guests.

## Why did a shell fall back without Wayland?

The automatic probe is deliberately conditional. If the current display is unset, its host socket is not declared in the machine's startup configuration, or the guest projection cannot be verified, Lasper can continue with a terminal-only interactive shell. The fallback does not modify the machine configuration. Use an explicit `--wayland=DISPLAY` when a missing or invalid bind should be reported as an error instead.

## Can Lasper run on a non-systemd init host?

That is not a supported target. `systemd-nspawn` can sometimes be invoked on a host without a full systemd boot, but Lasper relies on systemd machine registration, services, D-Bus APIs, and native `machinectl` behavior.

## Where should I report a security vulnerability?

Do not open a public GitHub issue for a suspected vulnerability. Follow the private reporting process in [SECURITY.md](SECURITY.md) and send the details by email so the issue can be assessed before public disclosure.
