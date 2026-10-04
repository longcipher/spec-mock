//! CLI entrypoint for standalone spec-mock runtime.

use std::net::SocketAddr;

use clap::{Parser, Subcommand, ValueEnum};
use specmock_core::MockMode;
use specmock_sdk::MockServer;

#[derive(Debug, Parser)]
#[command(
    name = "spec-mock",
    version,
    about = "Spec-driven mock runtime for OpenAPI/AsyncAPI/Protobuf"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start mock runtime.
    Serve(ServeArgs),
}

#[derive(Debug, Parser)]
struct ServeArgs {
    /// OpenAPI spec file path.
    #[arg(long)]
    openapi: Option<std::path::PathBuf>,
    /// AsyncAPI spec file path.
    #[arg(long)]
    asyncapi: Option<std::path::PathBuf>,
    /// Protobuf root .proto file path.
    #[arg(long)]
    proto: Option<std::path::PathBuf>,
    /// Runtime mode.
    #[arg(long, value_enum, default_value = "mock")]
    mode: ModeArg,
    /// Proxy upstream base URL.
    #[arg(long, required_if_eq("mode", "proxy"))]
    upstream: Option<String>,
    /// Deterministic data seed.
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// HTTP bind address.
    #[arg(long, default_value = "127.0.0.1:4010")]
    http_addr: SocketAddr,
    /// gRPC bind address.
    #[arg(long, default_value = "127.0.0.1:5010")]
    grpc_addr: SocketAddr,
    /// WebSocket path served by the AsyncAPI runtime.
    #[arg(long, default_value = "/ws")]
    ws_path: String,
    /// Maximum request body size in bytes.
    #[arg(long, default_value_t = 10_485_760)]
    max_body_size: usize,
    /// Allow private/link-local/loopback upstream URLs in proxy mode.
    #[arg(long)]
    allow_private_upstream: bool,
    /// Scenario sidecar declaring stateful behaviour.
    ///
    /// Resolved alongside the OpenAPI document rather than inside it, so a spec
    /// owned by someone else can be mocked without being forked.
    #[arg(long)]
    scenarios: Option<std::path::PathBuf>,
    /// Maximum live scenario instances across all sessions.
    #[arg(long, default_value_t = specmock_sdk::DEFAULT_MAX_SESSIONS)]
    max_sessions: usize,
    /// Maximum stored resources per scenario instance.
    #[arg(long, default_value_t = specmock_sdk::DEFAULT_MAX_STORE_ENTRIES)]
    max_store_entries: usize,
    /// Log verbosity.
    ///
    /// Scenario transitions log at `info`, because a state machine you cannot
    /// observe is the main reason people give up on a mock and fall back to
    /// something else.
    #[arg(long, value_enum, default_value = "info")]
    log_level: LogLevelArg,
    /// Disable the control endpoints under `/_specmock/`.
    ///
    /// They are the only mutating surface on the server, so they are guarded by
    /// a per-run bearer token unless this is passed.
    #[arg(long)]
    no_control_endpoints: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum LogLevelArg {
    /// Errors only.
    Error,
    /// Errors and warnings.
    Warn,
    /// Includes scenario transitions. The default.
    #[default]
    Info,
    /// Verbose diagnostics.
    Debug,
    /// Everything, including per-request traces.
    Trace,
}

impl LogLevelArg {
    const fn into_level(self) -> tracing::Level {
        match self {
            Self::Error => tracing::Level::ERROR,
            Self::Warn => tracing::Level::WARN,
            Self::Info => tracing::Level::INFO,
            Self::Debug => tracing::Level::DEBUG,
            Self::Trace => tracing::Level::TRACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum ModeArg {
    #[default]
    Mock,
    Proxy,
}

impl ModeArg {
    const fn into_runtime_mode(self) -> MockMode {
        match self {
            Self::Mock => MockMode::Mock,
            Self::Proxy => MockMode::Proxy,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), eyre::Report> {
    let cli = Cli::parse();

    let level = match &cli.command {
        Commands::Serve(args) => args.log_level.into_level(),
    };
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_max_level(level)
        .compact()
        .init();

    match cli.command {
        Commands::Serve(args) => serve_command(args).await?,
    }

    Ok(())
}

async fn serve_command(args: ServeArgs) -> Result<(), eyre::Report> {
    let mut builder = MockServer::builder()
        .seed(args.seed)
        .mode(args.mode.into_runtime_mode())
        .http_addr(args.http_addr)
        .grpc_addr(args.grpc_addr)
        .ws_path(args.ws_path)
        .max_body_size(args.max_body_size)
        .allow_private_upstream(args.allow_private_upstream)
        .max_sessions(args.max_sessions)
        .max_store_entries(args.max_store_entries);

    if let Some(openapi) = args.openapi {
        builder = builder.openapi(openapi);
    }
    if let Some(asyncapi) = args.asyncapi {
        builder = builder.asyncapi(asyncapi);
    }
    if let Some(proto) = args.proto {
        builder = builder.proto(proto);
    }
    if let Some(upstream) = args.upstream {
        builder = builder.upstream(upstream);
    }
    if let Some(scenarios) = args.scenarios {
        builder = builder.scenarios(scenarios);
    }
    if !args.no_control_endpoints {
        builder = builder.control_endpoints();
    }

    let server = builder.start().await?;
    tracing::info!("HTTP: {}", server.http_base_url());
    if let Some(grpc_addr) = server.grpc_addr() {
        tracing::info!("gRPC: {grpc_addr}");
    }
    if let Some(control) = server.control_token() {
        tracing::info!(
            "control endpoints: {}/_specmock (bearer {control})",
            server.http_base_url()
        );
    }
    tracing::info!("Press Ctrl+C to stop");

    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test code: a failed assertion should surface as a panic"
)]
mod tests {
    use clap::{CommandFactory, Parser};
    use specmock_core::MockMode;

    use super::{Cli, Commands, ModeArg, ServeArgs};

    /// Parse a full argv, panicking with a message on failure.
    fn parse(args: &[&str]) -> Cli {
        match Cli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(error) => panic!("failed to parse {args:?}: {error}"),
        }
    }

    /// Extract the `Serve` arguments from an argv slice.
    fn serve_args(args: &[&str]) -> ServeArgs {
        // `Commands` currently has exactly one variant, so this match is total.
        let Commands::Serve(serve) = parse(args).command;
        serve
    }

    // ── subcommand wiring ──────────────────────────────────────────────

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_is_the_only_subcommand() {
        let command = Cli::command();
        let names: Vec<String> =
            command.get_subcommands().map(|sub| sub.get_name().to_owned()).collect();
        assert_eq!(names, vec!["serve".to_owned()]);
    }

    #[test]
    fn no_arguments_is_an_error() {
        assert!(Cli::try_parse_from(["spec-mock"]).is_err(), "a subcommand is required");
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["spec-mock", "destroy"]).is_err());
    }

    // ── defaults ───────────────────────────────────────────────────────

    #[test]
    fn serve_defaults_are_production_safe() {
        let args = serve_args(&["spec-mock", "serve"]);
        assert_eq!(args.openapi, None);
        assert_eq!(args.asyncapi, None);
        assert_eq!(args.proto, None);
        assert_eq!(args.mode, ModeArg::Mock);
        assert_eq!(args.upstream, None);
        assert_eq!(args.seed, 42);
        assert_eq!(args.http_addr.to_string(), "127.0.0.1:4010");
        assert_eq!(args.grpc_addr.to_string(), "127.0.0.1:5010");
        assert_eq!(args.ws_path, "/ws");
        assert_eq!(args.max_body_size, 10_485_760);
        assert!(!args.allow_private_upstream);
    }

    // ── spec selection ─────────────────────────────────────────────────

    #[test]
    fn all_three_specs_can_be_served_together() {
        let args = serve_args(&[
            "spec-mock",
            "serve",
            "--openapi",
            "api.yaml",
            "--asyncapi",
            "chat.yaml",
            "--proto",
            "greeter.proto",
        ]);
        assert_eq!(
            args.openapi.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("api.yaml"))
        );
        assert_eq!(
            args.asyncapi.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("chat.yaml"))
        );
        assert_eq!(
            args.proto.as_deref().map(std::path::Path::new),
            Some(std::path::Path::new("greeter.proto"))
        );
    }

