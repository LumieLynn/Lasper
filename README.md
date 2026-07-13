# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` containers, written in Rust. Inspired by [lazydocker](https://github.com/jesseduffield/lazydocker), Lasper brings an interactive experience to systemd-nspawn container management.

![demo.gif](demo.gif)

## Features

- **Container Management**: Start, stop, restart, enable/disable, and terminate systemd-nspawn containers. View properties, full details, and journal logs directly in the terminal via a unified dashboard.
- **Integrated Terminal**: Seamlessly jump into container shells via `machinectl login`. Features a modal interface (Normal/Insert modes) for easy scrolling and multi-session management without leaving the dashboard.
- **Creation Wizard**: Interactively generate `.nspawn` configurations with background task execution for image provisioning and deployment.
- **Image Provisioning**:
  - Pull and extract OCI (Docker/Podman) images via `skopeo` and `umoci`.
  - Bootstrap native Debian/Ubuntu or Arch systems via `debootstrap` or `pacstrap`.
- **Hardware Passthrough**: Integrated NVIDIA GPU device allocation (`nvidia-container-toolkit` required) and automated Wayland/X11 socket mounting for GUI apps.
- **Storage Backends**: Supports Directory, Btrfs subvolumes, and Raw sparse images.

## Prerequisites

- `systemd-container` (provides `machinectl` and `systemd-nspawn`)
- Permission to perform privileged container operations. The recommended mode
  is `lasper -e`: the TUI stays unprivileged and starts a separate root daemon
  through `sudo`. Running the entire TUI with `sudo lasper` remains supported
  for compatibility but has a larger root attack surface.
- *Optional*: `skopeo` and `umoci` (for OCI image support)
- *Optional*: `debootstrap` and `pacstrap` (for native Debian/Ubuntu or Arch image support)
- *Optional*: `nvidia-container-toolkit` (for NVIDIA GPU passthrough)

## ⚠️ Before You Begin – Must Read

Lasper is in **early development**. **All users must read [CAVEATS.md](CAVEATS.md) before using Lasper.**
The elevated daemon trust model and current dependency exceptions are documented
in [SECURITY.md](SECURITY.md).
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

It's recommended to use `machinectl` to connect to the container after creation and starting. For example: `sudo machinectl shell <user_name>@<container_name>`. Ensure that you installed systembus and an init program inside your container.

**Keybindings:**
- `j` / `k` or `↓` / `↑` : Navigate
- `Enter` / `x` : Open Action Power Menu (Start, Poweroff, Reboot, Terminate, Kill, Enable, Disable)
- `Tab` : Toggle Focus (Container List ↔ Detail Panel ↔ Terminal)
- `n` / `a` : Create a new container (Creation Wizard)
- `s` : Start selected container
- `S` : Poweroff selected container
- `D` : Delete selected container
- `t` : Open shell terminal (machinectl login)
- `T` : Maximize terminal (when terminal is focused)
- `r` : Manual refresh
- `R` : Toggle panel resize mode
- `[` / `]` or `Alt + 1-5` : Switch view panes (Properties / Details / Logs / Config / Metrics)
- `?` : Show help
- `q` : Quit
- `Esc` : Back / Close Overlays

## Roadmap / TODO

- [x] Component-based TUI architecture & Responsive layout.
- [x] DBus integration via `zbus` (with automatic CLI fallback for legacy systems).
- [x] Asynchronous background task scheduling for long-running deployments.
- [x] Resource monitoring (CPU/Memory usage).
- [ ] Interactive .nspawn configuration editor.
- [ ] Global `config.toml` for overriding default settings.
- [ ] Better OCI import support.
- [ ] Customizable post-deployment hooks and scripts.
- [ ] Customizable deployment scripts.

## Credits

The terminal emulator in `src/term/` is ported from [mprocs](https://github.com/pvolok/mprocs) by Pavel Volokitin (MIT license).

## License

GPL V2
