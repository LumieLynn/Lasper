//! Selection requests produced by resource-list controls.

#[derive(Debug, Clone, PartialEq)]
pub enum ListMessage {
    Next,
    Prev,
    Select(usize),
}