    #[test]
    fn a_spec_is_not_mandatory_at_parse_time() {
        // Requiring a spec is `ServerConfig::validate`'s job, not clap's, so
        // the failure surfaces with a runtime message instead of a usage error.
        assert!(Cli::try_parse_from(["spec-mock", "serve"]).is_ok());
    }

    // ── mode and upstream ──────────────────────────────────────────────

    #[test]
    fn mode_accepts_both_variants() {
        let mock = serve_args(&["spec-mock", "serve", "--mode", "mock"]);
        assert_eq!(mock.mode, ModeArg::Mock);

        // `--mode proxy` also demands `--upstream` (see the next test).
        let proxy = serve_args(&[
            "spec-mock",
            "serve",
            "--mode",
            "proxy",
            "--upstream",
            "http://api.example.com",
        ]);
        assert_eq!(proxy.mode, ModeArg::Proxy);
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--mode", "record"]).is_err());
    }

    #[test]
    fn proxy_mode_requires_an_upstream() {
        let error = Cli::try_parse_from(["spec-mock", "serve", "--mode", "proxy"])
            .expect_err("proxy mode must demand --upstream");
        let rendered = error.to_string();
        assert!(rendered.contains("--upstream"), "got {rendered}");
    }

    #[test]
    fn proxy_mode_with_an_upstream_parses() {
        let args = serve_args(&[
            "spec-mock",
            "serve",
            "--mode",
            "proxy",
            "--upstream",
            "http://127.0.0.1:9999",
        ]);
        assert_eq!(args.upstream.as_deref(), Some("http://127.0.0.1:9999"));
    }

