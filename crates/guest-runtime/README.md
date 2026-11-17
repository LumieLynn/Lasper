# Lasper guest runtime

`lasper-guest-runtime` lets a host run commands and account shells inside a Linux container. Because the runtime runs inside the guest, it prepares each process with the guest's own accounts and execution context. The host receives either a PTY or separate pipes for stdout and stderr. The guest does not need systemd or a system bus.

The executable is built as a static PIE for x86_64 or aarch64, without libc. It makes Linux syscalls directly and maps the memory needed for its workspaces. On the host, [`guest-host-transport`](../guest-host-transport/README.md) provides the client API for the shared [`guest-protocol`](../guest-protocol/README.md). The [`guest-test-client`](../guest-test-client/README.md) supplies a command-line client and complete examples for nspawn.

## Starting the runtime

The broker can run directly, alongside a specified program, or alongside the guest's init. The examples below show all three entry paths. Unless identified as a host path, each path refers to the guest filesystem.

With `Boot=no`, run the broker as the container's main process:

```ini
[Exec]
Boot=no
ProcessTwo=no
Parameters=/opt/lasper/guest-runtime --broker /var/opt/lasper

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/opt/lasper/guest-runtime
```

In this configuration, the broker runs as PID 1. If `ProcessTwo=yes` is used instead, nspawn places its stub init at PID 1 and starts the broker beneath it. The `--broker DIRECTORY` entry requires the directory to exist and validates its ownership and permissions. Here, the tmpfs mount provides that directory.

To run a guest program alongside the broker, use the explicit wrapper instead:

```ini
[Exec]
Boot=no
Parameters=/opt/lasper/guest-runtime wrap -- /absolute/guest/program arg1 arg2

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/opt/lasper/guest-runtime
```

The `wrap` command starts a broker child and waits for it to bind its listener. It then calls `execve` to replace the wrapper with the specified program, which retains the same PID. The program must be supplied after `--`, since the wrapper has no default payload.

The broker waits until it can distinguish a successful `execve` from a failed handoff before admitting clients. It also retains a pidfd for the program and stops when that process exits.

For `Boot=yes` with a non-systemd rootfs, nspawn can start the wrapper through an otherwise absent init candidate:

```ini
[Exec]
Boot=yes

[Files]
TemporaryFileSystem=/var/opt/lasper:mode=0700
BindReadOnly=/absolute/host/path/lasper-guest-runtime:/lib/systemd/systemd
```

Before adding the bind mount, check that the guest's `/lib/systemd/systemd` and `/usr/lib/systemd/systemd` do not contain an installed init. The bind must not hide an existing init executable, and `/sbin/init` must still resolve to the guest's real init rather than back to the runtime. The test-client README includes a check that runs in a chroot so that absolute symlinks resolve within the guest filesystem.

When nspawn invokes either candidate path as PID 1, the runtime starts the broker and replaces itself with `/sbin/init`. It sets `argv[0]` to `/sbin/init` and forwards all remaining arguments unchanged.

As a result, `Parameters=` supplies arguments to the original init in this mode. Even an argument such as `wrap` or `--help` goes to init rather than being parsed by the runtime. When switching from `Boot=no`, remove any `Parameters=` value that previously launched the broker or wrapper.

## Runtime directory

By default, the broker stores `guest-exec.sock` and `broker.lock` in `/var/opt/lasper`. The directory must be owned by root in the guest and have mode 0700, while the socket and lock file use mode 0600. Before removing a stale socket, a broker must acquire the lock so that it cannot disturb another running instance. The lock file keeps the same inode across broker restarts.

The directory is outside `/run` because the guest init may mount over `/run` after the wrapper has started. Using `TemporaryFileSystem=/var/opt/lasper:mode=0700` also gives the broker writable storage when nspawn uses `ReadOnly=yes`. That storage lasts for the container's lifetime, and its mountpoint is provided by the container configuration.

