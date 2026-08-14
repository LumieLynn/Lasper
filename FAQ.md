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

Select a stopped container and press `D`. Lasper asks for confirmation, removes the machine through systemd/machinectl, and then cleans Lasper-managed host-side files such as the `.nspawn` file, its lock files, service override directory, and NVIDIA state file.

If you remove a machine manually with `sudo machinectl remove <name>`, Lasper-managed configuration files may remain and may need manual cleanup.

## Why can't an OCI-created container start?

Most OCI images do not contain a bootable systemd userspace. systemd runs their configured entrypoint directly with `Boot=no`; `machinectl login` and `machinectl shell` normally remain unavailable because these operations require services inside the container. When Lasper runs as root or with `lasper -e`, its integrated terminal detects the missing system bus and attaches a fixed `/bin/bash` or `/bin/sh` through the machined leader's namespaces instead.

Lasper preserves `Boot=` and does not reject `Boot=yes`. If you deliberately install systemd and a system bus in the image, you may switch the trusted `.nspawn` configuration to `Boot=yes`; the image then behaves as a system container rather than a direct-entrypoint application.

Mstack images require either `PrivateUsers=managed` or `PrivateUsers=no`. New Lasper imports use `no` because `importctl --system` creates root-owned layers, whereas `managed` expects directory layers imported in systemd's foreign UID/GID range; simply changing the setting to `managed` can expose the payload as `nobody` and make the writable overlay unusable. Use OCI payloads from trusted sources because `no` removes user-namespace isolation. Run Lasper as root or with `lasper -e` so it can preserve the OCI-generated entrypoint configuration in `/etc/systemd/nspawn/`. Existing foreign-owned mstack images may use `managed`, but they require `systemd-nsresourced`, `systemd-mountfsd`, host user-namespace/BPF-LSM support, and a private network mode. Lasper does not silently migrate older imports.

The nsenter terminal does not change those storage semantics. If the image was imported with incompatible ownership, the shell will still see `nobody` ownership or an unusable writable layer; fix the import/configuration pairing rather than treating the terminal path as a remount mechanism.

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
