//! Example showing process-mode SDK usage.

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

fn find_spec_mock_bin() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("SPEC_MOCK_BIN") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }

    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join("spec-mock");
    if local.is_file() {
        return Some(local);
    }

    None
}

fn free_local_addr() -> Result<SocketAddr, std::io::Error> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(bin_path) = find_spec_mock_bin() else {
        return Err("spec-mock binary not found (set SPEC_MOCK_BIN or build bin/spec-mock)".into());
    };

    let http_addr = free_local_addr()?;
    let grpc_addr = free_local_addr()?;

    let mut server = MockServer::builder()
        .openapi(openapi_spec_path())
        .seed(7)
        .http_addr(http_addr)
        .grpc_addr(grpc_addr)
        .start_process_with_bin(&bin_path)
        .await?;

    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    if response.status().as_u16() != 200 {
        return Err(format!("unexpected status: {}", response.status()).into());
    }

    server.shutdown()?;
    Ok(())
}
