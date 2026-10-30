//! Validate startup-configured display binds against the running guest.
//!
//! Each display owns its projection policy and observation parser. [`probe`]
//! supplies the shared filesystem script, framing, and bounded PTY collector;
//! host discovery and X server access management remain in platform adapters.

mod probe;
mod wayland;
mod x11;

pub(super) use wayland::automatic_wayland;
pub(crate) use wayland::WaylandSessionResolver;
pub(crate) use x11::X11SessionResolver;
