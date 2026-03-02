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
