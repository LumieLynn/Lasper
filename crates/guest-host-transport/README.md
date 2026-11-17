# Lasper guest host transport

`lasper-guest-host-transport` lets a Linux host run a command or shell inside an nspawn container that is running the Lasper guest runtime. The main entry point is `GuestClient`, which prepares the process and returns access to its I/O.

This README describes how to use the public API and who owns an execution's resources. The messages exchanged with the guest are defined by the [protocol crate](../guest-protocol/README.md).

## Purpose and entry points

The guest broker must already be running before the host connects. After choosing an account and program, the caller prepares a worker and retains the returned `Session` while observing execution and cleanup.

A `GuestClient` handles the protocol exchange and verifies that the worker belongs to the target machine. Callers that need to exercise the exchange directly, such as protocol tests, can use the lower-level interfaces in `raw`.

The objects involved in one execution have the following relationship:

```text
GuestClient          pins one machine instance; can initiate independent executions
  prepare(spec)
    Prepared
      session        controls this execution, maintains preparation, requests cleanup
      io             PTY, or separate stdout/stderr pipe readers
    session.start()
    session.wait()   returns final Completion
```

Cloning a `GuestClient` shares its target and configuration, but each call to `prepare()` opens a separate connection. That connection can prepare only one worker and submit only one `Start` request.

The resulting `Session` is the sole owner of control over that execution and cannot be cloned. Although the caller may pass its data FDs or state subscriptions to other tasks, it must retain the session for as long as it wants to keep the execution alive.

## Target and client

Async operations run on Tokio. The signatures below use `BTreeMap`, `Duration`, `Arc`, and the FD types from the standard library, together with `tokio::time::Instant` for deadlines.

```rust
#[derive(Clone)]
pub struct GuestClient { /* private */ }

#[derive(Clone)]
pub struct ClientOptions {
    pub endpoint: String,
    pub control_limits: ControlLimits,
    pub required_features: FeatureSet,
    pub timeouts: ClientTimeouts,
}

impl Default for ClientOptions;

impl GuestClient {
    pub fn new(
        target: Arc<MachineTarget>,
        options: ClientOptions,
    ) -> Result<Self, RequestError>;

    pub fn target(&self) -> &Arc<MachineTarget>;

    pub async fn probe(&self, deadline: Instant)
        -> Result<RuntimeInfo, BrokerConnectError>;

    pub async fn prepare(&self, spec: ProcessSpec, deadline: Instant)
        -> Result<Prepared, PrepareError>;
}

#[derive(Clone, Debug)]
pub struct RuntimeInfo {
    pub hello: NegotiatedHello,
}

pub struct Prepared {
    pub session: Session,
    pub io: SessionIo,
}
```

To construct a client, first call `MachineTarget::resolve(source, machine)` with a source of machine registration information. The resulting target pins that particular running instance. If the instance stops, resolve the machine again and create a new client rather than reusing the old target.

By default, the client connects to `/var/opt/lasper/guest-exec.sock` and uses `ControlLimits::CURRENT` and `ClientTimeouts::default()`. Its `required_features` set is initially empty; any configured requirements are added to the client's baseline requirements during Hello, alongside those imposed by the selected profile.

The `GuestClient::new` constructor validates these options. The endpoint must be an absolute path with no control characters or `.` and `..` components, and the requested features must be implemented by the local build. The supported protocol range comes from the library's registered codecs.

The `probe()` method connects to the broker and checks its identity before completing the Hello exchange. It then returns a `RuntimeInfo` value and closes that connection. A later call to `prepare()` performs a new exchange on its own connection, rather than reusing the probe's result.

A call to `prepare()` proceeds as follows:

1. It validates the `ProcessSpec` and checks that the request fits within the local build's storage limits.
2. It connects to the broker, verifies the peer, and completes Hello to select the profile and limits for this connection.
3. It validates the request against that profile, encodes the complete preparation request, and checks both its frame size and the number of FDs needed by the response.
4. If those checks pass, it sends the request so that the guest can create a worker.
5. After receiving `PrepareReady`, it checks the delivered FDs and verifies the worker's identity and membership in the target machine before returning `Prepared`.

If a capacity check fails, `prepare()` returns `PrepareError::Capacity` and closes the connection before sending any part of the request. No worker or prepared lease is created on that path.

