//! Runtime servers for HTTP(OpenAPI), WS(AsyncAPI), and gRPC(Protobuf).

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use specmock_core::MockMode;
use tokio::{sync::oneshot, task::JoinHandle};

pub mod grpc;
pub mod http;
pub mod ws;

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            openapi_spec: None,
            asyncapi_spec: None,
            proto_spec: None,
            mode: MockMode::Mock,
            upstream: None,
            seed: 42,
            http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            grpc_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            ws_path: "/ws".to_owned(),
            max_body_size: 10 * 1024 * 1024,
        }
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
}

/// Start protocol runtimes.
pub async fn start(config: ServerConfig) -> Result<RunningServer, RuntimeError> {
    if config.openapi_spec.is_none() &&
        config.asyncapi_spec.is_none() &&
        config.proto_spec.is_none()
    {
        return Err(RuntimeError::Config(
            "at least one spec must be provided: openapi_spec, asyncapi_spec, or proto_spec"
                .to_owned(),
        ));
    }

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
