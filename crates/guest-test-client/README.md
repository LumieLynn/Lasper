# Guest runtime test client

`lasper-guest-test-client` connects from the host to a guest runtime inside a container to run commands or open an interactive shell. It uses `guest-host-transport` and `guest-protocol` to test runtime startup, connections, and sessions independently before integrating them into the Lasper TUI.

The broker is the guest runtime process that stays running inside the container. It listens on a Unix socket, handles client requests, and manages command and shell sessions. The wrapper starts the broker before running a specified program. It forks the broker, waits for it to become ready, then replaces itself with that program through `execve`, keeping the same PID.

The client runs on the host, looks up the container through machined, and locates the broker socket through a pinned handle to the guest root. It doesn't enter the container's namespaces. The host needs systemd, machined, and a system bus; the guest doesn't need D-Bus. These experiments currently focus on non-systemd rootfs images. The `Boot=yes` entry preserves the image's original `/sbin/init`.

## Building

Build the runtime and client from the repository root:

```sh
cargo build-guest-runtime-x86-64
cargo build -p lasper-guest-test-client --locked
```

On an aarch64 host, use `cargo build-guest-runtime-aarch64` and replace `x86_64-unknown-linux-musl` in the runtime output path below with `aarch64-unknown-linux-musl`. The runtime must match the guest's architecture.

The following commands use x86_64 as an example. In a host terminal, set the paths to both executables and replace `ROOTFS` with the actual location of your test rootfs. Use a disposable copy so you can keep the original image intact.

```sh
RUNTIME="$(realpath target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime)"
CLIENT="$(realpath target/debug/lasper-guest-test-client)"
ROOTFS=/absolute/path/to/test-rootfs
```

If you open another terminal to run the client, set `CLIENT` there as well. The examples below use `lasper-test` as the registered machine name. Replace it with the actual name when using an existing container.

Run `"$CLIENT" --help` for a command overview, or `"$CLIENT" exec --help` and `"$CLIENT" shell --help` for each command's arguments. The runtime also provides `--help` and `wrap --help`. None of these help commands require a running container.

## Starting the runtime inside a container

The runtime executable is made available inside the guest through a read-only bind mount. Its socket and lock file live at `/var/opt/lasper`. Add the `.nspawn` configuration below to your test container's settings file manually. Replace the source on the left side of `BindReadOnly=` with the actual absolute path stored in `RUNTIME`.

### Boot=no: run the broker directly

With `Boot=no`, you can run the broker as the container's main process, then connect with the client to run commands or open a shell:

```ini
[Exec]
Boot=no
ProcessTwo=no
Parameters=/opt/lasper/guest-runtime --broker /var/opt/lasper

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/opt/lasper/guest-runtime
```

Here, the broker itself is PID 1, with no placeholder program. With `ProcessTwo=yes`, nspawn's stub init becomes PID 1 and the broker runs as PID 2. The direct `--broker` entry requires the runtime directory to exist; the tmpfs above provides it before the broker starts.

To run the container's intended command alongside the broker, use the wrapper. It forks the broker, then replaces itself with that command without changing its PID. The syntax is `wrap -- /absolute/program [ARGS...]`; you must explicitly specify the program to run.

Without a tmpfs mount, the wrapper creates missing parent directories and the runtime directory on a writable filesystem. New parent directories have mode 0755, and the runtime directory has mode 0700. Existing directory ownership and permissions are left unchanged.

The following standalone `Boot=no` command lets you observe the wrapper handing execution over to another program. It does not read a `.nspawn` file; all required mounts are specified on the command line:

```sh
sudo systemd-nspawn \
    --settings=no \
    --directory="$ROOTFS" \
    --machine=lasper-test \
    --register=yes \
    --console=pipe \
    --tmpfs=/var/opt/lasper:mode=0700 \
    --bind-ro="$RUNTIME:/opt/lasper/guest-runtime" \
    /opt/lasper/guest-runtime wrap -- /bin/sleep 86400
```

This command stays in the foreground, so run the connection tests in another host terminal. When `sleep 86400` exits, the broker stops as well. To keep only the broker running, use the direct entry shown earlier. If the image has no `/bin/sleep`, substitute a program available in it. Add `--as-pid2` to keep nspawn's stub init as PID 1.

