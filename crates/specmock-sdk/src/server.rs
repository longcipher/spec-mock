//! SDK server builder and handles.

use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
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
    ws_path: String,
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
        format!("ws://{}{}", self.running.http_addr, self.ws_path)
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
    ws_path: String,
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
        format!("ws://{}{}", self.http_addr, self.ws_path)
    }

    /// gRPC address if configured.
    #[must_use]
    pub const fn grpc_addr(&self) -> Option<SocketAddr> {
        self.grpc_addr
    }

    /// Kill spawned process.
    pub fn shutdown(&mut self) -> Result<(), SdkError> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        self.child.kill()?;
        let _status = self.child.wait()?;
        Ok(())
    }
}

impl Drop for ProcessMockServer {
    fn drop(&mut self) {
        let _ignored = self.shutdown();
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

    /// Set the WebSocket path served by the AsyncAPI runtime.
    #[must_use]
    pub fn ws_path(mut self, path: impl Into<String>) -> Self {
        self.config.ws_path = path.into();
        self
    }

    /// Set maximum request body size in bytes.
    #[must_use]
    pub const fn max_body_size(mut self, size: usize) -> Self {
        self.config.max_body_size = size;
        self
    }

    /// Allow private/link-local/loopback upstream URLs in proxy mode.
    #[must_use]
    pub const fn allow_private_upstream(mut self, allow: bool) -> Self {
        self.config.allow_private_upstream = allow;
        self
    }

    /// Start in-process runtime, ideal for `#[tokio::test]`.
    pub async fn start(self) -> Result<MockServer, SdkError> {
        let ws_path = self.config.ws_path.clone();
        let running = specmock_runtime::start(self.config).await?;
        Ok(MockServer { running, ws_path })
    }

    /// Start standalone CLI process.
    ///
    /// `bin_path` should point to the `spec-mock` executable.
    pub async fn start_process_with_bin(
        mut self,
        bin_path: &Path,
    ) -> Result<ProcessMockServer, SdkError> {
        self.config.http_addr = resolve_process_bind_addr(self.config.http_addr)?;
        self.config.grpc_addr = resolve_process_bind_addr(self.config.grpc_addr)?;

        let mut command = Command::new(bin_path);
        command
            .arg("serve")
            .arg("--http-addr")
            .arg(self.config.http_addr.to_string())
            .arg("--grpc-addr")
            .arg(self.config.grpc_addr.to_string())
            .arg("--seed")
            .arg(self.config.seed.to_string())
            .arg("--ws-path")
            .arg(self.config.ws_path.as_str())
            .arg("--mode")
            .arg(match self.config.mode {
                MockMode::Mock => "mock",
                MockMode::Proxy => "proxy",
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

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
        if self.config.allow_private_upstream {
            command.arg("--allow-private-upstream");
        }
        command.arg("--max-body-size").arg(self.config.max_body_size.to_string());

        let mut child = command.spawn()?;

        if let Err(error) = wait_for_tcp_ready(&mut child, self.config.http_addr, "http").await {
            let _ignored = child.kill();
            return Err(error);
        }
        if self.config.proto_spec.is_some() &&
            let Err(error) = wait_for_tcp_ready(&mut child, self.config.grpc_addr, "grpc").await
        {
            let _ignored = child.kill();
            return Err(error);
        }

        Ok(ProcessMockServer {
            child,
            http_addr: self.config.http_addr,
            grpc_addr: self.config.proto_spec.as_ref().map(|_path| self.config.grpc_addr),
            ws_path: self.config.ws_path,
        })
    }
}

fn resolve_process_bind_addr(addr: SocketAddr) -> Result<SocketAddr, SdkError> {
    if addr.port() != 0 {
        return Ok(addr);
    }
    let listener = std::net::TcpListener::bind(SocketAddr::new(addr.ip(), 0))?;
    let bound = listener.local_addr()?;
    drop(listener);
    Ok(bound)
}

fn process_start_error(
    child: &mut Child,
    listener_name: &str,
    addr: SocketAddr,
    status: ExitStatus,
) -> SdkError {
    let stderr = child.stderr.as_mut().map_or_else(String::new, |stderr| {
        let mut out = String::new();
        let _ignored = stderr.read_to_string(&mut out);
        out
    });
    let detail = if stderr.trim().is_empty() {
        format!("process exited with status {status}")
    } else {
        format!("process exited with status {status}: {}", stderr.trim())
    };

    SdkError::Io(std::io::Error::other(format!(
        "spec-mock process failed before {listener_name} listener became ready on {addr}: {detail}"
    )))
}

async fn wait_for_tcp_ready(
    child: &mut Child,
    addr: SocketAddr,
    listener_name: &str,
) -> Result<(), SdkError> {
    let deadline = Instant::now() + PROCESS_READY_TIMEOUT;

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(process_start_error(child, listener_name, addr, status));
        }

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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test code: a failed assertion should surface as a panic"
)]
mod tests {
    use std::{
        net::SocketAddr,
        process::{Command, Stdio},
    };

