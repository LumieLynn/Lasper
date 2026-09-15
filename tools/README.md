# Development tools

This directory contains standalone utilities for developing and validating
Lasper: inspecting host behavior, reproducing integration issues, and checking
assumptions before they become part of the application. Each tool's section
describes its purpose, required environment, invocation, output, and any changes
it makes to the system.

These tools are run explicitly by developers. They are not installed as Lasper
commands or started by the TUI or daemon. Rust tools registered as Cargo examples
participate in the normal build, lint, and test checks. Reusable tool source and
usage documentation belong here; local research notes, reference source trees,
and machine-specific output remain outside version control.

## X11 inspection

`x11-inspect` is a read-only development tool for the Configure / X11 work. Run
it as the current desktop user:

```sh
cargo run --example x11-inspect --
cargo run --example x11-inspect -- --display :0
cargo run --example x11-inspect -- --display :0.1 --socket /tmp/.X11-unix/X0_
```

It defaults to `$DISPLAY`, accepts local `:N[.S]` and `unix/:N[.S]` selections,
and connects directly to `/tmp/.X11-unix/XN` or the supplied absolute path.
An explicit socket path is a candidate to inspect; the tool does not establish
that an alternate `X0_` endpoint belongs to the server normally selected by `:0`.
It does not search for other displays or fall back to abstract sockets or TCP.

The JSON report contains:

- The display and screen selection, socket path and filesystem metadata.
- X11 setup information and the result of the server's `ListHosts` request.
- Access control mode and the exact address bytes of every ACL entry as hex.
  Server-interpreted entries also expose UTF-8 type/value text where available;
  usernames and numeric `#UID` values remain distinct.
- Whether an Xauthority entry was supplied, and any authority lookup error.
  Cookies are never printed. A missing authority file can still allow a
  connection through the server's existing access policy.

Inspection has a five-second deadline, excluding Cargo build time. Errors go
to stderr and return a nonzero exit status. Socket metadata is sampled before
and after the query; a detected path replacement or metadata change fails the
inspection. These samples and the server's setup fields are not a durable
server-generation identifier.

Successful inspection establishes that this desktop process can read this
endpoint's ACL. It does not prove that a container user can connect, that an ACL
entry was created by Lasper, or that the caller can change it. Socket ownership
is reported without imposing Wayland's UID equality requirement on X11.

The tool issues no access-control changes, probes no container, and writes no
configuration or authorization records. It is a Cargo example, excluded from
the Lasper executable. `x11rb` is a development dependency already present in
the dependency graph through the clipboard implementation.

```sh
cargo test --example x11-inspect
```

Tests use temporary sockets and a small in-process X11 fixture. They require
neither a running desktop server nor changes to the user's X11 access list.
