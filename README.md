# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` system containers.

Lasper provides a guided interface over native systemd resources. It organizes machines, images, terminals, and provisioning tasks in one place.

![demo.gif](demo.gif)

## Features

- **Machine and image management**: Running machines and persistent systemd images are shown as separate resources. Start images, control machine lifecycles, and inspect properties, journal logs, and image metadata without treating cached backing layers as stopped containers.
- **Integrated terminal**: Open multiple terminal tabs per container, use Lasper's selected-user shell prompt with Wayland/X11 integrations, or attach to the native login prompt. Root and elevated login attachments can fall back to `nsenter` for containers without a system bus.
- **Shell and desktop commands**: Enter a container with `lasper shell user@machine`, run a guest command, or use `lasper launch` from a desktop entry.
- **Image provisioning**:
  - Bootstrap native Debian, Ubuntu, Arch, or Fedora systems with `debootstrap`, `pacstrap`, or `dnf5`.
  - Pull experimental OCI application images through `importctl` on systemd 260 or newer.
- **Host integration**: Allocate NVIDIA GPU devices from generated or existing CDI data, configure per-user Wayland binds, and explicitly manage X11 socket access when the host X server permits it.
- **Storage backends**: Directory, Btrfs subvolume, and raw sparse image support.

## Status

Lasper is in an early functional stage. The current workflow focuses on container creation, lifecycle operations, and selected display integrations; broader nspawn configuration management is still evolving.

## Prerequisites

Required:

- `systemd-container`: provides `machinectl` and `systemd-nspawn`.
- `util-linux`: provides `nsenter` for terminal attachment to containers without a system bus.
- Permission to perform privileged container operations.

Optional:

- systemd 260 or newer for OCI application images through `importctl pull-oci`.
- `debootstrap`, `pacstrap` with `pacman`, or `dnf5` for native distribution bootstrap.
- GNU tar 1.35 or newer for tar rootfs imports. Older versions remain usable with a security warning.
- `nvidia-container-toolkit` for generated NVIDIA GPU discovery, or an existing host NVIDIA CDI document selected in `lasper.toml`.

## Security and caveats

Read [CAVEATS.md](CAVEATS.md) and [SECURITY.md](SECURITY.md) before use. They describe host-side effects, experimental providers, the elevated daemon trust model, and the current dependency audit status. For common questions, see [FAQ.md](FAQ.md).

The recommended mode is `lasper -e`. The TUI stays unprivileged and starts a separate root daemon through `sudo` by default, which brings a better experience for host integration. On systemd 256 or newer, `lasper --elevate=run0` can start the same daemon through run0 and polkit. Running the entire TUI with `sudo lasper` remains supported for compatibility but exposes a larger root attack surface.

## Installation

### Release binaries

