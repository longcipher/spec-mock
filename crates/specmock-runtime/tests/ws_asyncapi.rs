//! Integration tests for AsyncAPI WebSocket runtime behavior.

use std::{net::SocketAddr, path::PathBuf};

use futures_util::{SinkExt, StreamExt};
use specmock_core::MockMode;
use specmock_runtime::{RuntimeError, ServerConfig, start};
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn asyncapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join("asyncapi-chat.yaml")
}

fn asyncapi_v3_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("asyncapi-v3-chat.yaml")
}

#[tokio::test]
async fn ws_invalid_message_returns_structured_error_event()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let ws_url = format!("ws://{}/ws", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    socket
        .send(Message::Text(r#"{"channel":"chat.send","payload":{"room":123}}"#.to_owned().into()))
        .await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("error"));
    assert!(body.pointer("/errors/0/instance_pointer").is_some());
    assert!(body.pointer("/errors/0/schema_pointer").is_some());

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_valid_message_returns_mock_event() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let ws_url = format!("ws://{}/ws", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    socket
        .send(Message::Text(
            r#"{"channel":"chat.send","payload":{"room":"r1","text":"hello"}}"#.to_owned().into(),
        ))
        .await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("mock"));
    assert_eq!(body.get("channel").and_then(serde_json::Value::as_str), Some("chat.send"));
    assert_eq!(body.pointer("/payload/ok").and_then(serde_json::Value::as_bool), Some(true));

    server.shutdown().await;
    Ok(())
}

// ── AsyncAPI v3 integration tests ──────────────────────────────────

#[tokio::test]
async fn ws_v3_valid_message_returns_mock_event() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_v3_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let ws_url = format!("ws://{}/ws", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    socket
        .send(Message::Text(
            r#"{"channel":"chatChannel","payload":{"room":"r1","text":"hello"}}"#.to_owned().into(),
        ))
        .await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("mock"));
    assert_eq!(body.get("channel").and_then(serde_json::Value::as_str), Some("chatChannel"));
    assert_eq!(body.pointer("/payload/ok").and_then(serde_json::Value::as_bool), Some(true));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_v3_invalid_message_returns_error_event() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_v3_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let ws_url = format!("ws://{}/ws", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    // Send invalid payload — "room" should be a string, not a number.
    socket
        .send(Message::Text(
            r#"{"channel":"chatChannel","payload":{"room":123}}"#.to_owned().into(),
        ))
        .await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("error"));
    assert!(body.pointer("/errors/0/instance_pointer").is_some());

    server.shutdown().await;
    Ok(())
}
// ── Per-channel WebSocket path routing ─────────────────────────────

#[tokio::test]
async fn ws_per_channel_path_routes_raw_payload() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Connect to the per-channel path instead of the catch-all /ws.
    let ws_url = format!("ws://{}/ws/chat.send", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    // Send raw payload (no envelope) — the per-channel route wraps it
    // automatically with channel "chat.send".
    socket.send(Message::Text(r#"{"room":"r1","text":"hello"}"#.to_owned().into())).await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("mock"));
    assert_eq!(body.get("channel").and_then(serde_json::Value::as_str), Some("chat.send"));
    assert_eq!(body.pointer("/payload/ok").and_then(serde_json::Value::as_bool), Some(true));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_per_channel_path_invalid_payload_returns_error()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Connect to the per-channel path for chat.send.
    let ws_url = format!("ws://{}/ws/chat.send", server.http_addr);
    let (mut socket, _response) = connect_async(&ws_url).await?;

    // Send an invalid payload (room should be a string, not a number).
    socket.send(Message::Text(r#"{"room":123}"#.to_owned().into())).await?;

    let next_message = socket.next().await.ok_or("expected websocket response")??;
    let body: serde_json::Value = serde_json::from_str(next_message.to_text()?)?;

    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("error"));
    assert!(body.pointer("/errors/0/instance_pointer").is_some());

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_per_channel_path_unknown_channel_returns_not_found()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(asyncapi_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Connect to a path that doesn't correspond to any channel —
    // should fall through to the HTTP fallback (not a WS route).
    let ws_url = format!("ws://{}/ws/nonexistent", server.http_addr);
    let result = connect_async(&ws_url).await;

    // The server should reject the upgrade (returns HTTP 404 via fallback).
    assert!(result.is_err(), "expected connection to fail for unknown channel path");

    server.shutdown().await;
    Ok(())
}