Without a tmpfs mount, the wrapper can create the directory on a writable filesystem. It creates missing parents with mode 0755 and the final directory with mode 0700, leaving existing ownership and permissions unchanged. If the final directory does not satisfy the required ownership and mode, startup fails.

The direct broker entry does not create directories. If it is started with a custom directory, the host client must be configured to connect to the socket in that directory.

## Process ownership and execution

The broker listens for connections and starts a guardian for each one it admits, up to a current limit of 32 guardians. A connection continues to consume a slot until its guardian has been reaped, even if the socket has already closed. If a guardian fails, the broker also reclaims any descendants left behind.

Each guardian manages one preparation and the worker tree that belongs to it. After completing Hello and validating the preparation request, it sets up the worker and sends a `PrepareReady` response. At this point, the worker's account and I/O are prepared, but it still waits for a `Start` request before executing the program.

A `StartAccepted` response confirms only that the request was accepted. The host learns whether execution succeeded, and whether its resources were cleaned up, from subsequent `Exit` reports.

During account preparation, the worker resolves the user from the guest's local account files and sets its user ID and primary and supplementary groups. It changes to the account's home directory and builds the default environment, which caller-supplied variables can override.

For a login shell, the runtime uses the shell recorded in the account and constructs `argv[0]` using the login-shell convention. For an explicit command, it executes the supplied absolute path in the guest. Neither form authenticates a password or establishes a PAM or logind session.

For terminal execution, the host receives a PTY master. For piped execution, it receives separate readers for stdout and stderr, while the worker reads stdin from `/dev/null`. Output streams through those descriptors rather than being saved in an exit report. The host must drain both pipes concurrently to prevent either one from blocking the command.

When using a PTY, the host adapter forwards input and window-size changes and restores the host terminal afterward. It also decides how much output to retain.

A connection prepares one worker and allows at most one attempt to start it. If the host cancels the preparation, requests closure, or disconnects, the guardian cleans up that worker and its descendants.

