//! Shared contract-level enums and metadata used across protocol runtimes.

use serde::{Deserialize, Serialize};

/// Supported protocols in the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// HTTP REST from OpenAPI.
    Http,
    /// WebSocket from AsyncAPI.
    WebSocket,
    /// gRPC from protobuf.
    Grpc,
}

/// Validation direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDirection {
    /// Incoming request/message.
    Request,
    /// Outgoing response/message.
    Response,
}

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
