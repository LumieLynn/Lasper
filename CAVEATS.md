# Caveats & Known Issues

Read this before deploying Lasper in a production environment.

## 1. OCI Images & Init Systems
Docker/Podman images typically define a specific application binary (e.g., `nginx` or `bash`) as their entrypoint. However, `systemd-nspawn` expects a proper init system (like `/sbin/init` or `systemd`) to boot the container OS.
- **Workaround**: OCI images extracted by Lasper may exit immediately upon starting. You must manually inject a init process (e.g., `tini`, `dumb-init`) into the rootfs or modify the `[Exec]` block in the `.nspawn` config.

## 2. CLI Parsing Fragility
Lasper currently retrieves container states by parsing the `stdout` of `machinectl` and `journalctl`. 
- **Risk**: Any upstream changes to `systemd`'s text output format may break state detection.
- Migration to DBus is planned.

## 3. Security & Bind Mounts
Lasper modifies host-to-container namespace mappings (especially when configuring NVIDIA passthrough or custom bind mounts).
- **Warning**: Incorrectly mounting host directories without the "Read Only" flag can grant the container root user full write access to critical host files. Review your mounts carefully.