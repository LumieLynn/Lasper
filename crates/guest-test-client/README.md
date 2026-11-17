# Guest runtime test client

`lasper-guest-test-client` connects to a guest runtime from the host. It can run a command in the guest or open an interactive shell, so you can test the runtime on its own. The client uses `guest-host-transport` for host-side connections and `guest-protocol` for the wire protocol.

Inside the container, the broker is the long-running guest runtime process. It listens on a Unix socket and manages command and shell sessions. The wrapper provides another way to start it: it forks the broker, waits until it is ready, and then calls `execve` for the requested program. The program inherits the wrapper's PID.

On the host, the client looks up the container through machined and locates the broker socket relative to a pinned handle to the guest root. It does not enter the container's namespaces. The host needs systemd, machined, and a system bus, while the guest does not need D-Bus. The `Boot=yes` setup described here preserves the image's original `/sbin/init`; it is intended for rootfs images without an existing systemd installation.

## Building

Build the runtime and client from the repository root:

```sh
cargo build-guest-runtime-x86-64
cargo build -p lasper-guest-test-client --locked
```

On an aarch64 host, use `cargo build-guest-runtime-aarch64`. In the runtime output path below, replace `x86_64-unknown-linux-musl` with `aarch64-unknown-linux-musl`. The runtime binary must match the guest's architecture.

The examples below use x86_64. In a host terminal, set the paths to both executables and replace `ROOTFS` with the location of your test rootfs. Use a disposable copy to keep the original image intact.

```sh
RUNTIME="$(realpath target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime)"
CLIENT="$(realpath target/debug/lasper-guest-test-client)"
ROOTFS=/absolute/path/to/test-rootfs
```

If you use a second terminal for the client, set `CLIENT` in that terminal too. The examples use `lasper-test` as the registered machine name; substitute your container's name when testing an existing machine.

Run `"$CLIENT" --help` for a command overview, or `"$CLIENT" exec --help` and `"$CLIENT" shell --help` for each command's arguments. The runtime also provides `--help` and `wrap --help`. None of these help commands require a running container.

## Starting the runtime inside a container

The runtime binary is bind-mounted read-only into the guest. Its socket and lock file are stored under `/var/opt/lasper`. Add the relevant settings below to the container's `.nspawn` file. In each `BindReadOnly=` entry, replace the host-side path with the absolute path in `RUNTIME`.

### Boot=no: run the broker directly

With `Boot=no`, start the broker directly as the container's main process. You can then use the client to run guest commands or open a shell:

```ini
[Exec]
Boot=no
ProcessTwo=no
Parameters=/opt/lasper/guest-runtime --broker /var/opt/lasper

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/opt/lasper/guest-runtime
```

In this configuration, the broker runs as PID 1. Set `ProcessTwo=yes` if you want nspawn's stub init to run as PID 1 and the broker as PID 2. The direct `--broker` entry expects the runtime directory to exist already; the tmpfs mount creates it before startup.

To start the broker and the container's intended command together, use the wrapper. It forks the broker and then calls `execve` for the requested program, which inherits the wrapper's PID. Specify the program explicitly after `wrap --`, as in `wrap -- /absolute/program [ARGS...]`.

If you omit the tmpfs mount, the wrapper creates any missing parent directories and the runtime directory on the guest's writable filesystem. It creates parent directories with mode 0755 and the runtime directory with mode 0700. Existing directories keep their current ownership and permissions.

To try the wrapper without a `.nspawn` file, run nspawn directly with the required mounts on its command line:

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

The command runs nspawn in the foreground, so use another host terminal for the connection tests. When `/bin/sleep` exits, the wrapper exits and the broker stops with it. To leave the broker running on its own, use the direct entry above. If the image does not contain `/bin/sleep`, choose another program that is present. Add `--as-pid2` to keep nspawn's stub init as PID 1.

### Boot=yes: run the guest's original `/sbin/init`

