## Frequently Asked Questions

**Q: Why can't the container created from an OCI image start?**  
A: See [CAVEATS.md](CAVEATS.md) for details. This issue will be addressed in a future update.

**Q: Under veth or bridge mode, why does my container have no network?**  
A: When configuring containers in these two modes, lasper enables systemd's built-in `systemd-networkd` and `systemd-resolved` to manage networking. Simply enable both services on your host, and systemd will automatically configure the container's network and `/etc/resolv.conf`. Enabling them does not conflict with NetworkManager; please refer to the relevant documentation for more details.

**Q: Can lasper run on non-systemd init systems?**  
A: While it is possible to run `systemd-nspawn` containers on non-systemd init systems, you may attempt to use lasper after ensuring compatibility between `systemd-nspawn` and your init system. Note that this scenario has not been tested; use at your own risk.

**Q: Can I specify a custom container directory?**  
A: Not yet. After the upcoming DBus refactoring, we plan to introduce more flexible container directory configuration options.

**Q: Can I specify a bootstrap installer other than `pacstrap` or `debootstrap`?**  
A: Not yet. Future plans include supporting more bootstrap tools and custom installation scripts.