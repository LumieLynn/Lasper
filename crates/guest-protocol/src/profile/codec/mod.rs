//! Session codecs combine schema dispatch with negotiated limits and feature
//! checks. Identity packets use a separate schema and packet budget.

mod dispatch;
pub(super) mod fields;
mod identity;
mod negotiated;

#[cfg(test)]
pub(super) use dispatch::validate_timeout;