With `Boot=yes`, nspawn checks its init candidate paths before starting the guest. Bind the runtime to an unused systemd candidate so nspawn starts it as the entry point. The runtime forks the broker and then calls `execve` for the guest's original `/sbin/init`; the init process continues as PID 1. The broker uses the same `/var/opt/lasper` directory as in the `Boot=no` setup.

Use this setup only with an image that does not have systemd installed. Before adding the bind mount, check both candidate paths inside the rootfs. They must be absent: otherwise the mount would hide an existing program. Checking inside the rootfs also confirms that symlinks resolve against the guest filesystem. Run:

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

When changing from `Boot=no` to `Boot=yes`, remove the `Parameters=` value that launched the runtime directly. In `Boot=yes` mode, nspawn passes `Parameters=` to the original init through the runtime. Leave it unset unless the init needs arguments. For example, if `Parameters=wrap` is present, `wrap` becomes an argument to init; the runtime does not treat it as a subcommand.

Save the configuration and start the container as usual. If you replace the runtime binary, restart the container to load the new one. The candidate-path check assumes the runtime has not already been bound into the rootfs. Earlier tests may leave an empty directory at a previous bind target; if so, the script above will report that path as occupied.

## Connecting to the container

On the host, confirm that the container is registered:

```sh
machinectl show lasper-test -p Name -p Leader -p Unit
```

The host kernel must support pidfd and `SO_PEERPIDFD`, and machined must report a nonzero `LeaderPIDFDId`. If any of these requirements is missing, the client reports an error; it does not fall back to identifying the process by numeric PID alone. The examples use `sudo` because the client needs access to host procfs data and the guest's private socket.

### Running commands

First, check that stdout, stderr, and the guest exit code all reach the host:

```sh
sudo "$CLIENT" exec --machine lasper-test --user root -- \
    /bin/sh -c 'printf "stdout\n"; printf "stderr\n" >&2; exit 7'
printf 'client exit=%s\n' "$?"
```

The output should contain both lines, followed by `client exit=7`. This confirms that the guest command's output and exit status reached the host. If the host shell has `set -e` enabled, turn it off for this test because the guest command intentionally exits with a nonzero status.

The `--` after `exec` marks the end of client options; everything after it is the guest command. Give the command an absolute path inside the guest. The default account is root. To use another account, pass its name with `--user`; that account and its programs must already exist in the image.

By default, commands have no execution time limit. Pass `--timeout SECONDS` to set a positive whole-second limit; this option also works with `--tty`. Before connecting, the client rejects values above the protocol's maximum. The guest starts the timer when it accepts Start, so preparation time and any wait before Start do not count. For example, this command has up to three minutes to run:

```sh
sudo "$CLIENT" exec --machine lasper-test --timeout 180 -- /bin/sleep 120
```

For ordinary `exec`, stdout and stderr travel through separate pipes and are forwarded as they arrive. The client does not capture or truncate the output, and the execution timeout does not limit output size. Once the command finishes, the exit report includes its status and the cleanup result.

If writing to either host output stream fails, the client requests session cleanup and continues reading the other stream. SIGTERM also requests cleanup. In ordinary `exec` mode, SIGINT does the same; in interactive mode, Ctrl-C is sent through the PTY to the guest process. After the client receives the final session status, it gives the output streams one second to close. If they remain open, it reports an error.

When the worker exits normally and cleanup completes, the client returns the command's exit code, or `128 + signal` if it was killed by a signal. The client can still report a known exit status if the command finishes before the guardian confirms that `exec` succeeded. For a session failure, including a timeout, the client exits with status 125 and prints a diagnostic even if the worker later exits with status 0. If the client cannot determine whether Start was accepted, the diagnostic warns that the command may have run; the client does not retry it. Diagnostics for guest setup and `exec` failures include the failing stage and errno. Invalid command-line arguments produce exit status 2.

### Opening an interactive shell

Run the following command in an interactive terminal on the host:

```sh
sudo "$CLIENT" shell --machine lasper-test --user root
```

