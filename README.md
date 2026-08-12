# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` system containers, written in Rust. Inspired by [lazydocker](https://github.com/jesseduffield/lazydocker), Lasper provides a guided interface over native systemd resources instead of replacing `machinectl`, `systemd-nspawn`, or `importctl`.

Lasper is currently alpha software. It is suitable for testing and personal workflows where you understand the caveats, but it is not yet a production-stable container platform.

![demo.gif](demo.gif)

## Features

- **Machine and Image Management**: Running machines and persistent systemd images are shown as separate resources. Start images, control machine lifecycles, and inspect properties, journal logs, and image metadata without treating cached backing layers as stopped containers.
- **Integrated Terminal**: Seamlessly jump into container shells via `machinectl login`. Features a modal interface (Normal/Insert modes) for easy scrolling and multi-session management without leaving the dashboard.
- **Creation Wizard**: Interactively generate `.nspawn` configurations and run provisioning tasks.
- **Image Provisioning**:
  - Pull OCI registry images through systemd 260+'s `importctl pull-oci` as an experimental application-container provider. systemd stores these as `.mstack` images under `/var/lib/machines`.
  - Bootstrap native Debian/Ubuntu or Arch systems via `debootstrap` or `pacstrap`.
- **Hardware Passthrough**: Integrated NVIDIA GPU device allocation (`nvidia-container-toolkit` required) and automated Wayland/X11 socket mounting for GUI apps.
- **Storage Backends**: Supports Directory, Btrfs subvolumes, and Raw sparse images.

## Prerequisites

- `systemd-container` (provides `machinectl` and `systemd-nspawn`)
- Permission to perform privileged container operations. The recommended mode
  is `lasper -e`: the TUI stays unprivileged and starts a separate root daemon
  through `sudo`. Running the entire TUI with `sudo lasper` remains supported
  for compatibility but has a larger root attack surface.
- *Optional*: systemd 260+ (for OCI application images via `importctl pull-oci`)
- *Optional*: `debootstrap` and `pacstrap` (for native Debian/Ubuntu or Arch image support)
- *Optional*: `nvidia-container-toolkit` (for NVIDIA GPU passthrough)

## ⚠️ Before You Begin – Must Read

Lasper is in **early development**. **All users must read [CAVEATS.md](CAVEATS.md) before using Lasper.**
The elevated daemon trust model, remaining root-daemon authority, and current dependency exceptions are documented in [SECURITY.md](SECURITY.md).
Failure to review these caveats may lead to unexpected behavior or data loss.  
For common questions, see [FAQ.md](FAQ.md).

## Installation

To build Lasper from source, ensure you have Rust and Cargo installed, then run:

```bash
cargo build --release
```

The compiled binary will be located at `target/release/lasper`. You can copy it to your path for easy access:

```bash
sudo cp target/release/lasper /usr/local/bin/
```

## Usage

Start the UI in the recommended elevated-daemon mode:

```bash
lasper -e
```

Run `lasper` without `-e` to rely on systemd/polkit for operations supported by
the host policy.

Pass `--version` or `--help` for version info and usage.

You can add a container via the creation wizard. Tap `a` or `n` to open the wizard.

You can use Lasper's integrated terminal or native systemd tools after creation. For example: `sudo machinectl shell <user_name>@<container_name>`. Containers intended to boot through `machinectl` need an init system and a working system bus inside the root filesystem.

**Keybindings:**
- `j` / `k` or `↓` / `↑` : Navigate
- `[` / `]` or `Alt + 1-2` while Images is focused: Switch Regular/Internal images
- `PageUp` / `PageDown` while an Inspector is focused: Scroll the active pane
- `Enter` / `x` : Open the resource-appropriate action menu
- `Tab` / `Shift+Tab` : Cycle focus (Machines → Machine Inspector → Images → Image Inspector → Terminal)
- `n` / `a` : Create a new container (Creation Wizard)
- `s` : Start the selected machine or image
- `S` : Poweroff the selected machine
- `D` : Delete the selected image (while Images is focused)
- `t` : Open a terminal for the selected running machine or a regular image's same-name running machine
- `T` : Maximize terminal (when terminal is focused)
- `r` : Manual refresh
- `R` : Toggle panel resize mode
- `[` / `]` or `Alt + 1-5` while an Inspector is focused: Switch its available panes
- `?` : Show help
- `q` : Quit
- `Esc` : Back / Close Overlays

## Current Direction

The next stable milestone is `0.3.0`. The release is not defined by adding more providers or wizard switches. It should mark the point where the alpha feature set has a clearer security boundary and fewer surprising host-side side effects.

Before `0.3.0` stable, the project should:

- continue migrating elevated-daemon authority from generic command/path RPCs to typed operations;
- keep the systemd-native OCI application provider clearly experimental; it does not turn arbitrary OCI images into bootable system containers;
- preserve unknown `.nspawn` settings instead of rewriting files from only the fields Lasper understands;
- make long-running provisioning and image operations observable and recoverable enough for normal use;
- fail closed on unsafe storage/image conflicts instead of overwriting host resources.

Feature work that requires general-purpose root hooks or arbitrary daemon commands is intentionally deferred.

## Credits

The terminal emulator in `src/term/` is ported from [mprocs](https://github.com/pvolok/mprocs) by Pavel Volokitin (MIT license).

## License

GPL V2
