## FAQs

**Q: What is systemd-nspawn?**  
A: `systemd-nspawn` is a lightweight container engine that uses Linux namespaces and control groups to run an entire operating system or individual applications in an isolated environment. Unlike Docker, it is primarily designed for "system containers" that behave like lightweight virtual machines. For more details, see the [official documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd-nspawn.html).

**Q: Why I can't creat containers via bootstraps?** 
A: Ensure that you've installed the correct keyring for your distribution. For example, creating an Arch Linux container needs to install `archlinux-keyring` package. It's the same as creating a container via `debootstrap` on Debian/Ubuntu.

**Q: How should I remove the container?** 
A: You can use `sudo machinectl remove <container_name>` to remove a container. Due to some permission settngs, lasper creates some systemd configuration files in `/etc/systemd/system/` to ensure the correct `rw` mount for GPU passthrough. You may need to remove them manually. Future updates will add options for configurating the configuration files' path (continuously in `/etc/systemd/system` or temporarily in `/run/systemd/system`) and a better delete method. 

**Q: Why can't the container created from an OCI image start?**  
A: See [CAVEATS.md](CAVEATS.md) for details. In containers created by lasper via OCI images, it defaultly sets `boot=no` due to the lack of init program and systembus in OCI images. Setting passwords and users may fail when deploying due to some permission error. You can try to start the container manually with `sudo systemd-nspawn -D /var/lib/machines/<container_name>` and install a proper init program like systemd, and a systembus daemon like dbus. After doing these, don't forget to set `boot=yes` in your `.nspawn` config if you want to use the container with `machinectl`.

**Q: Under veth or bridge mode, why does my container have no network?**  
A: When configuring containers in these two modes, lasper enables systemd's built-in `systemd-networkd` and `systemd-resolved` to manage networking in the container. Simply enable both services on your host, and systemd will automatically configure the container's network and `/etc/resolv.conf`. Enabling them does not conflict with NetworkManager, but for ufw and firewalld users, please refer to the relevant documentation for more details. 

If you're desktop users, `systemd-networkd-wait-online.service` may slow down your boot time. You can disable it by running `sudo systemctl disable systemd-networkd-wait-online.service`, but don't mask it, or it will fail to automatically set up the NAT rules for your container.

If you don't plan to use `systemd-networkd` to manage your container's network, you may need to manually sets the `iptables` rules to enable NAT for your container. For example, you can use the following commands:
```bash
sudo iptables -t nat -A POSTROUTING -s <container_ip> -o <host_interface> -j MASQUERADE
```

**Q: Can lasper run on non-systemd init systems?**  
A: While it is possible to run `systemd-nspawn` containers on non-systemd init systems, you may attempt to use lasper after ensuring compatibility between `systemd-nspawn` and your init system. Note that this scenario has not been tested; use at your own risk.

**Q: Can I specify a custom container directory?**  
A: Not yet. In current version, the container directory is hardcoded to `/var/lib/machines/<container_name>`. It's recommended that you add a symlink of your container's rootfs here. Future plans may include adding customized container directory via config file.

**Q: Can I specify a bootstrap installer other than `pacstrap` or `debootstrap`?**  
A: Not yet. Future plans include supporting more bootstrap tools and custom installation scripts.