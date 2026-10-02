//! Integration tests for embedded SDK mode.

use std::path::PathBuf;

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
