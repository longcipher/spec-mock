//! Integration tests for process-mode SDK usage.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
};

use futures_util::{SinkExt, StreamExt};
use specmock_sdk::{MockServer, ProcessMockServer};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

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
        path.is_file().then_some(path)
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

fn asyncapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("specmock-runtime")
        .join("tests")
        .join("specs")
        .join("asyncapi-chat.yaml")
}

/// Start process mode with every spec loaded, skipping when the binary is not
/// available in this build configuration.
async fn boot_full_stack() -> Result<Option<ProcessMockServer>, Box<dyn std::error::Error>> {
    let Some(bin_path) = process_bin_path() else {
        return Ok(None);
    };
    if !bin_path.is_file() {
        return Ok(None);
    }

    let server = MockServer::builder()
        .openapi(openapi_spec_path())
        .asyncapi(asyncapi_spec_path())
        .http_addr(free_local_addr()?)
        .grpc_addr(free_local_addr()?)
        .ws_path("/ws")
        .seed(19)
        .start_process_with_bin(&bin_path)
        .await?;
    Ok(Some(server))
}

#[tokio::test]
async fn process_mode_serves_http_and_websocket_from_one_process()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(mut server) = boot_full_stack().await? else {
        return Ok(());
    };

    // HTTP.
    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert!(body.get("id").is_some() && body.get("name").is_some(), "got {body}");

    // WebSocket.
    let (mut socket, _) = connect_async(server.ws_url()).await?;
    socket
        .send(WsMessage::Text(
            r#"{"channel":"chat.send","payload":{"room":"r","text":"t"}}"#.to_owned().into(),
        ))
        .await?;
    let message = socket.next().await.ok_or("expected a ws reply")??;
    let event: serde_json::Value = serde_json::from_str(message.to_text()?)?;
    assert_eq!(event["type"], "mock", "got {event}");

    server.shutdown()?;
    Ok(())
}

#[tokio::test]
async fn process_mode_shutdown_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
    let Some(mut server) = boot_full_stack().await? else {
        return Ok(());
    };

    let base = server.http_base_url();
    server.shutdown()?;
    // A second shutdown must be a no-op rather than an error.
    server.shutdown()?;

    // The listener must be gone.
    let probe = hpx::get(format!("{base}/pets/1")).send().await;
    assert!(probe.is_err(), "the child process must be reaped after shutdown");

    Ok(())
}

#[tokio::test]
async fn process_mode_surfaces_a_bad_spec_before_returning_a_handle()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(bin_path) = process_bin_path() else {
        return Ok(());
    };
    if !bin_path.is_file() {
        return Ok(());
    }

    // Point at a file that exists but is not a valid OpenAPI document.
    let error = MockServer::builder()
        .openapi(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("specmock-runtime")
                .join("tests")
                .join("specs")
                .join("greeter.proto"),
        )
        .http_addr(free_local_addr()?)
        .grpc_addr(free_local_addr()?)
        .start_process_with_bin(&bin_path)
        .await
        .err()
        .ok_or("starting with an invalid spec must fail")?;

    let message = error.to_string();
    assert!(
        message.contains("before http listener became ready") || message.contains("invalid"),
        "expected a startup failure describing the problem, got: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn embedded_and_process_modes_agree_on_generated_data()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(mut process_server) = boot_full_stack().await? else {
        return Ok(());
    };
    let process_body: serde_json::Value = serde_json::from_slice(
        &hpx::get(format!("{}/pets/1", process_server.http_base_url()))
            .send()
            .await?
            .bytes()
            .await?,
    )?;

    // The embedded runtime is configured identically, so determinism must hold
    // across both start modes.
    let embedded = MockServer::builder().openapi(openapi_spec_path()).seed(19).start().await?;
    let embedded_body: serde_json::Value = serde_json::from_slice(
        &hpx::get(format!("{}/pets/1", embedded.http_base_url())).send().await?.bytes().await?,
    )?;

    embedded.shutdown().await;
    process_server.shutdown()?;

    assert_eq!(
        process_body, embedded_body,
        "embedded and process mode must generate identical data for the same seed"
    );
    Ok(())
}
