//! Integration tests for embedded SDK mode.

use std::{net::SocketAddr, path::PathBuf};

use specmock_runtime::RuntimeError;
use specmock_sdk::{MockServer, SdkError};

fn openapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("specmock-runtime")
        .join("tests")
        .join("specs")
        .join("openapi-pets.yaml")
}

fn asyncapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("specmock-runtime")
        .join("tests")
        .join("specs")
        .join("asyncapi-chat.yaml")
}

#[tokio::test]
async fn sdk_embedded_server_can_be_used_in_tokio_test() -> Result<(), Box<dyn std::error::Error>> {
    let server = match MockServer::builder().openapi(openapi_spec_path()).seed(42).start().await {
        Ok(value) => value,
        Err(SdkError::Runtime(specmock_runtime::RuntimeError::Io(error)))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    assert_eq!(response.status().as_u16(), 200);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sdk_ws_url_reflects_configured_ws_path() -> Result<(), Box<dyn std::error::Error>> {
    let server =
        match MockServer::builder().asyncapi(asyncapi_spec_path()).ws_path("/socket").start().await
        {
            Ok(value) => value,
            Err(SdkError::Runtime(specmock_runtime::RuntimeError::Io(error)))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.to_string().into()),
        };

    assert!(
        server.ws_url().ends_with("/socket"),
        "ws_url must use the configured ws_path, got {}",
        server.ws_url()
    );

    let response = hpx::get(format!("{}/socket", server.http_base_url())).send().await?;
    assert_ne!(
        response.status().as_u16(),
        404,
        "the configured WebSocket path must be routed, not fall through to the OpenAPI handler"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sdk_rejects_ws_path_without_leading_slash() -> Result<(), Box<dyn std::error::Error>> {
    let result =
        MockServer::builder().asyncapi(asyncapi_spec_path()).ws_path("socket").start().await;
    let Err(error) = result else {
        return Err("ws_path without a leading slash must be rejected".into());
    };

    assert!(
        error.to_string().contains("'/'"),
        "error should explain the leading-slash requirement, got: {error}"
    );
    Ok(())
}

/// The scenario fixtures live with the runtime's integration suite.
fn spec(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../specmock-runtime/tests/specs").join(name)
}

async fn scenario_server() -> Result<Option<MockServer>, Box<dyn std::error::Error>> {
    let builder = MockServer::builder()
        .openapi(spec("openapi-store.yaml"))
        .scenarios(spec("openapi-store.scenarios.yaml"))
        .seed(42)
        .http_addr(SocketAddr::from(([127, 0, 0, 1], 0)));

    match builder.start().await {
        Ok(server) => Ok(Some(server)),
        Err(SdkError::Runtime(RuntimeError::Io(error)))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            Ok(None)
        }
        Err(error) => Err(error.to_string().into()),
    }
}

#[tokio::test]
async fn an_embedded_server_reports_and_resets_scenario_state()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = scenario_server().await? else {
        return Ok(());
    };
    let base = server.http_base_url();

    assert_eq!(server.scenario_state(""), None, "no instance exists until a request arrives");

    hpx::Client::new().get(format!("{base}/widgets/seeded-1")).send().await?;
    let (scenario, state) = server.scenario_state("").ok_or("expected a live instance")?;
    assert_eq!(scenario, "catalogue");
    assert_eq!(state, "browsing");

    assert_eq!(server.reset_scenarios(), Some(1));
    let (_, after) = server.scenario_state("").ok_or("reset must not drop the instance")?;
    assert_eq!(after, "browsing");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn resetting_clears_the_store_as_well_as_the_state() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(server) = scenario_server().await? else {
        return Ok(());
    };
    let base = server.http_base_url();

    // Drive the `fragile` scenario into its failing state, then reset.
    hpx::Client::new()
        .get(format!("{base}/gadgets/fragile-1"))
        .header("Prefer", "scenario=fragile")
        .send()
        .await?;
    assert_eq!(
        server.scenario_state_for("fragile", "").map(|(_, state)| state),
        Some("failing".to_owned())
    );

    server.reset_scenarios();

    // Back at the initial state, the same read is served again rather than
    // continuing from `failing`.
    let response = hpx::Client::new()
        .get(format!("{base}/gadgets/fragile-1"))
        .header("Prefer", "scenario=fragile")
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 503, "the flow restarted from `healthy`");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_server_without_a_sidecar_reports_no_scenarios() -> Result<(), Box<dyn std::error::Error>>
{
    let builder = MockServer::builder()
        .openapi(spec("openapi-store.yaml"))
        .http_addr(SocketAddr::from(([127, 0, 0, 1], 0)));

    let server = match builder.start().await {
        Ok(server) => server,
        Err(SdkError::Runtime(RuntimeError::Io(error)))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    assert_eq!(server.reset_scenarios(), None);
    assert_eq!(server.scenario_state(""), None);
    assert_eq!(server.scenario_state_for("anything", ""), None);
    assert_eq!(server.control_token(), None);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn enabling_control_endpoints_mints_a_token() -> Result<(), Box<dyn std::error::Error>> {
    let builder = MockServer::builder()
        .openapi(spec("openapi-store.yaml"))
        .scenarios(spec("openapi-store.scenarios.yaml"))
        .control_endpoints()
        .http_addr(SocketAddr::from(([127, 0, 0, 1], 0)));

    let server = match builder.start().await {
        Ok(server) => server,
        Err(SdkError::Runtime(RuntimeError::Io(error)))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let token = server.control_token().ok_or("a token should be minted").map(str::to_owned);
    let token = token.unwrap_or_default();
    assert_eq!(token.len(), 16, "expected a hex token, got {token:?}");

    server.shutdown().await;
    Ok(())
}
