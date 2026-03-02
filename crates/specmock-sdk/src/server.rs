//! SDK server builder and handles.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use specmock_core::MockMode;
use specmock_runtime::{RunningServer, ServerConfig};
use thiserror::Error;
use tokio::time::{Duration, Instant};

const PROCESS_READY_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_READY_POLL_INTERVAL: Duration = Duration::from_millis(40);

/// SDK error.
#[derive(Debug, Error)]
pub enum SdkError {
    /// Runtime startup failed.
    #[error("runtime start failed: {0}")]
    Runtime(#[from] specmock_runtime::RuntimeError),
    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// In-process server handle.
#[derive(Debug)]
pub struct MockServer {
    running: RunningServer,
}

impl MockServer {
    /// Create a server builder.
    #[must_use]
    pub fn builder() -> MockServerBuilder {
        MockServerBuilder::default()
    }

    /// HTTP base URL.
    #[must_use]
    pub fn http_base_url(&self) -> String {
        format!("http://{}", self.running.http_addr)
    }

    /// WebSocket URL.
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.running.http_addr)
    }

    /// Bound gRPC address.
    #[must_use]
    pub const fn grpc_addr(&self) -> Option<SocketAddr> {
        self.running.grpc_addr
    }

    /// Explicit shutdown.
    pub async fn shutdown(self) {
        self.running.shutdown().await;
    }
}

/// Process-mode server handle.
#[derive(Debug)]
pub struct ProcessMockServer {
    child: Child,
    http_addr: SocketAddr,
    grpc_addr: Option<SocketAddr>,
}

impl ProcessMockServer {
    /// HTTP base URL.
    #[must_use]
    pub fn http_base_url(&self) -> String {
        format!("http://{}", self.http_addr)
    }

    /// WebSocket URL.
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.http_addr)
    }

    /// gRPC address if configured.
    #[must_use]
    pub const fn grpc_addr(&self) -> Option<SocketAddr> {
        self.grpc_addr
    }

    /// Kill spawned process.
    pub fn shutdown(&mut self) -> Result<(), SdkError> {
        self.child.kill()?;
        Ok(())
    }
}

impl Drop for ProcessMockServer {
    fn drop(&mut self) {
        let _ignored = self.child.kill();
    }
}

/// SDK builder.
#[derive(Debug, Clone, Default)]
pub struct MockServerBuilder {
    config: ServerConfig,
}

impl MockServerBuilder {
    /// Set OpenAPI spec path.
    #[must_use]
    pub fn openapi(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.openapi_spec = Some(path.into());
        self
    }

    /// Set AsyncAPI spec path.
    #[must_use]
    pub fn asyncapi(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.asyncapi_spec = Some(path.into());
        self
    }

    /// Set protobuf root file.
    #[must_use]
    pub fn proto(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.proto_spec = Some(path.into());
        self
    }

    /// Set deterministic seed.
    #[must_use]
    pub const fn seed(mut self, seed: u64) -> Self {
        self.config.seed = seed;
        self
    }

    /// Set runtime mode.
    #[must_use]
    pub const fn mode(mut self, mode: MockMode) -> Self {
        self.config.mode = mode;
        self
    }

    /// Set proxy upstream URL.
    #[must_use]
    pub fn upstream(mut self, upstream: impl Into<String>) -> Self {
        self.config.upstream = Some(upstream.into());
        self
    }

    /// Bind HTTP listen address.
    #[must_use]
    pub const fn http_addr(mut self, addr: SocketAddr) -> Self {
        self.config.http_addr = addr;
        self
    }

    /// Bind gRPC listen address.
    #[must_use]
    pub const fn grpc_addr(mut self, addr: SocketAddr) -> Self {
        self.config.grpc_addr = addr;
        self
    }

    /// Set maximum request body size in bytes.
    #[must_use]
    pub const fn max_body_size(mut self, size: usize) -> Self {
        self.config.max_body_size = size;
        self
    }

    /// Start in-process runtime, ideal for `#[tokio::test]`.
    pub async fn start(self) -> Result<MockServer, SdkError> {
        let running = specmock_runtime::start(self.config).await?;
        Ok(MockServer { running })
    }

    /// Start standalone CLI process.
    ///
    /// `bin_path` should point to the `spec-mock` executable.
    pub async fn start_process_with_bin(
        mut self,
        bin_path: &Path,
    ) -> Result<ProcessMockServer, SdkError> {
        if self.config.http_addr.port() == 0 {
            self.config.http_addr = SocketAddr::from(([127, 0, 0, 1], 4010));
        }
        if self.config.grpc_addr.port() == 0 {
            self.config.grpc_addr = SocketAddr::from(([127, 0, 0, 1], 5010));
        }

        let mut command = Command::new(bin_path);
        command
            .arg("serve")
            .arg("--http-addr")
            .arg(self.config.http_addr.to_string())
            .arg("--grpc-addr")
            .arg(self.config.grpc_addr.to_string())
            .arg("--seed")
            .arg(self.config.seed.to_string())
            .arg("--mode")
            .arg(match self.config.mode {
                MockMode::Mock => "mock",
                MockMode::Proxy => "proxy",
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(path) = &self.config.openapi_spec {
            command.arg("--openapi").arg(path);
        }
        if let Some(path) = &self.config.asyncapi_spec {
            command.arg("--asyncapi").arg(path);
        }
        if let Some(path) = &self.config.proto_spec {
            command.arg("--proto").arg(path);
        }
        if let Some(upstream) = &self.config.upstream {
            command.arg("--upstream").arg(upstream);
        }
        command.arg("--max-body-size").arg(self.config.max_body_size.to_string());

        let mut child = command.spawn()?;

        if let Err(error) = wait_for_tcp_ready(self.config.http_addr, "http").await {
            let _ignored = child.kill();
            return Err(error);
        }
        if self.config.proto_spec.is_some() &&
            let Err(error) = wait_for_tcp_ready(self.config.grpc_addr, "grpc").await
        {
            let _ignored = child.kill();
            return Err(error);
        }

        Ok(ProcessMockServer {
            child,
            http_addr: self.config.http_addr,
            grpc_addr: self.config.proto_spec.as_ref().map(|_path| self.config.grpc_addr),
        })
    }
}

async fn wait_for_tcp_ready(addr: SocketAddr, listener_name: &str) -> Result<(), SdkError> {
    let deadline = Instant::now() + PROCESS_READY_TIMEOUT;

    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(SdkError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for {listener_name} listener on {addr}: {}",
                            error
                        ),
                    )));
                }
            }
        }

        tokio::time::sleep(PROCESS_READY_POLL_INTERVAL).await;
    }
}
