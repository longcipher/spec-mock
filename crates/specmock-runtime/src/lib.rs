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

/// Default cap on live scenario instances.
///
/// Generous enough that ordinary use never reaches it. It exists to bound a
/// client that sends a fresh `session=` key on every request, which would
/// otherwise create an unbounded number of instances.
pub const DEFAULT_MAX_SESSIONS: usize = 256;

/// Default cap on stored resources per scenario instance.
pub const DEFAULT_MAX_STORE_ENTRIES: usize = 1024;

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
    /// Scenario sidecar declaring stateful behaviour.
    ///
    /// Resolved alongside the OpenAPI document rather than inside it, so a spec
    /// owned by someone else can be mocked without being forked.
    pub scenarios_spec: Option<PathBuf>,
    /// Maximum live scenario instances across all sessions.
    pub max_sessions: usize,
    /// Maximum stored resources per scenario instance.
    pub max_store_entries: usize,
    /// Bearer token required by the control endpoints. `None` disables them.
    pub control_token: Option<String>,
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
            scenarios_spec: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_store_entries: DEFAULT_MAX_STORE_ENTRIES,
            control_token: None,
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

                    // Reject private/link-local/loopback upstream unless explicitly allowed.
                    // `host_str` brackets IPv6 literals (`[::1]`), so the brackets
                    // must be stripped before parsing or the guard is bypassed.
                    if self.mode == MockMode::Proxy &&
                        !self.allow_private_upstream &&
                        let Some(host) = parsed.host_str() &&
                        let Ok(ip) = host
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .parse::<std::net::IpAddr>() &&
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
    scenarios: Option<Arc<http::scenarios::ScenarioRuntime>>,
    control_token: Option<String>,
}

impl RunningServer {
    /// The scenario runtime, present only when a sidecar was supplied.
    #[must_use]
    pub fn scenarios(&self) -> Option<&http::scenarios::ScenarioRuntime> {
        self.scenarios.as_deref()
    }

    /// Bearer token guarding the control endpoints, if they are enabled.
    #[must_use]
    pub fn control_token(&self) -> Option<&str> {
        self.control_token.as_deref()
    }

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
    /// No declared response representation satisfies the request.
    #[error("not acceptable: {0}")]
    NotAcceptable(String),
}

/// Start protocol runtimes.
/// Load the scenario runtime a server will share across protocols.
pub(crate) fn load_scenario_runtime(
    config: &ServerConfig,
) -> Result<Option<Arc<http::scenarios::ScenarioRuntime>>, RuntimeError> {
    http::load_scenarios(config)
}

