//! Example showing in-process SDK usage similar to a tokio integration test.

use std::path::PathBuf;

use specmock_sdk::MockServer;

fn openapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("specmock-runtime")
        .join("tests")
        .join("specs")
        .join("openapi-pets.yaml")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::builder().openapi(openapi_spec_path()).seed(42).start().await?;

    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    if response.status().as_u16() != 200 {
        return Err(format!("unexpected status: {}", response.status()).into());
    }

    server.shutdown().await;
    Ok(())
}
