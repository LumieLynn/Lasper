//! X11-specific configuration page contracts.
//!
//! Persistent declarations and edits live here; runtime ACL authorization is
//! intentionally kept in the sibling application X11 service.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum X11BindingScope {
    Directory,
    Socket { display: u16, alternate: bool },
}

/// A declaration recognizable from the standard host X11 path. The display
/// number is a filename hint, not evidence of a live server or authorization.
#[derive(Serialize, Deserialize)]
pub struct X11BindingDeclaration {
    pub line: usize,
    pub source: PathBuf,
    pub guest_target: PathBuf,
    pub readonly: bool,
    pub options: Vec<String>,
    pub scope: X11BindingScope,
}

/// Policy for a newly selected X11 endpoint. This is derived from the
/// inspected startup configuration; callers cannot choose a mount suffix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum X11BindRecommendation {
    Ready {
        private_users: String,
        idmapped: bool,
    },
    Unsupported {
        private_users: String,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum X11BindingChange {
    Add {
        source: PathBuf,
    },
    Update {
        line: usize,
        source: PathBuf,
        guest_target: PathBuf,
        readonly: bool,
    },
    Remove {
        line: usize,
    },
}

impl X11BindingChange {
    pub fn declaration_line(&self) -> Option<usize> {
        match self {
            Self::Add { .. } => None,
            Self::Update { line, .. } | Self::Remove { line } => Some(*line),
        }
    }
}
