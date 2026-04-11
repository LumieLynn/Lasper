# Caveats & Known Issues

Read this before deploying Lasper in a production environment.

## 1. OCI Images & Init Systems
Docker/Podman images typically define a specific application binary (e.g., `nginx` or `bash`) as their entrypoint. However, `systemd-nspawn` expects a proper init system (like `/sbin/init` or `systemd`) to boot the container OS. Since OCI images lack init systems, lasper defaultly just extracts the rootfs and sets `boot=no` for containers created from OCI images. 

Due to the positioning of `systemd-nspawn` container, it is actually more appropriate to be used as a system container rather than an application container. The support of running application scripts inside OCI images still needs to be triaged after finnishing the support of customized post-install hooks. Alternatively, you can try to use `docker` or `podman` inside a `systemd-nspawn` container according to [archwiki](https://wiki.archlinux.org/title/Systemd-nspawn).

## 2. CLI Parsing & DBus Integration
Lasper primarily communicates with `systemd` via DBus for state detection and machine management. 
- **DBus Primary**: This provides structured data access and reduces the risk of parsing errors.
- **CLI Fallback**: Lasper maintains a fallback mechanism that parses `machinectl` and `journalctl` stdout if DBus is unavailable (e.g., non-root access or environment issues). CLI parsing remains fragile to upstream format changes.

## 3. NVIDIA GPU Passthrough
Currently, Lasper uses `nvidia-container-toolkit` to generate CDI spec files for NVIDIA GPU passthrough. Defaultly, the NVIDIA passthrough passes all the NVIDIA GPU to the containers, a flexible multi-GPU passthrough method is still needs to be considered.

In the current version, Lasper passes GPU libraries and devices directly to containers without remapping their directory structure. This can cause compatibility issues if the host and container have different library paths. Future updates will add support for choosing a customised passthrough path for NVIDIA drivers.

## 4. Wayland Socket Passthrough
When wayland socket passthrough is enabled, lasper will let user to choose which socket to passthrough. What you need to notice is that the socket is passed to the `/mnt ` directory inside the container with the name of `wayland-socket`. A script called `.wayland-env` will write into the home directory of the user you created. Based on the login shell you configured for each user, it adds "source" for the `.wayland-env` file (supports bash, zsh, fish). This script links the wayland socket to `$XDG_RUNTIME_DIR/wayland-socket` and sets the variable `WAYLAND_DISPLAY` and `DISPLAY`. You can run Firefox to ensure the socket is successfully passed.

For the socket's file permission, lasper defaultly configures with `:idmap` to ensure that the socket's file permission works the same as the host's. However, this may not always work due to the systemd's version. If the systemd's version is lower than 248, lasper will set `PrivateUsers=no` to ensure the permission. But this may lead to some security issue, which requires you to clearly understand the security risks before using it. 

By the way, setting the socket's name to `wayland-socket` allows you to run nested wayland desktop with ease. For example, you can try to use kde plasma inside the container to experience a more virtual-machine-like environment. But this affects a lot by the desktop environment you're using. So, the effect of this feature is not guaranteed. You need to check it by yourself to see whether the nested wayland desktop works as you expected:).

## 5. Security & Bind Mounts
Lasper just offers basic bind mounts settings. If you needs `:idmap` or other advanced settings, you need to configure it by yourself. Never do binds if you're uncertain about what you're doing.
- **Warning**: Incorrectly mounting host directories without the "Read Only" flag can grant the container root user full write access to critical host files. Review your mounts carefully.