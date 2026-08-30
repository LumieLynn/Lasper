# FAQ

## What is `systemd-nspawn`?

`systemd-nspawn` is a systemd container tool for running system containers. It is closer to a lightweight VM-style operating-system container than to a Docker application container. See the upstream `systemd-nspawn(1)` documentation for the complete behavior.

## Is Lasper a Docker or Podman replacement?

No. Lasper targets `systemd-nspawn` system containers and native systemd resources. OCI support exists as an experimental systemd `.mstack` application-image path, not as a full OCI runtime.

## Should I run `lasper`, `lasper -e`, or `sudo lasper`?

Prefer `lasper -e` for normal privileged management. It keeps the TUI unprivileged and starts a dedicated root daemon for privileged operations.

Running plain `lasper` uses your normal user permissions and the host's systemd/polkit policy. Running `sudo lasper` is supported, but it runs the whole interface as root and has a larger attack surface.

## Does each Lasper session start its own elevated daemon?

Yes. Each `lasper -e` process starts its own daemon. Two shells owned by the same user get two independent daemons, and different users also get independent daemons. There is no system-wide daemon singleton.

## Why can't I create containers with bootstrap?

Check that the required bootstrap tool and distribution keyring are installed. For example, Arch bootstrap requires `pacstrap` and `archlinux-keyring`; Debian/Ubuntu bootstrap requires `debootstrap` and the appropriate archive keyring.

Also check the deployment log. Some setup warnings, such as failure to enable `systemd-networkd` inside a minimal rootfs, may not abort the whole deployment.

## How do I remove a container?

Select a stopped image and press `D`. The confirmation is bound to that image even if the list refreshes. Lasper first removes the image through systemd/machinectl; systemd's `RemoveImage` operation also attempts to remove every same-name `.nspawn` settings file from its system search paths and beside the image. The confirmation provides a separate, default-enabled option to remove Lasper's NVIDIA state and known systemd unit drop-ins after the image has been removed.

Lasper removes only its known unit drop-in filenames and removes their directories only when they are empty, so unrelated administrator drop-ins remain. If you run `sudo machinectl remove <name>` manually, systemd still removes the same-name `.nspawn` settings, while Lasper's NVIDIA state and unit drop-ins may remain and require manual cleanup.

## How does Lasper handle OCI-created containers?

Lasper treats OCI support as an experimental application-image path rather than a system-container bootstrap path. It delegates the pull to systemd 260 or newer `importctl pull-oci`, which stores the result as a systemd-managed `.mstack` image. Lasper promotes the generated `.nspawn` configuration to `/etc/systemd/nspawn/`, preserves its entrypoint and execution settings, and adds the network mode selected in the wizard.

The generated configuration normally uses `Boot=no`, so systemd-nspawn runs the OCI entrypoint directly. The payload is not expected to provide systemd, D-Bus, or login services. Lasper also accepts `Boot=yes`; that mode is useful only after the payload has been prepared with a bootable init system and the services required by a system container.

Lasper's system-scoped OCI import uses `PrivateUsers=no` because the imported layers are root-owned. This removes user-namespace isolation, so OCI payloads should come from trusted sources. Changing the configuration to `PrivateUsers=managed` does not convert those layers into systemd's foreign UID/GID layout and can make their files appear as `nobody` or leave the writable layer unusable. A separately prepared foreign-owned mstack may use `managed`, but it also requires the corresponding systemd resource services, host support, and private networking.

The integrated terminal uses `machinectl login` when the container provides a system bus. For a running nspawn container without one, root mode and `lasper -e` can attach a fixed `/bin/bash` or `/bin/sh` through `nsenter`. This fallback only enters the existing namespaces; it does not add an init system, configure networking, or repair incompatible layer ownership.

## Why does veth or bridge networking not work?

Check both the container and host:

- the container should have suitable network and resolver services for its own image configuration, commonly `systemd-networkd` and optionally `systemd-resolved`;
- the host firewall/NAT rules must allow the traffic;
- native systemd tools such as `networkctl`, `machinectl status <name>`, and `journalctl -M <name>` usually show the real failure.

For OCI applications, Host shares the host network namespace, Isolated provides loopback only, and Veth only creates a link. A `Boot=no` OCI payload normally has no network manager to request or configure an address on that veth.

Lasper's setup tries to help, but networking behavior still depends on your host distribution and firewall.

## Can Lasper run on non-systemd init systems?

This is not a supported target. `systemd-nspawn` itself can sometimes be used outside a full systemd host, but Lasper relies on systemd services, DBus APIs, and `machinectl` behavior.

## Can I use a custom container directory?

Not as a first-class setting yet. Lasper currently assumes the systemd-machined image locations, primarily `/var/lib/machines/<name>` and `/var/lib/machines/<name>.raw`.

Native systemd-compatible symlinks may work for advanced users, but custom storage roots are not a stable product contract yet.

## Can I use custom post-install scripts?

Not as privileged root hooks. General-purpose privileged hooks conflict with the daemon security model. Future setup customization should be represented as typed provisioning operations with validated arguments and clear audit output.

## Why does Wayland work but X11 does not?

Wayland and X11 have different authorization models. Lasper can bind display sockets into the container, but UID mapping, `PrivateUsers`, and host compositor policy still matter.

Disabling `PrivateUsers` may make X11 access easier, but it weakens isolation. Prefer nested Wayland or carefully reviewed display passthrough when possible.