### Boot=yes: run the guest's original `/sbin/init`

With `Boot=yes`, nspawn searches its init candidate paths. Bind the runtime to one of the systemd candidate locations so nspawn starts it first. After forking the broker, the runtime replaces itself with `/sbin/init` without changing its PID, so the guest's `/sbin/init` runs as PID 1. This entry shares its startup logic with the explicit `Boot=no` wrapper, and the broker uses the same `/var/opt/lasper` directory.

This setup requires an image without an existing systemd installation. Before the first bind mount, check both candidate paths from inside the test rootfs. This avoids covering an existing program and ensures that absolute guest symlinks resolve inside the guest rather than on the host. Use the following script:

```sh
sudo chroot "$ROOTFS" /bin/sh -ec '
    test -x /sbin/init
    for candidate in /lib/systemd/systemd /usr/lib/systemd/systemd; do
        if test -e "$candidate" || test -L "$candidate"; then
            echo "Check failed: $candidate already exists; do not bind the runtime over it" >&2
            exit 1
        fi
    done
    ls -l /sbin/init
'
```

Once the checks pass, start the container with this configuration:

```ini
[Exec]
Boot=yes

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/lib/systemd/systemd
```

When switching from `Boot=no` to `Boot=yes`, remove the `Parameters=` value previously used to start the runtime. With `Boot=yes`, the guest runtime forwards `Parameters=` to the original init, so set it only when you need to pass arguments to init. In particular, when nspawn starts the runtime through an init candidate path, `wrap` is forwarded to init as an argument; it is not interpreted as a guest runtime subcommand.

Save the configuration and start the container as you normally would. After replacing the runtime executable, restart the container to use the new version. The candidate-path check is intended for a rootfs where the runtime has not yet been bound. Earlier tests can leave empty mount points behind, especially if you changed the bind target, which can cause the chroot script above to reject the rootfs.

## Connecting to the container

On the host, confirm that the container is registered:

```sh
machinectl show lasper-test -p Name -p Leader -p Unit
```

The current connection implementation requires host support for pidfd and `SO_PEERPIDFD`, and machined must provide a nonzero `LeaderPIDFDId`. If these capabilities are missing, the client returns an error rather than falling back to a numeric PID alone. The examples below use `sudo` to read host procfs information and access the guest's private socket.

### Running commands

Start with a command that checks stdout, stderr, and the exit code separately:

```sh
sudo "$CLIENT" exec --machine lasper-test --user root -- \
    /bin/sh -c 'printf "stdout\n"; printf "stderr\n" >&2; exit 7'
printf 'client exit=%s\n' "$?"
```

You should see both output lines followed by `client exit=7`. This confirms that the command ran inside the guest and that its output and exit status reached the host. If your host shell has `set -e` enabled, disable it before testing this expected nonzero exit code.

The `--` separator after `exec` separates client options from the guest command. The program must be specified by an absolute path inside the guest. The default user is root; use `--user` to select another existing guest account. The client does not create users or install programs.

Commands have no execution time limit by default. Use `exec --timeout SECONDS` to set a positive whole-second budget, including with `--tty`. The client rejects values above the protocol's maximum execution budget before connecting. The guest measures this budget from acceptance of Start; preparation and the wait before Start do not count. For example, the following command has three minutes to run:

```sh
sudo "$CLIENT" exec --machine lasper-test --timeout 180 -- /bin/sleep 120
```

Ordinary `exec` returns captured output after the command exits. Capture is limited to 64 KiB in total, independently of the execution timeout. Each stream is guaranteed half of the available budget and can borrow space its peer has not used; later stderr can reclaim its share even after stdout has filled the buffer. The client warns when output is truncated or incomplete. This mode is unsuitable for following continuous logs.

For an ordinary worker exit with complete cleanup, the client returns its exit code or `128 + signal`. A command that finishes before the guardian can confirm exec still retains its known exit status. Session failures, including the execution deadline, return 125 with a diagnostic even if the worker subsequently exits with status 0. If Start acceptance cannot be determined, the diagnostic states that the command may have run; the client does not retry it. Guest setup and exec failures retain their failed stage and errno in the diagnostic. Command-line argument errors return 2.

### Opening an interactive shell

Run the following command in an interactive terminal on the host:

