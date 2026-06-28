//! Runtime servers for HTTP(OpenAPI), WS(AsyncAPI), and gRPC(Protobuf).

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use specmock_core::MockMode;
use tokio::{sync::oneshot, task::JoinHandle};

pub mod grpc;
pub mod http;
pub mod ws;

/// Default maximum request body size: 10 MiB.
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Default HTTP listen address.
const DEFAULT_HTTP_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

/// Default gRPC listen address.
const DEFAULT_GRPC_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 0);

/// Default WebSocket path.
const DEFAULT_WS_PATH: &str = "/ws";

/// Default deterministic seed.
const DEFAULT_SEED: u64 = 42;

const FOLD_HASH_MULTIPLIER: u64 = 131;

pub fn deterministic_hash(seed: u64, input: &str) -> u64 {
    seed ^ input.bytes().fold(0_u64, |acc, byte| {
        acc.wrapping_mul(FOLD_HASH_MULTIPLIER).wrapping_add(u64::from(byte))
    })
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// OpenAPI spec file.
    pub openapi_spec: Option<PathBuf>,
    /// AsyncAPI spec file.
    pub asyncapi_spec: Option<PathBuf>,
    /// Protobuf root file.
    pub proto_spec: Option<PathBuf>,
    /// Runtime mode.
    pub mode: MockMode,
    /// Upstream base URL for proxy mode.
    pub upstream: Option<String>,
    /// Deterministic seed used by faker.
    pub seed: u64,
    /// HTTP/WS listen address.
    pub http_addr: SocketAddr,
    /// gRPC listen address.
    pub grpc_addr: SocketAddr,
    /// WebSocket path.
    pub ws_path: String,
    /// Maximum request body size in bytes (default 10 MiB).
    pub max_body_size: usize,
    /// Allow private/link-local/loopback upstream URLs in proxy mode.
    pub allow_private_upstream: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            openapi_spec: None,
            asyncapi_spec: None,
            proto_spec: None,
            mode: MockMode::Mock,
            upstream: None,
            seed: DEFAULT_SEED,
            http_addr: SocketAddr::from(DEFAULT_HTTP_ADDR),
            grpc_addr: SocketAddr::from(DEFAULT_GRPC_ADDR),
            ws_path: DEFAULT_WS_PATH.to_owned(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            allow_private_upstream: false,
        }
    }
}

impl ServerConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        // Check that at least one spec is provided
        if self.openapi_spec.is_none() && self.asyncapi_spec.is_none() && self.proto_spec.is_none()
        {
            return Err(RuntimeError::Config(
                "at least one spec must be provided: openapi_spec, asyncapi_spec, or proto_spec"
                    .to_owned(),
            ));
        }

        // Check proxy mode configuration
        if self.mode == MockMode::Proxy && self.upstream.is_none() {
            return Err(RuntimeError::Config(
                "proxy mode requires upstream base URL (--upstream)".to_owned(),
            ));
        }

        // Reject non-HTTP upstream schemes to prevent SSRF
        if let Some(ref upstream) = self.upstream {
            match url::Url::parse(upstream) {
                Ok(parsed) => {
                    if parsed.scheme() != "http" && parsed.scheme() != "https" {
                        return Err(RuntimeError::Config(format!(
                            "upstream URL must use http or https scheme, got: {}",
                            parsed.scheme()
                        )));
                    }

                    // Reject private/link-local/loopback upstream unless explicitly allowed
                    if self.mode == MockMode::Proxy &&
                        !self.allow_private_upstream &&
                        let Some(host) = parsed.host_str() &&
                        let Ok(ip) = host.parse::<std::net::IpAddr>() &&
                        is_private_ip(ip)
                    {
                        return Err(RuntimeError::Config(
                            "upstream URL must not point to a private, loopback, or link-local address (use --allow-private-upstream to override)".to_owned(),
                        ));
                    }
                }
                Err(e) => {
                    return Err(RuntimeError::Config(format!("invalid upstream URL: {e}")));
                }
            }
        }

        // Check that HTTP and gRPC addresses are not the same
        if self.http_addr == self.grpc_addr &&
            self.http_addr.port() != 0 &&
            self.grpc_addr.port() != 0
        {
            return Err(RuntimeError::Config(
                "HTTP and gRPC addresses must be different".to_owned(),
            ));
        }

        // Check that WebSocket path starts with /
        if !self.ws_path.starts_with('/') {
            return Err(RuntimeError::Config("WebSocket path must start with '/'".to_owned()));
        }

        // Check that max_body_size is reasonable
        if self.max_body_size == 0 {
            return Err(RuntimeError::Config("max_body_size must be greater than 0".to_owned()));
        }

        // Check that spec files exist
        if let Some(ref path) = self.openapi_spec &&
            !path.exists()
        {
            return Err(RuntimeError::Config(format!(
                "OpenAPI spec file does not exist: {}",
                path.display()
            )));
        }

        if let Some(ref path) = self.asyncapi_spec &&
            !path.exists()
        {
            return Err(RuntimeError::Config(format!(
                "AsyncAPI spec file does not exist: {}",
                path.display()
            )));
        }

        if let Some(ref path) = self.proto_spec &&
            !path.exists()
        {
            return Err(RuntimeError::Config(format!(
                "Protobuf spec file does not exist: {}",
                path.display()
            )));
        }

        Ok(())
    }
}

