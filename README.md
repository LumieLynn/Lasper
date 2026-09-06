# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` system containers.

Lasper provides a guided interface over native systemd resources. It organizes machines, images, terminals, and provisioning tasks in one place.

![demo.gif](demo.gif)

## Features

- **Machine and image management**: Running machines and persistent systemd images are shown as separate resources. Start images, control machine lifecycles, and inspect properties, journal logs, and image metadata without treating cached backing layers as stopped containers.
- **Integrated terminal**: Open multi-session container terminals through native `machinectl login`, with a typed `nsenter` fallback for running containers that do not provide a system bus.
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

- startup and communication behavior, including `elevate`, `cli-mode`, and the journal `log-buffer-lines` limit;
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

You can use Lasper's integrated terminal or native systemd tools after creation. For example: `sudo machinectl shell <user>@<machine>`. Containers with a working system bus use `machinectl login`. When Lasper runs as root or with `lasper -e`, a running container without that bus can instead receive a fixed `nsenter` shell through its machined leader PID.

Lasper also provides process-level selected-user shell commands that follow `machinectl shell` and can run a specific guest executable with its arguments:

```bash
lasper shell user@machine
lasper shell --quiet user@machine
lasper shell user@machine -- /usr/bin/kitty --single-instance
lasper launch user@machine -- /usr/bin/kitty --single-instance
```

The executable is an absolute guest path and the remaining values are passed as its argv. Wayland probing is enabled by default. When the automatically selected display cannot be discovered or validated, an interactive `lasper shell` retries once without Wayland and prints a concise status line if that fallback succeeds. Detailed diagnostics are shown only if the fallback also fails. An exact `--wayland=DISPLAY` selection remains strict, and `lasper launch` never silently drops its display access. Use `--no-wayland` to request a terminal-only session directly.

`lasper shell` owns an interactive PTY and may use the configured elevated daemon. It prints a single detach hint for every transport; press `Ctrl+]` three times within one second to leave a session whose guest processes keep the PTY open. Pass `--quiet` to suppress this hint and the successful Wayland fallback notice; errors and guest output remain visible. `lasper launch` is intended for `Terminal=false` desktop entries. It always uses the invoking user's authority so machine1 can authenticate through the desktop polkit agent, while `--cli-mode` remains available as the systemd transport. It does not detach or discard output; Lasper forwards the guest PTY to its inherited stdout and waits for the guest command to finish.

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
- `t`: open a terminal for the selected running machine or regular image.
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
