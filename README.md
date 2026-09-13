# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` system containers.

Lasper provides a guided interface over native systemd resources. It organizes machines, images, terminals, and provisioning tasks in one place.

![demo.gif](demo.gif)

## Features

- **Machine and image management**: Running machines and persistent systemd images are shown as separate resources. Start images, control machine lifecycles, and inspect properties, journal logs, and image metadata without treating cached backing layers as stopped containers.
- **Integrated terminal**: Open multiple terminal tabs per container, use Lasper's selected-user shell prompt with Wayland validation, or attach to the native login prompt. Root and elevated login attachments can fall back to `nsenter` for containers without a system bus.
- **Shell and desktop commands**: Enter a container with `lasper shell user@machine`, run a guest command, or use `lasper launch` from a desktop entry.
- **Creation wizard**: Interactively generate `.nspawn` configurations and run provisioning tasks.
- **Image provisioning**:
  - Pull OCI registry images through `importctl pull-oci` on systemd 260 or newer as an experimental application-container provider. systemd stores these as `.mstack` images under `/var/lib/machines`.
  - Bootstrap native Debian, Ubuntu, or Arch systems with `debootstrap` or `pacstrap`.
- **Host Integration**: Allocate NVIDIA GPU devices (requires `nvidia-container-toolkit`) and grant per-user Wayland access.
- **Storage backends**: Directory, Btrfs subvolume, and raw sparse image support.

## Status

Lasper is in an early functional stage. The current workflow focuses on container creation and lifecycle operations, while configuration management is still evolving.

## Prerequisites

Required:

- `systemd-container`: provides `machinectl` and `systemd-nspawn`.
- `util-linux`: provides `nsenter` for terminal attachment to containers without a system bus.
- Permission to perform privileged container operations.

Optional:

- systemd 260 or newer for OCI application images through `importctl pull-oci`.
- `debootstrap` and/or `pacstrap` for native Debian, Ubuntu, or Arch image support.
- GNU tar 1.35 or newer for tar rootfs imports. Older versions remain usable with a security warning.
- `nvidia-container-toolkit` for NVIDIA GPU passthrough.

## Security and caveats

Read [CAVEATS.md](CAVEATS.md) and [SECURITY.md](SECURITY.md) before use. They describe host-side effects, experimental providers, the elevated daemon trust model, and current dependency exceptions. For common questions, see [FAQ.md](FAQ.md).

The recommended mode is `lasper -e`. The TUI stays unprivileged and starts a separate root daemon through `sudo`. Running the entire TUI with `sudo lasper` remains supported for compatibility but exposes a larger root attack surface.

## Installation

### Release binaries