```sh
sudo "$CLIENT" shell --machine lasper-test --user root
```

The `shell` command starts the shell listed in the guest account record as a login shell, with `HOME` and the working directory taken from that account. It does not display a login prompt, authenticate a password, or register a PAM/logind session. To use a regular user, replace `root` with an account that already exists in the image and make sure its home directory exists.

Once inside, run `id`, `pwd`, and `printf '%s\n' "$0" "$SHELL" "$HOME"` to check the account and shell environment. The `tty` command should show the guest's PTY. Run `stty size`, resize the host terminal window, and run it again; the dimensions should change accordingly. Run `sleep 30` and press Ctrl-C to check that terminal signals reach the guest program.

To choose a shell explicitly or run another interactive program, use `exec --tty`. For example, if the image has bash:

```sh
sudo "$CLIENT" exec --tty --machine lasper-test --user root -- /bin/bash -l
```

To check the interactive client's exit code, enter `exit 7` in the guest shell. Wait for the host prompt to return, then run this command separately:

```sh
printf 'client exit=%s\n' "$?"
```

Do not paste this command together with the command that starts the shell, or the guest shell may consume it. After exiting, host terminal echo and newline handling should work normally again.

The client connects to `/var/opt/lasper/guest-exec.sock` by default. When testing another runtime directory, specify the endpoint with `--socket /guest/path/to/guest-exec.sock`. This path is resolved within the guest root.

## Observing startup and diagnosing failures

After connecting to a shell with `Boot=yes`, inspect `readlink /proc/1/exe` and `/proc/1/cmdline` inside the guest to confirm that PID 1 is now the original init. The executable may be busybox on a BusyBox image, and Void's runit-init may go on to execute runit. The final command line does not have to keep showing `/sbin/init`. The `/var/opt/lasper` directory should be owned by root with mode 0700, and its socket and lock file should have mode 0600.

If a connection fails, keep the actual startup configuration, the client's stderr output, and its exit code. If the container is still running, use the repository's `collect-diagnostics.sh` script to collect host state:

```sh
# make sure you're in the project root.
mkdir -p target/guest-runtime-test-logs
sudo bash crates/guest-test-client/scripts/collect-diagnostics.sh \
    lasper-test target/guest-runtime-test-logs/lasper-test-state.txt
```

The script reads the machined registration, the machine leader's procfs information, endpoint metadata, and host logs. It does not enter guest namespaces or modify the guest. The output file must not already exist; choose a different filename for each collection. When reporting a problem, include the image source, version, architecture, and the runtime executable you used.

If the guest has no journal data, `journalctl -M` may produce no output. In that case, inspect the terminal output of a foreground nspawn process or query the host unit running the container:

```sh
unit=$(machinectl show lasper-test -p Unit --value)
sudo journalctl -b -u "$unit" -n 100
```

These logs mainly show startup and console output. They may contain terminal control bytes or messages from guest services that repeatedly fail. Complete guest logs depend on the logging service used by the image.

To end an experiment, run `sudo machinectl terminate lasper-test`. To test a normal shutdown initiated by the guest, run the guest's `poweroff` command from its shell. The container may exit before the client receives the final status, causing a disconnection error. Check whether nspawn has exited, the machined registration has disappeared, and any processes remain to determine whether shutdown completed. Restart the container and connect again to continue testing with the new machine instance.

## Testing and guest results

Run local regression tests from the repository root:

```sh
cargo test -p lasper-guest-test-client --locked
cargo test -p lasper-guest-runtime --test wrapper --locked -- \
    --binary target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime
```

The wrapper tests cover program handoff, creation and validation of the runtime directory and its parents, cleanup after exec failure, and rejection of recursive invocation. Add `--pid1` to test the PID 1 entry path. This requires an environment that allows creating a PID namespace and mounting its procfs.

Tests with LXC rootfs images for alpine-edge, devuan-excalibur, slackware-current, voidlinux, and gentoo-openrc succeeded in all three modes: `Boot=yes`, `Boot=no` running the broker directly, and `Boot=no` starting a program through the wrapper.

In the gentoo-openrc image used for these tests, `/sbin/init` defaults to SysVinit. To use OpenRC's init entry point instead, adjust the image's init configuration.
