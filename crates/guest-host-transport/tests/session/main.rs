mod deadlines;
mod expiry;
mod failure;
mod fixture;
mod identity;
mod io;
mod lease;
mod lifecycle;
mod preparation;
mod start;

#[path = "../support/control.rs"]
mod control;
#[path = "../support/session_clock.rs"]
mod session_clock;
#[path = "../support/target.rs"]
mod target;
// The shared fixture's terminal-release variant is used by tests/preparation.rs.
#[allow(dead_code)]
#[path = "../support/worker.rs"]
mod worker;
