//! Shared contract-level enums and metadata used across protocol runtimes.

use serde::{Deserialize, Serialize};

/// Runtime mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MockMode {
    /// Return mocked responses.
    #[default]
    Mock,
    /// Forward to upstream and validate upstream responses.
    Proxy,
}
