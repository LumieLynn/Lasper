# Caveats & Known Issues

Read this before deploying Lasper in a production environment.

## 1. OCI Images & Init Systems
Docker/Podman images typically define a specific application binary (e.g., `nginx` or `bash`) as their entrypoint. However, `systemd-nspawn` expects a proper init system (like `/sbin/init` or `systemd`) to boot the container OS.
- **Workaround**: OCI images extracted by Lasper may exit immediately upon starting. You must manually inject a init process (e.g., `tini`, `dumb-init`) into the rootfs or modify the `[Exec]` block in the `.nspawn` config.

## 2. CLI Parsing & DBus Integration
Lasper primarily communicates with `systemd` via DBus for state detection and machine management. 
- **DBus Primary**: This provides structured data access and reduces the risk of parsing errors.
- **CLI Fallback**: Lasper maintains a fallback mechanism that parses `machinectl` and `journalctl` stdout if DBus is unavailable (e.g., non-root access or environment issues). CLI parsing remains fragile to upstream format changes.

## 3. Security & Bind Mounts
Lasper modifies host-to-container namespace mappings (especially when configuring NVIDIA passthrough or custom bind mounts).
- **Warning**: Incorrectly mounting host directories without the "Read Only" flag can grant the container root user full write access to critical host files. Review your mounts carefully.