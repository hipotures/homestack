//! Proxmox node names.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Proxmox node name: a DNS label of letters, digits and inner hyphens.
///
/// Node names end up in API paths, so anything else is rejected before it can
/// reach Proxmox.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeName(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid node name: use 1-63 letters, digits and inner hyphens")]
pub struct NodeNameError;

impl NodeName {
    pub fn parse(value: &str) -> Result<Self, NodeNameError> {
        let bytes = value.as_bytes();
        let valid_len = (1..=63).contains(&bytes.len());
        let valid_chars = bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-');
        let valid_edges =
            bytes.first().is_some_and(u8::is_ascii_alphanumeric) && bytes.last().is_some_and(u8::is_ascii_alphanumeric);
        if valid_len && valid_chars && valid_edges {
            Ok(Self(value.to_owned()))
        } else {
            Err(NodeNameError)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NodeName {
    type Error = NodeNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<NodeName> for String {
    fn from(value: NodeName) -> Self {
        value.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
