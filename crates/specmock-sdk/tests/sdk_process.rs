//! Integration tests for process-mode SDK usage.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
};

use specmock_sdk::MockServer;

fn openapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("specmock-runtime")
        .join("tests")
        .join("specs")
        .join("openapi-pets.yaml")
}

fn free_local_addr() -> Result<SocketAddr, std::io::Error> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

fn process_bin_path() -> Option<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_spec-mock").and_then(|path| {
        let path = PathBuf::from(path);
        if path.is_file() { Some(path) } else { None }
    })
}

#[tokio::test]
async fn sdk_process_server_can_be_used_in_tokio_test() -> Result<(), Box<dyn std::error::Error>> {
    let Some(bin_path) = process_bin_path() else {
        return Ok(());
    };
    if !bin_path.is_file() {
        return Ok(());
    }

    let http_addr = free_local_addr()?;
    let grpc_addr = free_local_addr()?;

    let mut server = MockServer::builder()
        .openapi(openapi_spec_path())
        .http_addr(http_addr)
        .grpc_addr(grpc_addr)
        .seed(19)
        .start_process_with_bin(&bin_path)
        .await?;

    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    assert_eq!(response.status().as_u16(), 200);

    server.shutdown()?;
    Ok(())
}
