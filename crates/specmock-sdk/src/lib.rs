//! Rust SDK for embedding and process-mode spec mock runtime.

mod server;

pub use server::{MockServer, MockServerBuilder, ProcessMockServer, SdkError};
// Re-exported so the CLI can share the runtime's defaults rather than
// duplicating them, which is how a default drifts out of sync.
pub use specmock_runtime::{DEFAULT_MAX_SESSIONS, DEFAULT_MAX_STORE_ENTRIES};