After disconnect, cleanup continues even though the guardian can no longer report its result to the host. The host can confirm completion only if it receives and validates a final `Exit` report with `cleanup` set to `Complete`. Worker exit or output EOF alone cannot establish that all descendants were reclaimed. The [wire state machine](../guest-protocol/README.md#wire-state-machine) describes how these reports interact with outstanding requests.

The host treats this runtime as trusted guest code. Its credential and machine-membership checks identify the process that supplied a response, but they cannot independently establish that a compromised guardian executed the requested program. The host client's README describes the kernel and machine-registration support required for those checks.

## Budgets and diagnostics

Commands can run indefinitely unless the host supplies an execution timeout in its `Start` request. That timeout begins when the guest accepts the request. It does not include preparation or the time spent waiting for the host to start the worker, and it is independent of the host's API deadlines.

Whether started directly or by the wrapper, the broker uses the following environment variables to configure preparation:

| Variable | Default milliseconds | Purpose |
| --- | --- | --- |
| `LASPER_GUEST_HELLO_TIMEOUT_MS` | 1000 | Limits the time between accepting a connection and completing Hello. |
| `LASPER_GUEST_PREPARE_RECEIVE_TIMEOUT_MS` | 5000 | Limits the time allowed to receive a preparation request after Hello. |
| `LASPER_GUEST_WORKER_SETUP_TIMEOUT_MS` | 5000 | Limits the time allowed to prepare a worker after validating the request. |
| `LASPER_GUEST_PREPARED_LEASE_MS` | 45000 | Sets the lease granted when the worker becomes ready and on each renewal. |

Each value must be a positive decimal number of milliseconds. The first three may not exceed `MAX_EXECUTION_TIMEOUT_MS`. The prepared lease must fit every profile advertised by the broker; profile 1.0 permits up to 300000 ms. An invalid value prevents startup.

Once the guest accepts a start request, the prepared lease no longer limits the worker's lifetime. The host's renewal behavior and waiting deadlines are described under [host budgets and guest policy](../guest-host-transport/README.md#wait-budgets-and-guest-policy).

If startup fails, the runtime writes a diagnostic to stderr that identifies the failure class and operation, together with an errno when one is available. Failures within a session are reported through structured errors and exit reports, including the stage at which setup failed. These diagnostics are separate from the program's own output and exit status.

The runtime does not collect service logs or implement container shutdown. A consumer can run a guest command to retrieve logs or request shutdown using the tools installed in that image.

## Building and the standalone entry

From the repository root, use the alias for the guest architecture you need:

```sh
cargo build-guest-runtime-x86-64
cargo build-guest-runtime-aarch64
```

Both aliases select the `guest-runtime` build profile and enable the `runtime-bin` feature. They link through `rust-lld` without libc or a dynamic loader, so the corresponding Rust target must be installed before building. The resulting executable is written to `target/<target>/guest-runtime/lasper-guest-runtime` and must match the guest's architecture.

Before entering normal Rust code, the ELF entry applies relocations to the static PIE and protects the RELRO region. The broker and wrapper entry paths then switch to a mapped stack before forking. On aarch64, the link layout also accommodates systems with 16 or 64 KiB pages.

The compiler can emit calls to C ABI memory and string functions even in a libc-free executable. The `entry_memory` module supplies those symbols. Likewise, command-line help is generated during the build, so `clap` is needed by the build script but is not linked into the guest executable.

The runtime has no general-purpose allocator. Large buffers used for requests and worker setup reside in explicit memory mappings, whose required sizes follow from the selected protocol profile. The layout of request metadata is checked separately. The [protocol memory budgets](../guest-protocol/README.md#guest-memory-and-stack-budgets) describe how these mappings are owned and when they are released.

The functions that fork processes require a single-threaded caller with exclusive responsibility for creating and reaping children. Their unsafe API documentation states the requirements that an embedding caller must satisfy.

The following commands display help or emit build metadata without starting a broker:

```sh
target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime --help
target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime wrap --help
target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime --identify
```

The `--identify` option emits 32 bytes of binary build metadata rather than text. This record describes the artifact; the host still uses Hello to determine compatibility with a live broker. The [compatibility specification](../guest-protocol/COMPATIBILITY.md#static-runtimeidentity-and-live-negotiation) defines its fields.

## Source layout and testing

Execution begins in `main`, with `entry_relocation`, `entry_stack`, and `entry_memory` providing the support needed by the standalone ELF. From there, the wrapper path starts a broker before handing execution to its payload, while the direct broker path enters `broker` without a payload.

For each admitted connection, `broker` creates a guardian whose preparation and cleanup logic lives under `session`. That code uses `transport` to exchange framed messages and descriptors. The underlying kernel calls are implemented by `linux`, `syscall`, and `socket`. Shared paths and startup policy are defined in `paths` and `policy`, while `failure` and `diagnostic` provide error classification and reporting.

All tests live under `tests/`. The library and transport tests can be run from the repository root with:

```sh
cargo test -p lasper-guest-runtime --lib --test transport --locked
```

The session, guardian, control, and broker suites use custom harnesses that create real processes. They exercise the execution path from preparation through cleanup, including descriptor handoff and failures during account setup or execution. Some scenarios require root or permission to create the namespaces used by the test.

The entry and wrapper suites instead run the separately built executable. On a host that permits creating PID namespaces, run them with:

```sh
cargo test -p lasper-guest-runtime --test entry --test wrapper --locked -- \
    --sudo --pid1 \
    --binary target/x86_64-unknown-linux-musl/guest-runtime/lasper-guest-runtime
```

The `--sudo` flag re-executes the harness with elevated privileges, and `--pid1` enables the scenario that runs the wrapper as PID 1. On an aarch64 host, supply the aarch64 artifact instead.

To check the artifact's ELF layout, run `.github/scripts/verify-guest-runtime.sh TARGET`. The script verifies that the executable is a static PIE for the expected architecture and that its load segments and RELRO region have the required alignment. Adding `--run` also executes the artifact, so use it only on a host that can run that architecture natively. For deployment in nspawn and interactive checks, follow the test-client README.
