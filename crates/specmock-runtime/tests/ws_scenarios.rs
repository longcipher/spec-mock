//! End-to-end tests for scenario-driven WebSocket mocking.
//!
//! A WebSocket connection is the natural session scope: it is a long-lived,
//! self-contained conversation, and HTTP has no equivalent. These tests drive
//! real connections and assert that a resource published on one channel comes
//! back on another, and that two connections do not share a store.

use std::{net::SocketAddr, path::PathBuf};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use specmock_core::MockMode;
use specmock_runtime::{RunningServer, RuntimeError, ServerConfig, start};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

fn spec(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join(name)
}

fn config() -> ServerConfig {
    ServerConfig {
        asyncapi_spec: Some(spec("asyncapi-store.yaml")),
        scenarios_spec: Some(spec("asyncapi-store.scenarios.yaml")),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    }
}

async fn boot() -> Result<Option<RunningServer>, Box<dyn std::error::Error>> {
    match start(config()).await {
        Ok(server) => Ok(Some(server)),
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(None)
        }
        Err(error) => Err(error.to_string().into()),
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Send one text frame and read the reply.
async fn exchange(socket: &mut Socket, text: &str) -> Result<Value, Box<dyn std::error::Error>> {
    socket.send(WsMessage::text(text.to_owned())).await?;

    loop {
        let Some(Ok(item)) = socket.next().await else {
            break;
        };
        if let WsMessage::Text(reply) = item {
            return Ok(serde_json::from_str(&reply)?);
        }
    }
    Err("socket closed before replying".into())
}

#[tokio::test]
async fn two_connections_sharing_a_session_see_the_same_store()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let (mut created, _) =
        connect_async(format!("{base}/widgets.created?scenario=events&session=shared")).await?;
    let published =
        exchange(&mut created, &json!({"id": "e1", "name": "First"}).to_string()).await?;
    assert_eq!(published["type"], "mock", "reply was {published}");
    assert_eq!(published["payload"]["id"], "e1", "the publish is accepted");

    let (mut fetched, _) =
        connect_async(format!("{base}/widgets.fetched?scenario=events&session=shared")).await?;
    let got = exchange(&mut fetched, &json!({"id": "e1"}).to_string()).await?;
    assert_eq!(
        got["payload"],
        json!({"id": "e1", "name": "First"}),
        "the second connection must read what the first published; reply was {got}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_seeded_resource_is_readable_without_publishing_it_first()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let (mut fetched, _) = connect_async(format!("{base}/widgets.fetched?scenario=events")).await?;
    let got = exchange(&mut fetched, &json!({"id": "seeded-1"}).to_string()).await?;

    assert_eq!(
        got["payload"],
        json!({"id": "seeded-1", "name": "Preloaded Widget"}),
        "reply was {got}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn two_connections_do_not_share_a_store() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let publish = |id: &'static str| {
        let base = base.clone();
        async move {
            let (mut socket, _) =
                connect_async(format!("{base}/widgets.created?scenario=events")).await?;
            exchange(&mut socket, &json!({"id": id, "name": id}).to_string()).await
        }
    };
    publish("shared").await?;

    let read = || {
        let base = base.clone();
        async move {
            let (mut socket, _) =
                connect_async(format!("{base}/widgets.fetched?scenario=events")).await?;
            exchange(&mut socket, &json!({"id": "shared"}).to_string()).await
        }
    };

    assert_eq!(read().await?["type"], "error", "a fresh connection starts with an empty store");

    // Two creates on one connection collide, because one connection is one store.
    let (mut socket, _) = connect_async(format!("{base}/widgets.created?scenario=events")).await?;
    let first = exchange(&mut socket, &json!({"id": "twice", "name": "First"}).to_string()).await?;
    assert_eq!(first["type"], "mock");
    let second =
        exchange(&mut socket, &json!({"id": "twice", "name": "Second"}).to_string()).await?;
    assert_eq!(
        second["type"], "error",
        "a create that collides must be refused rather than overwriting: {second}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn fetching_an_unknown_resource_reports_a_structured_error()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let (mut socket, _) = connect_async(format!("{base}/widgets.fetched?scenario=events")).await?;
    let got = exchange(&mut socket, &json!({"id": "ghost"}).to_string()).await?;

    assert_eq!(got["type"], "error");
    let message = got["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(message.contains("/widgets/ghost"), "the error must name the resource: {got}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_closed_connection_releases_its_session() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { max_sessions: 1, ..config() };
    let server = match start(config).await {
        Ok(server) => server,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };
    let base = format!("ws://{}/ws", server.http_addr);

    // With a cap of one, the second connection would be refused if the first
    // still held its session. Dropping the socket must return it.
    for _ in 0..3 {
        let (mut socket, _) =
            connect_async(format!("{base}/widgets.fetched?scenario=events")).await?;
        let got = exchange(&mut socket, &json!({"id": "seeded-1"}).to_string()).await?;
        assert_eq!(got["type"], "mock", "each connection gets a usable session: {got}");
        drop(socket);
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_unknown_scenario_name_reports_a_structured_error()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let (mut socket, _) = connect_async(format!("{base}/widgets.fetched?scenario=ghost")).await?;
    let got = exchange(&mut socket, &json!({"id": "seeded-1"}).to_string()).await?;

    assert_eq!(got["type"], "error");
    let message = got["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(message.contains("ghost"), "the error must name what was asked for: {got}");
    assert!(message.contains("events"), "and list what exists: {got}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_channel_no_scenario_mentions_is_served_normally()
-> Result<(), Box<dyn std::error::Error>> {
    // The sidecar is an additive overlay: an AsyncAPI channel it does not name
    // must keep working exactly as before.
    let Some(server) = boot().await? else {
        return Ok(());
    };
    let base = format!("ws://{}/ws", server.http_addr);

    let (mut socket, _) = connect_async(format!("{base}/widgets.archived?scenario=events")).await?;
    let got = exchange(&mut socket, &json!({"id": "x"}).to_string()).await?;
    assert_eq!(got["type"], "mock", "an unlisted channel must still be served: {got}");
    assert!(
        got["payload"].get("archived").is_some_and(Value::is_boolean),
        "and answered from its own subscribe schema rather than the store: {got}"
    );

    server.shutdown().await;
    Ok(())
}