/// Check if an IP address is loopback, private (RFC 1918), or link-local.
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Runtime handle.
#[derive(Debug)]
pub struct RunningServer {
    /// Bound HTTP address.
    pub http_addr: SocketAddr,
    /// Bound gRPC address, if proto runtime is active.
    pub grpc_addr: Option<SocketAddr>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningServer {
    /// Shut down runtime tasks gracefully.
    pub async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ignored = shutdown_tx.send(());
        }
        for task in self.tasks.drain(..) {
            let _ignored = task.await;
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ignored = shutdown_tx.send(());
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Runtime error.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Invalid configuration.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Serialization / parsing error.
    #[error("parse error: {0}")]
    Parse(String),
    /// Requested response not found.
    #[error("not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_file_upstream_scheme() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            mode: MockMode::Proxy,
            upstream: Some("file:///etc/passwd".into()),
            ..Default::default()
        };
        let err = config.validate().expect_err("should reject file:// scheme");
        let msg = format!("{err}");
        assert!(
            msg.contains("http") || msg.contains("scheme"),
            "error should mention scheme/http, got: {msg}"
        );
    }

    #[test]
    fn reject_private_upstream_by_default() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            mode: MockMode::Proxy,
            upstream: Some("http://127.0.0.1:9999".into()),
            allow_private_upstream: false,
            ..Default::default()
        };
        let err = config.validate().expect_err("should reject loopback upstream");
        let msg = format!("{err}");
        assert!(
            msg.contains("private") || msg.contains("loopback"),
            "error should mention private/loopback, got: {msg}"
        );
    }

    #[test]
    fn reject_link_local_upstream_by_default() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            mode: MockMode::Proxy,
            upstream: Some("http://169.254.169.254/latest/meta-data".into()),
            allow_private_upstream: false,
            ..Default::default()
        };
        let err = config.validate().expect_err("should reject link-local upstream");
        let msg = format!("{err}");
        assert!(
            msg.contains("private") || msg.contains("loopback") || msg.contains("link-local"),
            "error should mention private/loopback/link-local, got: {msg}"
        );
    }

    #[test]
    fn allow_private_upstream_when_flag_set() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            mode: MockMode::Proxy,
            upstream: Some("http://127.0.0.1:9999".into()),
            allow_private_upstream: true,
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "should allow private upstream when flag is set");
    }
}

/// Start protocol runtimes.
pub async fn start(config: ServerConfig) -> Result<RunningServer, RuntimeError> {
    config.validate()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shared_shutdown = Arc::new(tokio::sync::Notify::new());

    let http_runtime = http::HttpRuntime::from_config(&config).await?;
    let (http_addr, http_task) =
        http::spawn_http_server(http_runtime, config.http_addr, Arc::clone(&shared_shutdown))
            .await?;

    let mut tasks = vec![http_task];
    let mut grpc_addr = None;

    if config.proto_spec.is_some() {
        let grpc_runtime = grpc::GrpcRuntime::from_config(&config).await?;
        let (bound_grpc_addr, grpc_task) =
            grpc::spawn_grpc_server(grpc_runtime, config.grpc_addr, Arc::clone(&shared_shutdown))
                .await?;
        grpc_addr = Some(bound_grpc_addr);
        tasks.push(grpc_task);
    }

    // Relay oneshot shutdown to notify-based shutdown for all tasks.
    let relay_notify = Arc::clone(&shared_shutdown);
    tasks.push(tokio::spawn(async move {
        let _ignored = shutdown_rx.await;
        relay_notify.notify_waiters();
    }));

    Ok(RunningServer { http_addr, grpc_addr, shutdown_tx: Some(shutdown_tx), tasks })
}
