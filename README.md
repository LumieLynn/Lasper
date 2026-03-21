# Lasper

A terminal user interface (TUI) for managing `systemd-nspawn` containers, written in Rust. Inspired by [lazydocker](https://github.com/jesseduffield/lazydocker), Lasper brings an interactive experience to systemd-nspawn container management.

![demo.gif](demo.gif)

## Features

- **Container Management**: Start, stop, and terminate systemd-nspawn containers. View container properties and journal logs directly in the terminal.
- **Creation Wizard**: Interactively generate `.nspawn` configurations (network modes, port forwarding, bind mounts, user namespaces).
- **Image Provisioning**:
  - Pull and extract OCI (Docker/Podman) images via `skopeo` and `umoci`.
  - Bootstrap native Debian/Ubuntu or Arch systems via `debootstrap` or `pacstrap`.
- **Hardware Passthrough**: Automated NVIDIA GPU device allocation and Wayland/X11 socket mounting for GUI apps.
- **Storage Backends**: Supports Directory, Btrfs subvolumes, and Raw sparse images.

## Prerequisites

- `systemd-container` (provides `machinectl` and `systemd-nspawn`)
- Root privileges (run via `sudo`)
- *Optional*: `skopeo` and `umoci` (for OCI image support)

## ⚠️ Before You Begin – Must Read

Lasper is in **early development**. **All users must read [CAVEATS.md](CAVEATS.md) before using Lasper.**  
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

Start the UI:

```bash
sudo lasper
```

**Keybindings:**
- `j` / `k` or `↓` / `↑` : Navigate
- `Enter` : Select / Next Step
- `Esc` : Back / Cancel
- `n` / `a` : Create a new container
- `s` / `S` / `x` : Start / Stop / Terminate selected container
- `p` / `l` / `c` : Switch views (Properties / Logs / Config)
- `q` : Quit

## Roadmap / TODO

- [ ] Better UI layout.
- [ ] Resource monitoring.
- [ ] `.raw` file custom partition.
- [ ] DBus integration via `zbus` (replace `machinectl` CLI parsing).
- [ ] Advanced networking config (`macvlan`, `ipvlan`, IP address customizing, etc.).
- [ ] Global `config.toml` for overriding defaults and writing custom pre/post-deployment hooks.
- [ ] Customizable deployment scripts.
- [ ] Background task scheduling for long-running deployments.

## License

GPL V2