Download the latest binary for your architecture from the [GitHub Releases](https://github.com/LumieLynn/Lasper/releases) page. Each release provides glibc and musl builds for x86_64 and aarch64. The musl build is useful when the host's glibc version is not known to be compatible.

After downloading the binary and `SHA256SUMS`, verify the checksum and place the binary on your `PATH`:

```bash
sha256sum -c SHA256SUMS --ignore-missing
install -Dm755 lasper-x86_64-unknown-linux-musl ~/.local/bin/lasper
```

Replace the filename with the build that matches your architecture. Use `/usr/local/bin/lasper` with `sudo install` if you want a system-wide installation.

The release also provides `lasper-licenses.tar.gz` and a toolchain-notices archive for each build. Keep the applicable license files when redistributing a binary; corresponding source is available from the release tag and the dependencies pinned in Cargo.lock.

### Build from source

Rust and Cargo are required. Build with the locked dependency versions:

```bash
cargo build --release --locked
install -Dm755 target/release/lasper ~/.local/bin/lasper
```

Stable releases are built from `main`. Development releases use a commit-qualified version such as `0.3.4-dev.7dc320f`, shown by `lasper --version`.

Lasper has a few compile-time path options for distributions with non-standard filesystem layouts. See `src/paths.rs` for details.

## Configuration

Lasper reads an optional TOML file from `~/.config/lasper/lasper.toml` once at startup. See [CONFIGURATION.md](CONFIGURATION.md) for the complete reference and examples.

The configuration is typed and can control:

- startup and communication behavior, including `elevate`, `elevation-method`, `systemd-tools`, journal `log-buffer-lines`, and terminal `scrollback-lines` limits;
- bootstrap defaults, named profiles, provider-specific policies, package inheritance, and local artifact paths for `debootstrap`, `pacstrap`, `dnf5`, and artifact imports;
- NVIDIA CDI acquisition, including generated snapshots or an existing host `nvidia.yaml`/JSON document;
- TUI colors and semantic status styling through the `[theme]` section.

Command-line flags take precedence over the corresponding settings. Configuration does not add arbitrary executable paths or arbitrary root commands.

The TUI's per-machine configuration editor separately manages Wayland and X11 display binds. Those binds belong to the machine's `.nspawn` configuration rather than to the global TOML file.

## Usage

Start the UI in the recommended elevated-daemon mode:

```bash
lasper -e
```

To try run0 as the privilege launcher, use `lasper --elevate=run0` or `lasper --elevate=run0 shell user@machine`. Set `elevation-method = "run0"` under `[settings]` to use it with ordinary `-e` invocations. Choosing a launcher does not change the D-Bus/systemd-tools backend selection. Authentication completes before the TUI takes over the terminal. Each run0 daemon uses its own temporary system service; if forced cleanup is needed, Lasper requests a stop without another authentication prompt and reports any failure.

Run `lasper` without `-e` to rely on systemd/polkit for operations supported by the host policy.

Pass `--version` or `--help` for version info and usage. Use `lasper shell --help` or `lasper launch --help` for command-specific options.

Press `a` or `n` to open the creation wizard.

Press `t` to open Lasper's selected-user shell prompt for the current running container. Wayland is probed automatically when the current display has a configured bind; X11 remains an explicit opt-in.

When the machine list or image list is focused, press `Space` to open the available terminal actions, including X11-enabled selected-user shells and the native login interface. If a container has no system bus, the login action can use a fixed `nsenter` shell through the machined leader PID in root or elevated mode.

Lasper also provides process-level selected-user shell commands that follow `machinectl shell` and can run a specific guest executable with its arguments:

```bash
lasper shell user@machine
lasper shell --quiet user@machine
lasper shell user@machine /usr/bin/kitty --single-instance
lasper shell --with-x11 user@machine
lasper launch user@machine /usr/bin/kitty --single-instance
lasper launch --with-x11=:0 user@machine /usr/bin/kitty --single-instance
```

The executable is an absolute guest path, and the remaining values are passed as its `argv`. Automatic Wayland selection uses the current `WAYLAND_DISPLAY` only when its host socket is declared as a bind source in the machine's effective `.nspawn` configuration. Use `--no-wayland` to request a terminal-only session directly. `--with-x11` explicitly selects the current local `DISPLAY`.

`--` is optional and ends Lasper option parsing: both `lasper shell user@machine -- /usr/bin/kitty` and `lasper shell -- user@machine /usr/bin/kitty` work. With no executable, `lasper shell user@machine --` opens the default shell. After the executable, all arguments belong to the guest command; for example, `lasper shell user@machine /bin/echo --help` passes `--help` to `/bin/echo`.

Use `--wayland=SOCKET_NAME` or `--with-x11=:N` to explicitly select a Wayland or X11 display. Pass `--quiet` after `shell` to suppress detach and successful Wayland fallback notices; errors and guest output remain visible.

`lasper launch` is intended for `Terminal=false` desktop entries. It uses the invoking user's authority so machine1 can authenticate through the desktop polkit agent; `--systemd-tools` remains available when the systemd command-line transport is preferred. Lasper forwards the guest PTY to its inherited stdout and waits for the command's lifecycle to finish.

A desktop entry example:

```ini
[Desktop Entry]
Type=Application
Name=Kitty (archlinux)
Exec=lasper launch Lumie@archlinux /usr/bin/kitty --single-instance
Terminal=false
```

If you're curious about how Lasper manages Wayland and X11 integration, see the [FAQ](FAQ.md).

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

The embedded terminal is powered by [rio-vt](https://github.com/raphamorim/rio) by Raphael Amorim, with code originating from Alacritty, the Rust Project, and WezTerm. The attribute and color types in `src/tui/foundation/term/attrs.rs` and `src/tui/foundation/term/color.rs` retain code adapted from [dekit (formerly mprocs)](https://github.com/pvolok/dekit) by Pavel Volokitin.

[Ratatui](https://github.com/ratatui/ratatui), [Crossterm](https://github.com/crossterm-rs/crossterm), and [portable-pty](https://github.com/wezterm/wezterm) provide the TUI and PTY foundations. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for the generated dependency inventory and license texts, and [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) for adapted source, bundled components, and additional copyright notices.

## License

Lasper is licensed under [GPL-3.0-or-later](LICENSE): GNU GPL version 3 or, at your option, any later version. Third-party components retain their own licenses; see [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).