When preparation succeeds, the worker waits for `Start` while the background driver renews its lease. The caller already owns the I/O descriptors at this point, subject to the input timing rules described under [I/O ownership](#io-ownership-and-reading).

The connection and Hello exchange use `timeouts.connect_hello`, which defaults to one second. The entire preparation uses `timeouts.prepare`, which defaults to five seconds. Either value can be increased, but neither extends the deadline supplied by the caller.

If preparation fails or its future is cancelled, the driver stops renewing the lease and requests cleanup. The call does not wait beyond its deadline for cleanup to finish. Instead, the driver continues observing cleanup in the background for `timeouts.cleanup_observation`, which defaults to fifteen seconds.

## Wait budgets and guest policy

The `timeouts` field in `ClientOptions` controls how long the host waits for protocol operations. These defaults can be increased; they do not impose execution time limits on the guest command.

```rust
#[derive(Clone, Debug)]
pub struct ClientTimeouts {
    pub connect_hello: Duration,
    pub prepare: Duration,
    pub operation: Duration,
    pub cleanup_observation: Duration,
}

impl Default for ClientTimeouts;
```

| Field | Default | Start point and scope |
| --- | --- | --- |
| `connect_hello` | 1 second | Begins with the connection attempt and covers peer verification and the Hello exchange. |
| `prepare` | 5 seconds | Begins when `prepare()` is called and covers the full preparation, including worker verification. |
| `operation` | 5 seconds | Begins when `start()` or `recheck()` is called and includes its preliminary checks and response wait. |
| `cleanup_observation` | 15 seconds | Begins when cleanup observation is first triggered. Later calls or progress reports do not refresh it. |

Each field must contain a positive `Duration` that can be converted and added to an `Instant` without overflow. The client validates these requirements at construction.

For the first three budgets, an operation ends at the earlier of the caller's deadline and the time obtained by adding the configured duration to the start of the call. The connection phase of `prepare()` is also bounded by the overall preparation deadline. Increasing a configured duration therefore relaxes the default budget without extending the caller's deadline.

The cleanup budget has a different purpose: it limits how long the driver observes cleanup after requesting it. If that period expires, the host cannot confirm the result; it does not establish that cleanup failed in the guest.

The guest enforces its own preparation deadlines, which are not changed by the host's settings. If a guest needs more time to prepare a worker, configure both sides as described in the [runtime README](../guest-runtime/README.md#budgets-and-diagnostics).

The guest reports `remaining_lease_ms` in its initial `PrepareReady` response and in later replies whose status is `Prepared`. The host uses that reported value to schedule `KeepPrepared` requests. To allow for transmission delay, it conservatively treats the request's send time plus the reported duration as the local expiry time and begins renewal after one third of the remaining budget.

If a response arrives after that expiry time, the host does not use it to refresh the lease. Instead, it stops allowing execution to start and requests cleanup.

The Hello exchange does not synchronize these host and guest deadlines. In particular, the guest continues reclaiming the session even after the host stops waiting for cleanup.

## Execution requests

A `ProcessSpec` owns the account name, program arguments, and environment supplied by the host:

```rust
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub user: String,
    pub program: Program,
    pub stdio: Stdio,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum Program {
    LoginShell,
    Argv(Vec<String>),
}

#[derive(Clone, Debug)]
pub enum Stdio {
    Terminal { size: TerminalSize },
    Piped,
}

#[derive(Clone, Debug, Default)]
pub struct StartOptions {
    pub terminal_size: Option<TerminalSize>,
    pub execution_timeout: Option<Duration>,
}
```

The client re-exports the protocol's validated `TerminalSize` type. Environment variables are supplied through a `BTreeMap<String, String>`.

For example, the following request runs a command with separate stdout and stderr pipes:

```rust
let spec = ProcessSpec {
    user: "root".into(),
    program: Program::Argv(vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf 'stdout\n'; printf 'stderr\n' >&2; exit 7".into(),
    ]),
    stdio: Stdio::Piped,
    environment: BTreeMap::new(),
};
```

For `Program::Argv`, the first element supplies both the executable path and `argv[0]`. It must be a nonempty absolute path in the guest filesystem. The runtime executes that path directly, so shell syntax works only when the caller explicitly runs a shell, such as `/bin/sh -c ...`.

Profile 1.0 permits up to 4096 bytes in each argument and up to 256 arguments after `argv[0]`. The total allowance is 64 KiB, including NUL separators between arguments but excluding the final NUL. Arguments may be empty or contain newlines, but may not contain NUL. Both endpoints check these values against the selected profile.

An account name must be a nonempty ASCII string of at most 32 bytes. It must not begin with `-` or contain whitespace, control characters, `/`, `\`, or `@`. During preparation, the guest resolves the account and checks whether it can be used for execution. If those checks fail, the result includes the corresponding guest failure.

For `Program::LoginShell`, the runtime uses the shell recorded in the guest account database and constructs `argv[0]` according to the login-shell convention. To choose a shell explicitly, use `Program::Argv`, for example with `/bin/bash` as its first element. Either form can use a PTY.

Before execution, the guest sets the user and both primary and supplementary groups, constructs the account environment, and changes to the account's home directory. This does not perform password authentication; an interactive login through `login` or a getty is a separate mechanism.

Choosing `Stdio::Terminal` prepares a PTY and supplies its initial size through the protocol's `initial_size` field. Choosing `Stdio::Piped` instead prepares separate stdout and stderr pipes, with stdin connected to `/dev/null`. The [execution encoding](../guest-protocol/README.md#execution-kind-encoding) specifies how these choices appear on the wire.

The guest supplies `HOME`, `USER`, `LOGNAME`, `SHELL`, and `PATH` from the account defaults, adding `TERM=dumb` when a PTY is used. It then applies the caller's environment, replacing any default with the same name. Changing these variables does not change the credentials or working directory established during account preparation.

Because `BTreeMap` keeps keys in order, its entries can be encoded directly in the order required by the protocol. The [environment wire contract](../guest-protocol/README.md#environment-wire-contract) defines valid names and values, including how empty values override defaults and how additions are counted against capacity.

If a shell needs access to a host display or another host service, the application arranges the necessary authorization and mounts before execution. It then supplies the relevant variables through `ProcessSpec.environment`; the runtime treats them like any other environment entries.

Preparation fixes the account, program, I/O mode, and environment. The subsequent call to `start()` may supply only a final terminal size and an execution budget. If a caller supplies a terminal size for piped execution, the client rejects that option before submitting the request.

Leaving `execution_timeout` as `None` allows the command to run without a time limit. If a duration is supplied, it must be positive and, after rounding up to milliseconds, must not exceed the selected profile's `max_execution_timeout_ms`. Profile 1.0 sets that ceiling to `u64::MAX / 2`.

This budget starts when the guest accepts the `Start` request. It is independent of the deadline for the API call, the lease that keeps an unstarted worker prepared, and the time allowed for observing cleanup.

## Session operations

```rust
pub struct Session { /* private */ }

impl std::fmt::Debug for Session;

impl Session {
    pub fn runtime_info(&self) -> &RuntimeInfo;
    pub fn observation(&self) -> &WorkerObservation;

    pub async fn recheck(&mut self, deadline: Instant)
        -> Result<&WorkerObservation, BrokerSessionError>;

    pub async fn start(&mut self, options: StartOptions, deadline: Instant)
        -> Result<StartOutcome, StartError>;

    pub fn snapshot(&self) -> SessionSnapshot;
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<SessionSnapshot>;

    pub async fn close(&self) -> Completion;
    pub async fn wait(&self) -> Completion;
}

#[derive(Clone, Debug)]
pub enum StartOutcome {
    Accepted,
    Reported(ExitReport),
    Unknown { cause: Arc<BrokerSessionError> },
}

#[derive(Clone, Debug)]
pub enum StartError {
    NotSubmitted(BrokerSessionError),
    Rejected(ErrorFrame),
}
```

The `WorkerObservation::reply()` method provides the account name and UID/GID reported by the guest. The `host_uid()` and `host_gid()` methods provide the credentials that the host kernel observed on the identity channel.

Both an explicit call to `recheck()` and the check performed internally by `start()` verify the worker's identity and its membership in the target machine. If either identity or membership is invalid, the driver terminates the preparation.

The `recheck()` and `start()` methods require the session to be in the `Prepared` phase. Otherwise, they return an `InvalidState` error without sending a request. Each method uses `timeouts.operation`, which defaults to five seconds, subject to the caller's deadline. If that deadline has already passed, the method returns `Deadline` before submission.

The result of `start()` distinguishes the following cases:

| Result | Known fact | Caller action |
| --- | --- | --- |
| `Ok(Accepted)` | The guest acknowledged the start request. | Use the I/O endpoints and await `Completion`. |
| `Ok(Reported(report))` | An exit report arrived before the start response. | Preserve the report and continue waiting for `Completion`. |
| `Ok(Unknown { cause })` | The request was submitted, but its response is unresolved. | Continue observing this execution or call `close()`; do not rerun the command automatically. |
| `Err(NotSubmitted(error))` | This start request was not submitted. | Inspect the error and the session state before deciding whether another call is allowed. |
| `Err(Rejected(error))` | The guest rejected the request, and the driver requested cleanup. | Inspect the rejection, then use `close()` or `wait()` to obtain `Completion`. |

An error detected before submission is returned as `StartError::NotSubmitted`. This also applies if the worker exits during the identity check, although its report is retained in the snapshot.

Once submission has begun, a write failure, response timeout, or driver interruption can leave the result uncertain. In that case, `start()` returns `StartOutcome::Unknown` rather than implying that the command did not run. Any later `Exit` report updates the execution and cleanup facts in the snapshot.

The client never retries a command automatically. A caller may correct invalid start options and call `start()` again only if the session remains in the `Prepared` phase and no request has been queued. Changing the account, program, or environment requires closing the preparation and creating another one.

Once a start request is queued, the session forbids a second submission. In particular, an uncertain outcome must be handled by observing or closing the existing execution, not by automatically running the command again.

The `close()` method sends a `Cancel` request if execution has not been submitted. After submission, it sends `Close` instead, including when the start request was rejected. If an `Exit` report has already arrived, the driver only observes the cleanup in progress. It cannot send anything after disconnect, and a call made after the session reaches `Finished` returns the saved `Completion` immediately.

Repeated or concurrent calls to `close()` share the same cleanup request and result. The first event that triggers cleanup observation starts the `cleanup_observation` budget, which defaults to fifteen seconds. Later calls and progress reports do not extend its deadline.

The `wait()` method waits until observation is complete without requesting termination. If the worker has not started, the driver continues renewing its prepared lease. To terminate the session deliberately, call `close()`; to limit how long a command may execute, supply `execution_timeout` when starting it.

## Mapping wire contracts to client behavior

The [wire state machine](../guest-protocol/README.md#wire-state-machine) determines which messages may be exchanged. The table below shows how the client exposes those exchanges through its API.

| Wire event or condition | Client behavior and public result |
| --- | --- |
| Hello completes before a preparation request is sent. | `probe()` closes and returns the negotiation result. `prepare()` instead validates and sends its request. |
| The driver validates `PrepareReady` and its FDs. | The caller receives `Prepared`, containing a session in the `Prepared` phase and its `SessionIo`. The driver retains the identity channel and worker pidfd. |
| The start request enters the send queue. | The phase becomes `Starting` and submission becomes `Unconfirmed`. No second start request is allowed. |
| The driver receives `StartAccepted`. | Submission becomes `Accepted`. The phase also becomes `Accepted` unless cleanup has already begun. |
| The guest rejects the start request. | `start()` returns `Rejected`, and submission becomes `Rejected`. The driver enters `Closing` and sends `Close`; later calls to `close()` share that cleanup. |
| An exit report arrives before the start response. | `start()` returns `Reported`, leaving submission `Unconfirmed`. The phase becomes `Closing`, or `Finished` if the report is final. |
| An error occurs before submission. | `start()` returns `NotSubmitted`, retaining any existing report in the snapshot. |
| A write fails, the response wait expires, or the driver is interrupted after submission but before the response is resolved. | `start()` returns `Unknown` without retrying. The caller can continue observing or request closure. |
| The driver receives EOF during preparation. | `prepare()` fails. If it has already returned a prepared session, that session records the connection error with submission `NotSubmitted`. |
| The connection fails or the observation budget expires during cleanup. | `Completion` retains the submission state, latest valid report, and error. Without a final report, cleanup remains unconfirmed. |
| The driver receives a final exit report. | Control observation ends, but the caller continues draining each data endpoint until its own EOF or drain deadline. |

The client processes `KeepPrepared`, `Recheck`, and `Start` exchanges serially. A call to `close()` may interrupt the wait for a response and send a cleanup request, but the driver still retains enough information to validate the earlier response if it arrives later. Concurrent cleanup requests are combined into a single exchange.

The `operation` and `cleanup_observation` budgets govern the host's waiting behavior. They are not transmitted as deadlines in control messages.

Although the wire protocol permits access to a PTY as soon as it is delivered, consumers of this API must not write input before the session is accepted. They must also stop input when cleanup starts. These restrictions prevent early bytes from reaching a program that has not yet started.

Output should continue to be read even if the control exchange fails or finishes. For piped execution, read stdout and stderr concurrently so that neither pipe blocks the worker.

## States and completion

A session records its phase separately from the facts established by execution reports and the errors encountered during API operations. These values are exposed through a snapshot while the session is active and through `Completion` once observation ends.

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Prepared,
    Starting,
    Accepted,
    Closing,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartSubmission {
    NotSubmitted,
    Unconfirmed,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub phase: SessionPhase,
    pub submission: StartSubmission,
    pub report: Option<ExitReport>,
    pub error: Option<Arc<BrokerSessionError>>,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub submission: StartSubmission,
    pub report: Option<ExitReport>,
    pub error: Option<Arc<BrokerSessionError>>,
}
```

The `Starting` phase means that a start request is queued but its response has not arrived. The `submission` field remains `Unconfirmed` until the driver receives acceptance or rejection. On acceptance, the phase becomes `Accepted` unless cleanup has already begun. On rejection, it moves directly to `Closing` and the driver requests cleanup; the session never returns to `Prepared` after a rejected start.

If an `Exit` report arrives before the start response, `start()` returns `StartOutcome::Reported`. The phase becomes `Closing`, or `Finished` if cleanup is complete, while `submission` remains `Unconfirmed`. The report can establish what the worker did without establishing whether the guest sent an acceptance or rejection response.

The table below gives the state resulting from each event. Once cleanup has begun or observation has ended, the phase remains `Closing` or `Finished`; a late response cannot move it back to an earlier phase. However, a valid start response may still resolve an `Unconfirmed` submission to `Accepted` or `Rejected`. Connection errors and cleanup requests do not overwrite a submission that was already confirmed.

| Start result or event | Phase | Submission | Behavior of `close()` |
| --- | --- | --- | --- |
| A correctable error returns `NotSubmitted` while preparation remains valid. | `Prepared` | `NotSubmitted` | The driver sends `Cancel`. |
| A failed precheck or early worker exit returns `NotSubmitted`. | `Closing`, then `Finished` | `NotSubmitted` | The driver sends `Cancel` if the guest has not begun cleanup and the connection is usable. Otherwise, it observes cleanup or returns the saved result. |
| The start request is queued and awaits a response. | `Starting` | `Unconfirmed` | The driver sends `Close`. |
| The guest accepts the start request. | `Accepted` | `Accepted` | The driver sends `Close`. |
| The guest rejects the start request. | `Closing`; cleanup has been requested. | `Rejected` | The driver reuses the existing cleanup request rather than sending `Cancel`. |
| An exit report arrives before the start response, producing `Reported`. | `Closing`, or `Finished` for a final report. | `Unconfirmed` until a valid outstanding start response arrives. | The driver observes the remaining reports without sending another cleanup request. |
| The response wait expires with a usable connection, producing `Unknown`. | `Starting` | `Unconfirmed` | The driver sends `Close`. |
| A connection or driver failure ends observation, producing `Unknown`. | `Finished` | `Unconfirmed` | The method returns the saved `Completion`, including its error, without sending anything. |

When `start()` returns a report without having received an acceptance response, the submission remains unconfirmed even if the report establishes that execution occurred. This distinction is intentional: `submission` records the response to the start request, while `report` records what happened to the process and its resources.

A session normally follows `Prepared → Starting → Accepted → Closing → Finished`. It can enter cleanup earlier if preparation is cancelled, its lease expires, or the worker exits before starting. A rejected start or an early exit report likewise moves it from `Starting` to `Closing`.

If acceptance arrives after cleanup has begun, it updates only `submission`. The phase stays `Closing` until observation ends. Faults follow the same cleanup observation path and eventually leave the session in `Finished`.

The `Finished` phase means that the host has stopped observing the control connection. It confirms cleanup only if it obtained a valid report for which `cleanup() == CleanupStatus::Complete`, as required by the [protocol](../guest-protocol/README.md#observability-of-cleanup).

If the connection failed or the observation period expired without that report, `Completion.error` records that the host could not confirm cleanup. It does not establish a cleanup failure in the guest. Conversely, EOF cannot invalidate a final report that has already been validated, and an earlier operation error may be retained alongside that report.

The snapshot retains the latest valid `ExitReport` and the first operation error that caused the session to close. Correctable local errors are returned by the method that detected them. Once observation ends, `Completion` preserves the final values of `submission`, `report`, and `error`, so they remain available after the caller releases the `Session`.

A `Completion` contains a report, an error, or both. If no error is present, its report must confirm that cleanup completed. A nonzero command exit, a failure to execute the program, or an execution timeout is described by `ExitReport` rather than inferred from the presence of a host error.

The client re-exports the protocol's report types, including `ExitReport`, `ExecutionStatus`, `ExitStatus`, `ExitReason`, `CleanupStatus`, `GuestFailure`, and `GuestFailureStage`. A consumer can use these structured values to choose an exit code or display a diagnostic. Command output remains separate and is read from the data FDs.

The `subscribe()` method returns a Tokio watch receiver, which exposes the latest snapshot rather than a complete sequence of events. Intermediate states may therefore be coalesced. Retaining a subscription does not keep execution alive; the caller must still retain the `Session`.

## I/O ownership and reading

```rust
pub enum SessionIo {
    Terminal(PtyMaster),
    Pipes {
        stdout: PipeReader,
        stderr: PipeReader,
    },
}

pub struct PtyMaster { /* private */ }
pub struct PipeReader { /* private */ }

impl tokio::io::AsyncRead for PtyMaster;
impl tokio::io::AsyncWrite for PtyMaster;
impl std::fmt::Debug for PtyMaster;
impl AsFd for PtyMaster;
impl PtyMaster {
    pub fn resize(&self, size: TerminalSize) -> std::io::Result<()>;
    pub fn into_owned_fd(self) -> OwnedFd;
}

impl tokio::io::AsyncRead for PipeReader;
impl AsFd for PipeReader;
impl PipeReader {
    pub fn into_owned_fd(self) -> OwnedFd;
}
```

A `Session` implements `Send` and `Sync`, while both I/O wrappers implement `Send` and `Unpin`. When reading from a PTY after its slave closes, `PtyMaster` interprets `EIO` as EOF. The same error is not treated as EOF by `PipeReader`.

The `PtyMaster::resize()` method changes the terminal's window size through an ioctl. Calling `into_owned_fd()` instead transfers ownership of the descriptor without changing its nonblocking mode. The recipient is then responsible for registering it with an event loop.

Before acceptance, the caller may read the PTY or change its window size, but must not forward input. Input can begin after `start()` returns `Accepted` or a snapshot confirms the `Accepted` phase. A `Reported` or `Unknown` outcome does not permit input, and forwarding must stop when the phase becomes `Closing` or `Finished`.

The `PtyMaster` wrapper owns an independent descriptor and does not enforce this timing itself. An early write can be buffered or echoed, generate terminal signals, or be consumed by the program after execution begins. Consumers remain responsible for input timing even if they access the descriptor through `AsFd`, extract it with `into_owned_fd()`, or duplicate it for another process.

Preparation delivers the data endpoints to the caller, and every possible result from `start()` leaves those endpoints available. The caller can therefore read startup diagnostics even when execution is rejected or its acceptance is uncertain. Reading ends when the stream reaches EOF, an I/O error occurs, the caller closes the endpoint, or its chosen drain deadline expires.

Dropping a `SessionIo` closes the descriptors it owns, which can affect the running program even without a control request. Closing the last reference to a PTY master causes a terminal hangup and `SIGHUP`. Closing the last reader of a pipe causes subsequent writes to generate `SIGPIPE` or fail with `EPIPE`.

The default action for these signals terminates the affected process, although a program may catch, ignore, or block them. Duplicated descriptors and references transferred to other processes still count when determining whether the last reference has closed.

To ignore output while allowing the command to continue, keep reading and discard the bytes rather than closing the reader. If the data endpoints are closed, retain the `Session` to observe the eventual result. To request termination deliberately, call `close()`; dropping the session also triggers cleanup through the control connection.

Both `start()` and `recheck()` borrow the session mutably, so they cannot run concurrently on it. In the example below, `copy_or_close()` needs a shared reference to the session and therefore begins draining only after `start()` returns. A task that owns only a `PipeReader` may begin reading earlier, but it cannot also borrow that session for control operations while `start()` holds the mutable reference.

The caller should read stdout and stderr concurrently and treat their EOFs separately from control completion. Buffered output may remain after `Completion` is available, so reading should continue within a drain budget chosen by the consumer. The consumer also decides how much output to retain.

Because pipes use kernel backpressure, a program can block when the caller stops draining its output. For terminal execution, the adapter is also responsible for putting the host terminal into raw mode, forwarding input and size changes, and restoring the terminal afterward.

## Errors and cancellation

```rust
#[derive(Clone, Debug)]
pub struct RequestError {
    pub field: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub enum PrepareError {
    InvalidRequest(RequestError),
    Connect(BrokerConnectError),
    Capacity(PrepareCapacityError),
    Session {
        error: BrokerSessionError,
        last_snapshot: SessionSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareCapacityError {
    FrameBytes { required: usize, available: usize },
    ReadyDescriptors { required: u16, available: u16 },
}
```

The `field` member of `RequestError` identifies the invalid input, such as `user`, `program.argv`, or `environment`. Its `message` provides a diagnostic for that field.

If preparation fails after a session driver has been created, `PrepareError::Session` retains the last snapshot while the driver continues observing cleanup. Regardless of the preparation error, no start request has been submitted, so the submission state is `NotSubmitted`.

For the `FrameBytes` variant, `required` is the size of the complete preparation frame encoded with the selected profile, including its header and any JSON escapes. The `available` value is the negotiated `max_control_frame_bytes`. A frame that exactly fills the allowance is valid.

For `ReadyDescriptors`, the two values compare the number of FDs needed by the response with the negotiated `max_descriptors`. Both checks take place before transmission. If both fail, the client reports `FrameBytes` first.

A request with valid fields can still exceed the connection's frame allowance, in which case preparation returns a capacity error. If the fields themselves exceed the profile's argument or environment limits, it returns `InvalidRequest` instead. Neither case is reported as a connection or transport failure.

The length check uses the same codec and environment ordering as transmission, and the complete frame is encoded and checked before it is handed to the transport. The caller may use `required` and `available` to adjust its request or limits before preparing again; the library does not reconnect automatically.

The `BrokerSessionError` enum distinguishes failures of the exchange from invalid local calls. Its `UnsupportedFeatures` variant includes the required and available feature sets, while `InvalidState` includes the session phase that prevented the operation. Public error types implement `std::error::Error` and expose an underlying source when one exists.

If Hello fails because the endpoints are incompatible, preparation returns `PrepareError::Connect` containing `BrokerConnectError::Incompatible`. If a later operation requires a capability that was not enabled, it instead reports `BrokerSessionError::UnsupportedFeatures { required, available }`.

The driver determines whether a start operation was cancelled before or after its request was queued. Before queueing, cancellation leaves the command unexecuted. After queueing, it requests cleanup, while retaining the submission state in both the snapshot and the final `Completion`.

Cancelling a `recheck()` call does not publish a new observation. If its request has already been sent, the driver finishes that exchange before processing another operation. By contrast, cancelling `wait()` only stops that particular wait. Once the driver has accepted a request from `close()`, cleanup continues even if the caller stops awaiting it; another call to `close()` or `wait()` can retrieve the result.

Dropping a `Session` requests cleanup and returns immediately, leaving the driver to observe the result within its configured budget. If the host process exits or its Tokio runtime stops, the socket closes and the guardian cleans up through its disconnect path.

Explicit calls to `close()` or `wait()` return a `Completion`, but can confirm cleanup only if the driver received a valid final `Exit` report. After disconnect, they return the saved result without reconnecting or treating a lack of confirmation as proof that cleanup failed.

## Examples

### Run a command and forward both output streams

The `run_command()` helper below forwards stdout and stderr to sinks supplied by the caller. Its result preserves the outcome of the start request, the final `Completion`, and the result of copying the output, so one does not hide another. After control observation ends, `drain_budget` limits how long the helper waits for the remaining output.

```rust
use lasper_guest_host_transport::*;
use std::{collections::BTreeMap, io, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    time::{timeout, Instant},
};

pub enum DrainResult {
    Complete {
        stdout: io::Result<u64>,
        stderr: io::Result<u64>,
    },
    Deadline,
}

pub struct CommandRun {
    pub start: Result<StartOutcome, StartError>,
    pub completion: Completion,
    pub output: DrainResult,
}

async fn copy_or_close<W: AsyncWrite + Unpin>(
    mut reader: PipeReader,
    mut writer: W,
    session: &Session,
) -> io::Result<u64> {
    let result = async {
        let bytes = tokio::io::copy(&mut reader, &mut writer).await?;
        writer.flush().await?;
        Ok(bytes)
    }
    .await;
    if result.is_err() {
        session.close().await;
    }
    result
}

pub async fn run_command<W1, W2>(
    client: &GuestClient,
    user: String,
    argv: Vec<String>,
    stdout_sink: W1,
    stderr_sink: W2,
    drain_budget: Duration,
) -> Result<CommandRun, PrepareError>
where
    W1: AsyncWrite + Unpin,
    W2: AsyncWrite + Unpin,
{
    let Prepared { mut session, io } = client
        .prepare(
            ProcessSpec {
                user,
                program: Program::Argv(argv),
                stdio: Stdio::Piped,
                environment: BTreeMap::new(),
            },
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
    let SessionIo::Pipes { stdout, stderr } = io else {
        unreachable!("Piped preparation must return pipes");
    };

    let start = session
        .start(
            StartOptions::default(), // No execution time limit.
            Instant::now() + Duration::from_secs(5),
        )
        .await;
    let drain = async {
        tokio::join!(
            copy_or_close(stdout, stdout_sink, &session),
            copy_or_close(stderr, stderr_sink, &session),
        )
    };
    tokio::pin!(drain);

    let should_close = matches!(&start, Err(_) | Ok(StartOutcome::Unknown { .. }));
    let finish = async {
        if should_close {
            session.close().await
        } else {
            session.wait().await
        }
    };
    tokio::pin!(finish);

    let (completion, output) = tokio::select! {
        streams = &mut drain => {
            (finish.await, DrainResult::Complete {
                stdout: streams.0, stderr: streams.1,
            })
        }
        completion = &mut finish => {
            let output = match timeout(drain_budget, &mut drain).await {
                Ok((stdout, stderr)) => DrainResult::Complete { stdout, stderr },
                Err(_) => DrainResult::Deadline,
            };
            (completion, output)
        }
    };
    Ok(CommandRun {
        start,
        completion,
        output,
    })
}
```

After `start()` returns, the helper polls the output readers and the completion future concurrently. If starting failed or its outcome is uncertain, it calls `close()`; otherwise, it calls `wait()`. When output ends first, it continues waiting for control completion. When control completes first, it allows the remaining output to drain within `drain_budget`.

While `start()` is still waiting, a full output pipe may block the command, but it does not prevent the guardian from sending its control response. Later, if copying either stream fails, the helper requests cleanup while continuing to drain the other stream. Bytes that were already written can therefore still be read as the guest terminates the command.

### Open an account shell for a terminal adapter

The `open_shell()` helper returns a session and its PTY only after the guest accepts the start request. For any other outcome, it returns an error that retains both objects, allowing the caller to read startup diagnostics before releasing them. This example uses the imports from the preceding example.

```rust
#[derive(Debug)]
pub enum ShellOpenError {
    Prepare(PrepareError),
    Start {
        outcome: Result<StartOutcome, StartError>,
        session: Session,
        pty: PtyMaster,
    },
}

pub async fn open_shell(
    client: &GuestClient,
    user: String,
    size: TerminalSize,
    environment: BTreeMap<String, String>,
) -> Result<(Session, PtyMaster), ShellOpenError> {
    let Prepared { mut session, io } = client
        .prepare(
            ProcessSpec {
                user,
                program: Program::LoginShell,
                stdio: Stdio::Terminal { size },
                environment,
            },
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .map_err(ShellOpenError::Prepare)?;
    let SessionIo::Terminal(pty) = io else {
        unreachable!("Terminal preparation must return a PTY");
    };

    let outcome = session
        .start(
            StartOptions::default(),
            Instant::now() + Duration::from_secs(5),
        )
        .await;
    match outcome {
        Ok(StartOutcome::Accepted) => Ok((session, pty)),
        outcome => Err(ShellOpenError::Start {
            outcome,
            session,
            pty,
        }),
    }
}
```

After `open_shell()` succeeds, the terminal adapter can forward input to the PTY until the session enters `Closing` or `Finished`. It passes window changes to `pty.resize()` and calls `session.close()` when the user closes the terminal. Because output may remain buffered after the command exits, the adapter continues reading until EOF or its drain deadline.

When the helper returns `ShellOpenError::Start`, the caller takes ownership of `session` and `pty`. It should keep input disabled and read the PTY for diagnostics while awaiting `session.close()`. Reading can continue after completion until the drain deadline, with the original start outcome retained alongside the completion and any diagnostics.

The caller can propagate this error to another layer, since the error retains the objects needed to finish handling the failed startup. Dropping it, however, releases both the session and the PTY, triggering cleanup and terminal hangup. A caller that wants to display startup diagnostics must therefore read them before dropping the error.

## Trust and ownership

A `Session` controls the lifetime of its execution. Its data FDs may be passed to another task, but retaining them does not keep the control connection alive. How much output to read or save remains the caller's decision.

The host relies on its kernel, procfs view, and machine registration source to establish which process it is communicating with and whether that process belongs to the target machine. It then relies on the guest runtime to prepare the requested account, execute the program, and clean up its descendants.

These checks assume that the guest runtime is trusted. They do not independently prove that a guardian created the worker for this preparation or delivered the requested start operation to it. In particular, they do not protect against a compromised guardian or guest administrator. The crate-level Rust documentation describes this trust boundary in full.

Each connection is responsible for one execution. If that connection is lost, the guest begins cleanup; the client cannot use another connection to reattach to the old execution.