The `shell` command starts the login shell listed for the selected guest account. It sets `HOME` and the working directory from that account's record. It does not show a login prompt, check a password, or register a PAM or logind session. To open a shell as a regular user, choose an account that already exists in the image and make sure its home directory is present.

Once inside, run `id`, `pwd`, and `printf '%s\n' "$0" "$SHELL" "$HOME"` to check the account and shell environment. The `tty` command should show the guest's PTY. Run `stty size`, resize the host terminal window, and run it again; the dimensions should change accordingly. Run `sleep 30` and press Ctrl-C to check that terminal signals reach the guest program.

To choose a shell yourself, or to run another interactive program, use `exec --tty`. For example, if Bash is installed in the guest:

```sh
sudo "$CLIENT" exec --tty --machine lasper-test --user root -- /bin/bash -l
```

To check the client's exit code, enter `exit 7` in the guest shell. After the host prompt returns, run this command at the host prompt:

```sh
printf 'client exit=%s\n' "$?"
```

Do not paste the `printf` command together with the command that starts the shell. The guest shell could read it as input. After the shell exits, the host terminal's echo and newline handling should work normally again.

The client connects to `/var/opt/lasper/guest-exec.sock` by default. When testing another runtime directory, specify the endpoint with `--socket /guest/path/to/guest-exec.sock`. This path is resolved within the guest root.

## Observing startup and diagnosing failures

After connecting to a shell with `Boot=yes`, inspect `readlink /proc/1/exe` and `/proc/1/cmdline` inside the guest to confirm that PID 1 is now the original init. The executable may be busybox on a BusyBox image, and Void's runit-init may go on to execute runit. The final command line does not have to keep showing `/sbin/init`. The `/var/opt/lasper` directory should be owned by root with mode 0700, and its socket and lock file should have mode 0600.

If a connection fails, save the startup configuration, the client's stderr output, and its exit code. If the container is still running, collect host-side state with `collect-diagnostics.sh`:

```sh
# make sure you're in the project root.
mkdir -p target/guest-runtime-test-logs
sudo bash crates/guest-test-client/scripts/collect-diagnostics.sh \
    lasper-test target/guest-runtime-test-logs/lasper-test-state.txt
```

The script records the machined registration, procfs details for the machine leader, socket metadata, and host logs. It neither enters the guest's namespaces nor changes the guest. It will not overwrite an existing output file, so use a new filename for each run. When reporting a problem, include the image source and version, its architecture, and the runtime binary used for the test.

If the guest has no journal data, `journalctl -M` may produce no output. In that case, inspect the terminal output of a foreground nspawn process or query the host unit running the container:

```sh
unit=$(machinectl show lasper-test -p Unit --value)
sudo journalctl -b -u "$unit" -n 100
```

These logs mainly contain startup and console output. They can also include terminal control bytes or repeated errors from guest services. What other guest logs are available depends on the logging service in the image.

To stop the experiment, run `sudo machinectl terminate lasper-test`. To test shutdown from inside the guest, run `poweroff` in its shell. The container may stop before the client receives the final status, in which case the client reports a disconnection. Check whether nspawn has exited, the machined registration is gone, and any guest processes remain. To continue testing, start the container again and reconnect to the new machine instance.

## Testing and guest results

Run local regression tests from the repository root:

```sh
cargo test -p lasper-guest-test-client --locked
cargo test -p lasper-guest-runtime --test wrapper --locked -- \
    --binary target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime
```

The wrapper tests cover program handoff, runtime-directory setup and validation, cleanup after an `exec` failure, and rejection of recursive invocation. Add `--pid1` to exercise the PID 1 entry path; that test requires permission to create a PID namespace and mount its procfs.

The runtime has been tested with LXC rootfs images for alpine-edge, devuan-excalibur, slackware-current, voidlinux, and gentoo-openrc. Each image worked in all three modes: `Boot=yes`, `Boot=no` with the broker running directly, and `Boot=no` with the wrapper starting another program.

In the gentoo-openrc image used for these tests, `/sbin/init` points to SysVinit by default. Adjust the image's init configuration if you want to test OpenRC's init entry point instead.