    use specmock_core::MockMode;

    use super::{
        MockServer, MockServerBuilder, SdkError, process_start_error, resolve_process_bind_addr,
        wait_for_tcp_ready,
    };

    // ── builder wiring ─────────────────────────────────────────────────

    #[test]
    fn builder_defaults_match_the_runtime_defaults() {
        let builder = MockServer::builder();
        assert_eq!(builder.config.mode, MockMode::Mock);
        assert_eq!(builder.config.seed, 42);
        assert_eq!(builder.config.ws_path, "/ws");
        assert_eq!(builder.config.max_body_size, 10 * 1024 * 1024);
        assert!(builder.config.openapi_spec.is_none());
        assert!(builder.config.asyncapi_spec.is_none());
        assert!(builder.config.proto_spec.is_none());
        assert!(builder.config.upstream.is_none());
        assert!(!builder.config.allow_private_upstream);
    }

    #[test]
    fn builder_records_every_spec_path() {
        let builder =
            MockServer::builder().openapi("api.yaml").asyncapi("chat.yaml").proto("greeter.proto");
        assert_eq!(
            builder.config.openapi_spec.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("api.yaml"))
        );
        assert_eq!(
            builder.config.asyncapi_spec.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("chat.yaml"))
        );
        assert_eq!(
            builder.config.proto_spec.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("greeter.proto"))
        );
    }

    #[test]
    fn builder_records_every_scalar_setting() {
        let http_addr: SocketAddr = "127.0.0.1:1111".parse().expect("addr");
        let grpc_addr: SocketAddr = "127.0.0.1:2222".parse().expect("addr");

        let builder = MockServer::builder()
            .seed(9_999)
            .mode(MockMode::Proxy)
            .upstream("http://api.example.com")
            .http_addr(http_addr)
            .grpc_addr(grpc_addr)
            .ws_path("/socket")
            .max_body_size(64)
            .allow_private_upstream(true);

        assert_eq!(builder.config.seed, 9_999);
        assert_eq!(builder.config.mode, MockMode::Proxy);
        assert_eq!(builder.config.upstream.as_deref(), Some("http://api.example.com"));
        assert_eq!(builder.config.http_addr, http_addr);
        assert_eq!(builder.config.grpc_addr, grpc_addr);
        assert_eq!(builder.config.ws_path, "/socket");
        assert_eq!(builder.config.max_body_size, 64);
        assert!(builder.config.allow_private_upstream);
    }

    #[test]
    fn builder_is_chainable_and_ordered() {
        let builder = MockServer::builder().seed(1).seed(2).ws_path("/a").ws_path("/b");
        assert_eq!(builder.config.seed, 2, "the last call wins");
        assert_eq!(builder.config.ws_path, "/b");
    }

    // ── start-time validation ──────────────────────────────────────────

    #[tokio::test]
    async fn start_without_a_spec_is_rejected() {
        let error = MockServer::builder().start().await.expect_err("at least one spec is required");
        assert!(
            matches!(error, SdkError::Runtime(specmock_runtime::RuntimeError::Config(_))),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn start_with_a_missing_spec_is_rejected() {
        let error = MockServer::builder()
            .openapi("/definitely/not/a/real/spec.yaml")
            .start()
            .await
            .expect_err("a missing spec file must be rejected");
        assert!(error.to_string().contains("does not exist"), "got {error}");
    }

    #[tokio::test]
    async fn start_with_an_invalid_ws_path_is_rejected() {
        let error = MockServer::builder()
            .openapi(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/pets.openapi.yaml"))
            .ws_path("socket")
            .start()
            .await
            .expect_err("a ws path without a leading slash must be rejected");
        assert!(error.to_string().contains("'/'"), "got {error}");
    }

    #[tokio::test]
    async fn start_with_proxy_mode_and_no_upstream_is_rejected() {
        let error = MockServer::builder()
            .openapi(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/pets.openapi.yaml"))
            .mode(MockMode::Proxy)
            .start()
            .await
            .expect_err("proxy mode needs an upstream");
        assert!(error.to_string().contains("proxy mode requires upstream"), "got {error}");
    }

    // ── embedded mode ──────────────────────────────────────────────────

    #[tokio::test]
    async fn embedded_server_exposes_consistent_urls() {
        let server = match MockServer::builder()
            .openapi(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/pets.openapi.yaml"))
            .ws_path("/socket")
            .start()
            .await
        {
            Ok(value) => value,
            Err(SdkError::Runtime(specmock_runtime::RuntimeError::Io(error)))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("unexpected start failure: {error}"),
        };

        assert_eq!(server.http_base_url(), format!("http://{}", server.running.http_addr));
        assert_eq!(server.ws_url(), format!("ws://{}/socket", server.running.http_addr));
        assert!(server.grpc_addr().is_none(), "no proto spec means no gRPC address");

        server.shutdown().await;
    }

    // ── resolve_process_bind_addr ──────────────────────────────────────

    #[test]
    fn a_fixed_port_is_passed_through_unchanged() {
        let addr: SocketAddr = "127.0.0.1:34567".parse().expect("addr");
        assert_eq!(resolve_process_bind_addr(addr).expect("fixed port"), addr);
    }

    #[test]
    fn an_ephemeral_port_is_resolved_to_a_concrete_one() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let resolved = resolve_process_bind_addr(addr).expect("ephemeral port");

        assert_ne!(resolved.port(), 0, "the child process needs a concrete port to bind");
        assert_eq!(resolved.ip(), addr.ip(), "the bind address family is preserved");
    }

    #[test]
    fn ephemeral_resolution_yields_a_free_port_each_time() {
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let mut ports = std::collections::BTreeSet::new();
        for _ in 0..8 {
            let resolved = resolve_process_bind_addr(addr).expect("ephemeral port");
            ports.insert(resolved.port());
        }
        assert!(
            ports.len() > 1,
            "repeated resolution should keep finding free ports, got {ports:?}"
        );
    }

    // ── process_start_error ────────────────────────────────────────────

    #[test]
    fn process_start_error_without_stderr_reports_the_status() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 3")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let status = child.wait().expect("wait");
        let error =
            process_start_error(&mut child, "http", "127.0.0.1:1".parse().expect("addr"), status);

        let message = error.to_string();
        assert!(message.contains("http listener"), "got {message}");
        assert!(message.contains("127.0.0.1:1"), "got {message}");
        assert!(message.contains("exit status: 3"), "got {message}");
        assert!(!message.contains("process exited with status 3:"), "got {message}");
    }

    #[test]
    fn process_start_error_includes_stderr_when_present() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("echo 'spec file is unreadable' >&2; exit 2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let status = child.wait().expect("wait");
        let error =
            process_start_error(&mut child, "grpc", "127.0.0.1:2".parse().expect("addr"), status);

        let message = error.to_string();
        assert!(message.contains("grpc listener"), "got {message}");
        assert!(message.contains("spec file is unreadable"), "got {message}");
        assert!(message.contains("exit status: 2"), "got {message}");
    }

    #[test]
    fn process_start_error_without_a_piped_stderr_still_reports() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let status = child.wait().expect("wait");
        let error =
            process_start_error(&mut child, "http", "127.0.0.1:1".parse().expect("addr"), status);
        assert!(error.to_string().contains("exit status: 1"), "got {error}");
    }

    // ── wait_for_tcp_ready ─────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_ready_detects_an_early_exit() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 7")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let error = wait_for_tcp_ready(&mut child, "127.0.0.1:1".parse().expect("addr"), "http")
            .await
            .expect_err("the process exited before the listener was ready");

        let message = error.to_string();
        assert!(message.contains("before http listener became ready"), "got {message}");
        assert!(message.contains("exit status: 7"), "got {message}");
    }

    #[tokio::test]
    async fn wait_for_ready_succeeds_once_the_port_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Keep accepting so repeated probes do not fill the backlog.
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        // A process that stays alive for the duration of the check.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let result = wait_for_tcp_ready(&mut child, addr, "http").await;
        let _kill = child.kill();
        let _wait = child.wait();

        assert!(result.is_ok(), "an open listener must satisfy the readiness probe");
    }

    #[tokio::test]
    async fn wait_for_ready_times_out_when_nothing_listens() {
        // Reserve a port, learn its number, then release it so nothing answers.
        let addr = resolve_process_bind_addr("127.0.0.1:0".parse().expect("addr"))
            .expect("reserve a port");

        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            wait_for_tcp_ready(&mut child, addr, "grpc"),
        )
        .await
        .expect("wait_for_tcp_ready must not hang");

        let _kill = child.kill();
        let _wait = child.wait();

        let error = result.expect_err("nothing is listening on the reserved port");
        assert!(
            matches!(error, SdkError::Io(ref inner) if inner.kind() == std::io::ErrorKind::TimedOut),
            "got {error:?}"
        );
        assert!(error.to_string().contains("timed out waiting for grpc listener"), "got {error}");
    }

    // ── process mode failure paths ─────────────────────────────────────

    #[tokio::test]
    async fn process_mode_with_a_missing_binary_is_an_io_error() {
        let error = MockServer::builder()
            .openapi(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/pets.openapi.yaml"))
            .start_process_with_bin(std::path::Path::new("/definitely/not/a/binary"))
            .await
            .expect_err("spawning a missing binary must fail");
        assert!(matches!(error, SdkError::Io(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn process_mode_with_a_non_bindable_binary_is_reported() {
        // `/bin/echo` exits immediately without opening a listener.
        let error = MockServer::builder()
            .openapi(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/specs/pets.openapi.yaml"))
            .start_process_with_bin(std::path::Path::new("/bin/echo"))
            .await
            .expect_err("a binary that never listens must be reported");
        assert!(matches!(error, SdkError::Io(_)), "got {error:?}");
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.
    #[rustfmt::skip]
    proptest! {
        /// A builder built from arbitrary configuration never panics, and every
        /// recorded value survives the chain intact.
        #[test]
        fn builder_chain_never_panics(
            seed in any::<u64>(),
            max_body_size in 1usize..1_000_000,
            ws_path in "/[a-z]{0,16}",
        ) {
            let builder = MockServerBuilder::default()
                .seed(seed)
                .max_body_size(max_body_size)
                .ws_path(ws_path.clone());
            assert_eq!(builder.config.seed, seed);
            assert_eq!(builder.config.max_body_size, max_body_size);
            assert_eq!(builder.config.ws_path, ws_path);
        }

        /// Resolving an ephemeral bind address is total for any valid address.
        #[test]
        fn bind_resolution_never_panics(ip in prop::sample::select(vec!["127.0.0.1", "[::1]"])) {
            let addr: SocketAddr = format!("{ip}:0").parse().expect("addr");
            if let Ok(resolved) = resolve_process_bind_addr(addr) {
                assert_ne!(resolved.port(), 0);
            }
        }
    }
}