    #[test]
    fn upstream_without_proxy_mode_is_still_accepted() {
        let args = serve_args(&["spec-mock", "serve", "--upstream", "http://api.example.com"]);
        assert_eq!(args.upstream.as_deref(), Some("http://api.example.com"));
        assert_eq!(args.mode, ModeArg::Mock);
    }

    #[test]
    fn mode_arg_maps_onto_the_runtime_mode() {
        assert_eq!(ModeArg::Mock.into_runtime_mode(), MockMode::Mock);
        assert_eq!(ModeArg::Proxy.into_runtime_mode(), MockMode::Proxy);
        assert_eq!(ModeArg::default(), ModeArg::Mock, "the default must be mock mode");
    }

    // ── numeric and address arguments ──────────────────────────────────

    #[test]
    fn seed_accepts_the_full_u64_range() {
        for raw in ["0", "1", "42", "18446744073709551615"] {
            let args = serve_args(&["spec-mock", "serve", "--seed", raw]);
            assert_eq!(args.seed.to_string(), raw, "seed {raw} must round-trip");
        }
    }

    #[test]
    fn a_negative_or_overflowing_seed_is_rejected() {
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--seed", "-1"]).is_err());
        assert!(
            Cli::try_parse_from(["spec-mock", "serve", "--seed", "18446744073709551616"]).is_err()
        );
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--seed", "abc"]).is_err());
    }

    #[test]
    fn bind_addresses_accept_host_port_forms() {
        let args = serve_args(&[
            "spec-mock",
            "serve",
            "--http-addr",
            "0.0.0.0:8080",
            "--grpc-addr",
            "[::1]:9090",
        ]);
        assert_eq!(args.http_addr.to_string(), "0.0.0.0:8080");
        assert_eq!(args.grpc_addr.to_string(), "[::1]:9090");
    }

    #[test]
    fn a_malformed_bind_address_is_rejected() {
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--http-addr", "not-an-addr"]).is_err());
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--grpc-addr", "8080"]).is_err());
    }

    #[test]
    fn max_body_size_accepts_zero_and_rejects_negatives() {
        let args = serve_args(&["spec-mock", "serve", "--max-body-size", "0"]);
        assert_eq!(args.max_body_size, 0);
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--max-body-size", "-1"]).is_err());
    }

    #[test]
    fn ws_path_is_taken_verbatim() {
        for raw in ["/ws", "/socket", "socket", "/nested/socket"] {
            let args = serve_args(&["spec-mock", "serve", "--ws-path", raw]);
            assert_eq!(args.ws_path, raw, "the leading-slash check lives in the runtime");
        }
    }

    #[test]
    fn allow_private_upstream_is_a_boolean_flag() {
        let args = serve_args(&["spec-mock", "serve", "--allow-private-upstream"]);
        assert!(args.allow_private_upstream);

        let args = serve_args(&["spec-mock", "serve"]);
        assert!(!args.allow_private_upstream);
    }

    // ── unknown arguments ──────────────────────────────────────────────

    #[test]
    fn an_unknown_flag_is_rejected() {
        assert!(Cli::try_parse_from(["spec-mock", "serve", "--turbo"]).is_err());
        assert!(
            Cli::try_parse_from(["spec-mock", "serve", "--openapi"]).is_err(),
            "a missing value is an error"
        );
    }

    #[test]
    fn version_and_help_are_available() {
        use clap::error::ErrorKind;

        // clap reports `--version` / `--help` as a *rendered* error rather than
        // a parse success, so the kind is what matters here.
        let cases: [&[&str]; 4] = [
            &["spec-mock", "--version"],
            &["spec-mock", "--help"],
            &["spec-mock", "serve", "--help"],
            &["spec-mock", "serve", "--openapi", "api.yaml", "--help"],
        ];
        for args in cases {
            match Cli::try_parse_from(args) {
                Ok(_) => {}
                Err(error) => assert!(
                    matches!(error.kind(), ErrorKind::DisplayVersion | ErrorKind::DisplayHelp),
                    "{args:?} should render help or version, got: {error}"
                ),
            }
        }
    }

    #[test]
    fn the_command_name_matches_the_binary() {
        assert_eq!(Cli::command().get_name(), "spec-mock");
    }
}