Download a binary for your architecture from the [GitHub Releases](https://github.com/LumieLynn/Lasper/releases) page. Each release provides glibc and musl builds for x86_64 and aarch64. The musl build is recommended for most Linux hosts because it has fewer dependencies on the host's glibc version.

After downloading the binary and `SHA256SUMS`, verify the checksum and place the binary on your `PATH`:

```bash
sha256sum -c SHA256SUMS --ignore-missing
install -Dm755 lasper-x86_64-unknown-linux-musl ~/.local/bin/lasper
```

Replace the filename with the build that matches your architecture. Use `/usr/local/bin/lasper` with `sudo install` if you want a system-wide installation.

### Build from source

Rust and Cargo are required. Build with the locked dependency versions:

```bash
cargo build --release --locked
install -Dm755 target/release/lasper ~/.local/bin/lasper
```

## Configuration

Lasper reads an optional TOML file from `~/.config/lasper/lasper.toml` once at startup. See [CONFIGURATION.md](CONFIGURATION.md) for the complete reference and examples.

The configuration is typed and can control:

- startup and communication behavior, including `elevate`, `systemd-tools`, journal `log-buffer-lines`, and terminal `scrollback-lines` limits;
- bootstrap defaults, named profiles, provider-specific policies, package inheritance, and local artifact paths for `debootstrap`, `pacstrap`, `dnf5`, and artifact imports;
- TUI colors and semantic status styling through the `[theme]` section.

Command-line flags take precedence over the corresponding settings. Configuration does not add arbitrary executable paths or arbitrary root commands.

## Usage

Start the UI in the recommended elevated-daemon mode:

```bash
lasper -e
```

Run `lasper` without `-e` to rely on systemd/polkit for operations supported by
the host policy.

Pass `--version` or `--help` for version info and usage.

Press `a` or `n` to open the creation wizard.

Press `t` to open Lasper's selected-user shell prompt for the current running container. Enter a guest username to start a shell; exiting it returns to the username prompt. With Machines, Images, or an Inspector focused, `Space` then `t` opens another shell tab, and `Space` then `l` opens the container's native login prompt. Image selections must correspond to a running machine. The native login path uses systemd's login interface; when the container has no system bus, root and elevated mode can instead open a fixed `nsenter` shell through its machined leader PID. The embedded terminal uses `TERM=xterm-256color`.

Lasper also provides process-level selected-user shell commands that follow `machinectl shell` and can run a specific guest executable with its arguments:

```bash
lasper shell user@machine
lasper shell --quiet user@machine
lasper shell user@machine -- /usr/bin/kitty --single-instance
lasper launch user@machine -- /usr/bin/kitty --single-instance
```

The executable is an absolute guest path and the remaining values are passed as its argv. Automatic Wayland selection uses the current `WAYLAND_DISPLAY` only when its host socket is declared as a bind source in the machine's effective `.nspawn` configuration. Otherwise the shell opens without a Wayland probe or fallback notice. If validation of an automatically selected socket fails, an interactive `lasper shell` retries once without Wayland and prints `🪐 Continuing without Wayland...` when that fallback succeeds. Detailed diagnostics are shown only if the fallback also fails. An exact `--wayland=DISPLAY` selection remains strict, and `lasper launch` never silently falls back after a validation failure. Use `--no-wayland` to request a terminal-only session directly.

`lasper shell` owns an interactive PTY and may use the configured elevated daemon. It forwards `TERM`, `COLORTERM`, and `NO_COLOR`, and prints a single detach hint for every transport; press `Ctrl+]` three times within one second to leave a session whose guest processes keep the PTY open. Pass `--quiet` after `shell` to suppress this hint and the successful Wayland fallback notice; errors and guest output remain visible. `lasper launch` is intended for `Terminal=false` desktop entries. It always uses the invoking user's authority so machine1 can authenticate through the desktop polkit agent, while `--systemd-tools` remains available as the systemd transport. Lasper forwards the guest PTY to its inherited stdout and waits for the command's reported lifecycle to finish. After completion, both commands allow up to two seconds for remaining output to drain so an inherited PTY descriptor cannot hold the launcher open indefinitely.

A desktop entry example:

```ini
[Desktop Entry]
Type=Application
Name=Kitty (archlinux)
Exec=lasper launch Lumie@archlinux -- /usr/bin/kitty --single-instance
Terminal=false
```

### Keybindings

Navigation:

- `j` / `k` or `Up` / `Down`: navigate.
- `Tab` / `Shift+Tab`: cycle focus through the main panels.
- `r`: refresh the current data.
- `R`: toggle panel resize mode.

Actions:

- `Enter` / `x`: open the resource action menu.
- `n` / `a`: create a new container.
- `s`: start the selected machine or image.
- `S`: power off the selected machine.
- `D`: delete the selected image.
- `t`: open the selected-user shell prompt for the selected running machine or regular image.
- `Space`, then `t` / `l`: open an additional selected-user shell / native login terminal from Machines, Images, or an Inspector. `Esc` dismisses the action overlay.
- `T`: maximize the terminal when it is focused.

Panels:

- `[` / `]` or `Alt+1-2` while Images is focused: switch regular and internal images.
- `[` / `]` or `Alt+1-5` while an Inspector is focused: switch available panes.
- `PageUp` / `PageDown` while an Inspector is focused: scroll the active pane.

Other:

- `?`: show help.
- `q`: quit.
- `Esc`: go back or close an overlay.

## Credits

The terminal emulator in `src/tui/term/` is ported from [dekit (formerly mprocs)](https://github.com/pvolok/dekit) by Pavel Volokitin. See [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) for its MIT license.

## License

GPL-2.0-only
