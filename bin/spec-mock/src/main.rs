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
    /// Maximum request body size in bytes.
    #[arg(long, default_value_t = 10_485_760)]
    max_body_size: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
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
    tracing_subscriber::fmt().with_target(false).with_level(true).compact().init();

    let cli = Cli::parse();
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
        .max_body_size(args.max_body_size);

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

    let server = builder.start().await?;
    tracing::info!("HTTP: {}", server.http_base_url());
    if let Some(grpc_addr) = server.grpc_addr() {
        tracing::info!("gRPC: {grpc_addr}");
    }
    tracing::info!("Press Ctrl+C to stop");

    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}