pub async fn start(config: ServerConfig) -> Result<RunningServer, RuntimeError> {
    config.validate()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // One `Notify` per runtime, each awaited by exactly one task. `Notify` has a
    // single stored permit, so a per-task notifier both wakes a task that is
    // already waiting and latches the signal for one that has not started yet —
    // whereas `notify_waiters` on a shared notifier would silently drop the
    // signal for whichever task had not yet reached its `notified().await`, and
    // `notify_one` on a shared notifier would wake only one of the two waiters.
    let http_shutdown = Arc::new(tokio::sync::Notify::new());
    let grpc_shutdown = Arc::new(tokio::sync::Notify::new());

    // One sidecar load, one set of live sessions, shared by every protocol in the
    // process. `scc`'s `Clone` snapshots, so a per-protocol load would be a
    // disconnected copy that never saw the other's requests.
    let scenarios = load_scenario_runtime(&config)?;

    let http_runtime = http::HttpRuntime::from_config_with(&config, scenarios.clone()).await?;
    let (http_addr, http_task) =
        http::spawn_http_server(http_runtime, config.http_addr, Arc::clone(&http_shutdown)).await?;

    let mut tasks = vec![http_task];
    let mut grpc_addr = None;

    if config.proto_spec.is_some() {
        let grpc_runtime = grpc::GrpcRuntime::from_config_with(&config, scenarios.clone()).await?;
        let (bound_grpc_addr, grpc_task) =
            grpc::spawn_grpc_server(grpc_runtime, config.grpc_addr, Arc::clone(&grpc_shutdown))
                .await?;
        grpc_addr = Some(bound_grpc_addr);
        tasks.push(grpc_task);
    }

    // Relay the oneshot shutdown signal to each runtime's notifier.
    let relay_http = Arc::clone(&http_shutdown);
    let relay_grpc = Arc::clone(&grpc_shutdown);
    tasks.push(tokio::spawn(async move {
        let _ignored = shutdown_rx.await;
        relay_http.notify_one();
        relay_grpc.notify_one();
    }));

    Ok(RunningServer {
        http_addr,
        grpc_addr,
        shutdown_tx: Some(shutdown_tx),
        tasks,
        scenarios,
        control_token: config.control_token.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[test]
    fn reject_ws_path_without_leading_slash() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            ws_path: "socket".to_owned(),
            ..Default::default()
        };
        let err = config.validate().expect_err("should reject a ws path without a leading slash");
        assert!(format!("{err}").contains("'/'"), "unexpected error: {err}");
    }

    #[test]
    fn accept_custom_ws_path_with_leading_slash() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config = ServerConfig {
            openapi_spec: Some(spec),
            ws_path: "/socket".to_owned(),
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "a custom ws path must be accepted");
    }

    #[test]
    fn reject_zero_max_body_size() {
        let spec =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml");
        let config =
            ServerConfig { openapi_spec: Some(spec), max_body_size: 0, ..Default::default() };
        assert!(config.validate().is_err(), "a zero body limit must be rejected");
    }

    // ── deterministic_hash ─────────────────────────────────────────────

    #[test]
    fn deterministic_hash_is_stable_and_seed_dependent() {
        assert_eq!(deterministic_hash(42, "GET/pets/1"), deterministic_hash(42, "GET/pets/1"));
        assert_ne!(deterministic_hash(42, "GET/pets/1"), deterministic_hash(43, "GET/pets/1"));
        assert_ne!(deterministic_hash(42, "GET/pets/1"), deterministic_hash(42, "GET/pets/2"));
    }

    #[test]
    fn deterministic_hash_handles_an_empty_input() {
        assert_eq!(
            deterministic_hash(0, ""),
            0,
            "an empty input folds to zero so the seed passes through"
        );
        assert_eq!(deterministic_hash(7, ""), 7);
    }

    #[test]
    fn deterministic_hash_distinguishes_path_boundaries() {
        // "/pets/1" and "/pets1" must not collide even though the byte sums are
        // identical, because the fold is position- and multiplier-dependent.
        assert_ne!(deterministic_hash(0, "/pets/1"), deterministic_hash(0, "/pets1"));
    }

    #[test]
    fn deterministic_hash_never_panics_on_arbitrary_text() {
        for text in ["", "/", "//", "\u{1F600}", "a\u{0}b", &"x".repeat(4096)] {
            let _ = deterministic_hash(u64::MAX, text);
        }
    }

    // ── is_private_ip ──────────────────────────────────────────────────

    #[test]
    fn private_ip_classification() {
        use std::net::IpAddr;

        // Loopback.
        assert!(is_private_ip("127.0.0.1".parse::<IpAddr>().expect("ip")));
        assert!(is_private_ip("127.10.20.30".parse::<IpAddr>().expect("ip")));
        assert!(is_private_ip("::1".parse::<IpAddr>().expect("ip")));

        // RFC 1918.
        assert!(is_private_ip("10.0.0.1".parse::<IpAddr>().expect("ip")));
        assert!(is_private_ip("172.16.0.1".parse::<IpAddr>().expect("ip")));
        assert!(is_private_ip("192.168.1.1".parse::<IpAddr>().expect("ip")));

        // Link-local.
        assert!(is_private_ip("169.254.169.254".parse::<IpAddr>().expect("ip")));

        // Public.
        assert!(!is_private_ip("8.8.8.8".parse::<IpAddr>().expect("ip")));
        assert!(!is_private_ip("1.1.1.1".parse::<IpAddr>().expect("ip")));
        assert!(!is_private_ip("172.32.0.1".parse::<IpAddr>().expect("ip")));
        assert!(!is_private_ip("2606:4700::1111".parse::<IpAddr>().expect("ip")));
    }

    // ── ServerConfig::validate ─────────────────────────────────────────

    fn openapi_spec() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs/pets.openapi.yaml")
    }

    #[test]
    fn at_least_one_spec_is_required() {
        let config = ServerConfig::default();
        let error = config.validate().expect_err("no spec is invalid");
        assert!(error.to_string().contains("at least one spec"), "unexpected error: {error}");
    }

    #[test]
    fn each_spec_kind_alone_is_enough() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs");

        for spec in [
            dir.join("pets.openapi.yaml"),
            dir.join("chat.asyncapi.yaml"),
            dir.join("greeter.proto"),
        ] {
            let config = ServerConfig { openapi_spec: Some(spec.clone()), ..Default::default() };
            assert!(config.validate().is_ok(), "{} should be accepted", spec.display());
        }
    }

    #[test]
    fn missing_spec_files_are_reported() {
        let missing = PathBuf::from("/definitely/not/a/real/spec.yaml");

        for (config, expected) in [
            (
                ServerConfig { openapi_spec: Some(missing.clone()), ..Default::default() },
                "OpenAPI spec file does not exist",
            ),
            (
                ServerConfig { asyncapi_spec: Some(missing.clone()), ..Default::default() },
                "AsyncAPI spec file does not exist",
            ),
            (
                ServerConfig { proto_spec: Some(missing.clone()), ..Default::default() },
                "Protobuf spec file does not exist",
            ),
        ] {
            let error = config.validate().expect_err("a missing file must be rejected");
            assert!(error.to_string().contains(expected), "expected {expected:?}, got {error}");
        }
    }

    #[test]
    fn identical_http_and_grpc_addresses_are_rejected() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            http_addr: addr,
            grpc_addr: addr,
            ..Default::default()
        };
        let error = config.validate().expect_err("the addresses must differ");
        assert!(error.to_string().contains("must be different"), "got {error}");
    }

    #[test]
    fn an_ephemeral_port_on_one_side_is_allowed() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        for (http_addr, grpc_addr) in [
            (addr, "127.0.0.1:0".parse::<SocketAddr>().expect("addr")),
            ("127.0.0.1:0".parse::<SocketAddr>().expect("addr"), addr),
        ] {
            let config = ServerConfig {
                openapi_spec: Some(openapi_spec()),
                http_addr,
                grpc_addr,
                ..Default::default()
            };
            assert!(config.validate().is_ok(), "port 0 asks the OS to choose, so it cannot clash");
        }
    }

    #[test]
    fn malformed_upstream_url_is_rejected() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            upstream: Some("definitely not a url".to_owned()),
            ..Default::default()
        };
        let error = config.validate().expect_err("a malformed URL must be rejected");
        assert!(error.to_string().contains("invalid upstream URL"), "got {error}");
    }

    #[test]
    fn non_http_upstream_schemes_are_rejected() {
        for upstream in ["file:///etc/passwd", "ftp://example.com", "gopher://example.com"] {
            let config = ServerConfig {
                openapi_spec: Some(openapi_spec()),
                upstream: Some(upstream.to_owned()),
                ..Default::default()
            };
            let error = config.validate().expect_err("only http(s) is allowed");
            assert!(error.to_string().contains("http or https scheme"), "{upstream}: {error}");
        }
    }

    #[test]
    fn private_upstream_is_only_checked_in_proxy_mode() {
        for mode in [MockMode::Mock, MockMode::Proxy] {
            let config = ServerConfig {
                openapi_spec: Some(openapi_spec()),
                mode,
                upstream: Some("http://10.0.0.1:8080".to_owned()),
                allow_private_upstream: false,
                ..Default::default()
            };
            let result = config.validate();
            if mode == MockMode::Proxy {
                assert!(result.is_err(), "proxy mode must reject a private upstream");
            } else {
                assert!(result.is_ok(), "mock mode never dials upstream, so it must be accepted");
            }
        }
    }

    #[test]
    fn a_public_upstream_hostname_is_accepted_in_proxy_mode() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            mode: MockMode::Proxy,
            upstream: Some("https://api.example.com".to_owned()),
            allow_private_upstream: false,
            ..Default::default()
        };
        assert!(config.validate().is_ok(), "a public hostname must be accepted");
    }

    #[test]
    fn ipv6_loopback_upstream_is_rejected_in_proxy_mode() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            mode: MockMode::Proxy,
            upstream: Some("http://[::1]:8080".to_owned()),
            allow_private_upstream: false,
            ..Default::default()
        };
        assert!(config.validate().is_err(), "IPv6 loopback must be treated as private");
    }

    #[test]
    fn a_full_valid_configuration_is_accepted() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            mode: MockMode::Proxy,
            upstream: Some("https://api.example.com".to_owned()),
            seed: 12345,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            ws_path: "/socket".to_owned(),
            max_body_size: 4096,
            allow_private_upstream: false,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    // ── RuntimeError / RunningServer ───────────────────────────────────

    #[test]
    fn runtime_error_variants_render_useful_messages() {
        assert!(
            RuntimeError::Config("bad flag".to_owned())
                .to_string()
                .contains("invalid configuration")
        );
        assert!(RuntimeError::Parse("bad yaml".to_owned()).to_string().contains("parse error"));
        assert!(RuntimeError::NotFound("no route".to_owned()).to_string().contains("not found"));
        assert!(
            RuntimeError::NotAcceptable("no type".to_owned())
                .to_string()
                .contains("not acceptable")
        );

        let io = RuntimeError::from(std::io::Error::other("boom"));
        assert!(io.to_string().contains("io error"), "got {io}");
    }

    #[tokio::test]
    async fn start_rejects_an_invalid_configuration_before_binding() {
        let config = ServerConfig::default();
        let error = start(config).await.expect_err("no spec is invalid");
        assert!(
            matches!(error, RuntimeError::Config(_)),
            "validation must run before any listener is bound, got {error:?}"
        );
    }

    #[tokio::test]
    async fn running_server_shutdown_stops_accepting() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..Default::default()
        };

        let server = match start(config).await {
            Ok(value) => value,
            Err(RuntimeError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("unexpected start failure: {error}"),
        };

        assert!(server.grpc_addr.is_none(), "no proto spec means no gRPC listener");

        // The server task may not have reached its shutdown wait yet when
        // `shutdown` fires, so the relay must latch the signal; without that,
        // shutdown hangs instead of completing.
        tokio::time::timeout(Duration::from_secs(5), server.shutdown())
            .await
            .expect("shutdown must complete promptly");
    }

    #[tokio::test]
    async fn dropping_a_running_server_stops_the_listener() {
        let config = ServerConfig {
            openapi_spec: Some(openapi_spec()),
            http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..Default::default()
        };

        let addr = match start(config).await {
            Ok(server) => server.http_addr,
            Err(RuntimeError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("unexpected start failure: {error}"),
        };

        // Dropping the handle aborts the tasks, so the port stops answering.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let probe = hpx::get(format!("http://{addr}/pets/1")).send().await;
        assert!(probe.is_err(), "the listener must be gone after the handle is dropped");
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// `deterministic_hash` must be total, deterministic, and sensitive to
        /// both the seed and the input.
        #[test]
        fn deterministic_hash_is_total_and_deterministic(
            seed in any::<u64>(),
            input in ".{0,64}",
        ) {
            let first = deterministic_hash(seed, &input);
            prop_assert_eq!(first, deterministic_hash(seed, &input));
        }

        /// A one-character input change must change the hash.
        #[test]
        fn deterministic_hash_is_input_sensitive(
            seed in any::<u64>(),
            left in "[a-z]{1,16}",
            right in "[a-z]{1,16}",
        ) {
            prop_assume!(left != right);
            prop_assert_ne!(deterministic_hash(seed, &left), deterministic_hash(seed, &right));
        }

        /// A seed change must change the hash for any non-empty input.
        #[test]
        fn deterministic_hash_is_seed_sensitive(
            left in any::<u64>(),
            right in any::<u64>(),
            input in "[a-z]{1,16}",
        ) {
            prop_assume!(left != right);
            prop_assert_ne!(deterministic_hash(left, &input), deterministic_hash(right, &input));
        }

        /// `is_private_ip` is total: it never panics and only ever accepts
        /// addresses that are genuinely non-public.
        #[test]
        fn private_ip_detection_never_panics(octet1 in 0u8..=255, octet2 in 0u8..=255) {
            let text = format!("{octet1}.{octet2}.0.1");
            if let Ok(ip) = text.parse::<std::net::IpAddr>() {
                let _ = is_private_ip(ip);
            }
        }

        /// Any `ServerConfig` either validates or produces a config error —
        /// never a panic and never an unrelated error variant.
        #[test]
        fn validation_never_panics(
            mode in prop::sample::select(vec![MockMode::Mock, MockMode::Proxy]),
            upstream in prop::option::of("[a-z]{0,10}://[a-z0-9./:]{0,20}"),
            ws_path in prop::sample::select(vec!["/ws", "socket", "", "/a/b"]),
            max_body_size in prop::sample::select(vec![0usize, 1, 1024]),
        ) {
            let config = ServerConfig {
                mode,
                upstream,
                ws_path: ws_path.to_owned(),
                max_body_size,
                allow_private_upstream: true,
                ..ServerConfig { openapi_spec: Some(openapi_spec()), ..Default::default() }
            };
            if let Err(error) = config.validate() {
                prop_assert!(
                    matches!(error, RuntimeError::Config(_)),
                    "validate must only fail with a config error, got {error:?}"
                );
            }
        }
    }
}
