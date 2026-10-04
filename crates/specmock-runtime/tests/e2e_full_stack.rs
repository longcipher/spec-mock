//! End-to-end tests that drive the whole runtime the way a client would.
//!
//! These complement the per-protocol suites (`http_openapi`, `ws_asyncapi`,
//! `grpc_protobuf`) by exercising the paths that only show up when the
//! runtimes share one process: simultaneous HTTP + WS serving, rate limiting,
//! HEAD/OPTIONS routing, cross-restart determinism, hostile input, and
//! concurrent load.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use futures_util::{SinkExt, StreamExt};
use specmock_core::MockMode;
use specmock_runtime::{RunningServer, RuntimeError, ServerConfig, start};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn spec(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join(name)
}

/// Start a server, tolerating sandboxes that forbid binding a socket.
async fn boot(config: ServerConfig) -> Result<Option<RunningServer>, Box<dyn std::error::Error>> {
    match start(config).await {
        Ok(server) => Ok(Some(server)),
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(None)
        }
        Err(error) => Err(error.to_string().into()),
    }
}

fn base_config() -> ServerConfig {
    ServerConfig {
        openapi_spec: Some(spec("openapi-edge.yaml")),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        grpc_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    }
}

/// Read a response into `(status, content-type, body)`.
async fn fetch(
    response: hpx::Response,
) -> Result<(u16, Option<String>, Vec<u8>), Box<dyn std::error::Error>> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response.bytes().await?.to_vec();
    Ok((status, content_type, body))
}

fn json_body(bytes: &[u8]) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(bytes)?)
}

#[tokio::test]
async fn http_and_ws_serve_from_one_process() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { asyncapi_spec: Some(spec("asyncapi-chat.yaml")), ..base_config() };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    // 1. HTTP.
    let (status, _, body) =
        fetch(hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?).await?;
    assert_eq!(status, 200);
    let pet = json_body(&body)?;
    assert!(pet.get("id").is_some() && pet.get("name").is_some(), "got {pet}");

    // 2. WebSocket.
    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.http_addr)).await?;
    socket
        .send(WsMessage::Text(
            r#"{"channel":"chat.send","payload":{"room":"r","text":"t"}}"#.to_owned().into(),
        ))
        .await?;
    let message = socket.next().await.ok_or("expected a ws reply")??;
    let event: serde_json::Value = serde_json::from_str(message.to_text()?)?;
    assert_eq!(event["type"], "mock", "got {event}");

    // 3. Both are still healthy afterwards.
    let (status, _, _) =
        fetch(hpx::get(format!("http://{}/health", server.http_addr)).send().await?).await?;
    assert_eq!(status, 200);

    server.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP routing: HEAD, OPTIONS, bodyless responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn head_and_options_operations_are_routed() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    for method in ["HEAD", "OPTIONS"] {
        for path in ["/status", "/pets/1"] {
            let method = http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::GET);
            let response = hpx::Client::new()
                .request(method, format!("http://{}{path}", server.http_addr))
                .send()
                .await?;
            assert_eq!(
                response.status().as_u16(),
                200,
                "a HEAD/OPTIONS request must be routed and mocked"
            );
        }
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unsupported_method_on_a_known_path_returns_404() -> Result<(), Box<dyn std::error::Error>>
{
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    // `/health` only declares `get`, so a `delete` must not match.
    let response =
        hpx::Client::new().delete(format!("http://{}/health", server.http_addr)).send().await?;
    let (status, _, body) = fetch(response).await?;
    assert_eq!(status, 404, "an undeclared method must not match an existing path");

    let problem = json_body(&body)?;
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["type"], "about:blank");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn bodyless_204_has_no_body_and_no_content_type() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    let response = hpx::get(format!("http://{}/empty", server.http_addr)).send().await?;
    let (status, content_type, body) = fetch(response).await?;

    assert_eq!(status, 204);
    assert!(body.is_empty(), "a 204 must not carry a body");
    assert!(content_type.is_none(), "a 204 must not advertise a content type");

    server.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Request validation across parameter kinds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_parameter_bounds_are_enforced() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    // Inside bounds.
    for query in ["?limit=1", "?limit=100", "?limit=50", ""] {
        let response = hpx::get(format!("http://{}/pets{query}", server.http_addr)).send().await?;
        assert_eq!(response.status().as_u16(), 200, "limit query {query:?} should be accepted");
    }

    // Outside bounds.
    for query in ["?limit=0", "?limit=101", "?limit=-3"] {
        let response = hpx::get(format!("http://{}/pets{query}", server.http_addr)).send().await?;
        assert_eq!(response.status().as_u16(), 400, "limit query {query:?} should be rejected");
    }

    // A non-numeric value for a typed parameter.
    let response = hpx::get(format!("http://{}/pets?limit=abc", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 400);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn boolean_query_parameter_is_coerced() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    for query in ["?verbose=true", "?verbose=false"] {
        let response = hpx::get(format!("http://{}/pets{query}", server.http_addr)).send().await?;
        assert_eq!(response.status().as_u16(), 200, "{query} should coerce to a boolean");
    }

    let response =
        hpx::get(format!("http://{}/pets?verbose=maybe", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 400, "an uncoercible value must be rejected");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unknown_query_parameters_are_ignored() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    let response =
        hpx::get(format!("http://{}/pets?nonsense=1&limit=5", server.http_addr)).send().await?;
    assert_eq!(
        response.status().as_u16(),
        200,
        "an undeclared query parameter must not fail the request"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn post_body_matrix_covers_validation_and_media_types()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    // 1. A conforming JSON body.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "application/json")
        .body(r#"{"id":1,"name":"Rex"}"#)
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 201);

    // 2. A JSON body missing a required property.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "application/json")
        .body(r#"{"id":1}"#)
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 400);
    let problem = json_body(&response.bytes().await?)?;
    assert!(problem["errors"].as_array().map(|list| !list.is_empty()).unwrap_or(false));

    // 3. A wrong scalar type.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "application/json")
        .body(r#"{"id":"not-a-number","name":"Rex"}"#)
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 400);

    // 4. An empty body when one is required.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "application/json")
        .body("")
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 400, "a required body must not be omitted");

    // 5. A declared non-JSON media type is accepted but not schema-checked.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "text/plain")
        .body("a raw document that would fail JSON validation")
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 201, "text/plain is declared for this operation");

    // 6. An undeclared media type is rejected.
    let response = hpx::Client::new()
        .post(format!("http://{}/pets", server.http_addr))
        .header("Content-Type", "application/xml")
        .body("<pet/>")
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 415);

    server.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identical_seeds_produce_identical_responses_across_restarts()
-> Result<(), Box<dyn std::error::Error>> {
    let capture = async || -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let config = ServerConfig { seed: 1234, ..base_config() };
        let Some(server) = boot(config).await? else {
            return Ok(Vec::new());
        };

        let mut bodies = Vec::new();
        for path in ["/pets", "/pets/1", "/pets/2"] {
            let response = hpx::get(format!("http://{}{path}", server.http_addr)).send().await?;
            bodies.push(String::from_utf8(response.bytes().await?.to_vec()).unwrap_or_default());
        }
        server.shutdown().await;
        Ok(bodies)
    };

    let first = capture().await?;
    if first.is_empty() {
        return Ok(());
    }
    let second = capture().await?;

    assert_eq!(first, second, "the same seed must reproduce byte-identical bodies");

    Ok(())
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

#[tokio::test]
#[expect(clippy::panic, reason = "test code: a dropped connection is a test failure, not a bug")]
async fn hostile_paths_never_crash_the_server() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    let paths = [
        "/".to_owned(),
        "//".to_owned(),
        "/pets/../../etc/passwd".to_owned(),
        "/pets/%2e%2e%2f".to_owned(),
        "/pets/1/extra/segments".to_owned(),
        "/pets/not-a-number".to_owned(),
        "/pets/9999999999999999999999".to_owned(),
        "/pets/-1".to_owned(),
        format!("/pets/{}", "x".repeat(2000)),
        "/does/not/exist".to_owned(),
        "/pets/1?limit=".to_owned(),
        "/pets?limit=1&limit=2&limit=3".to_owned(),
    ];

    for path in paths {
        let url = format!("http://{}{path}", server.http_addr);
        let response = hpx::get(&url).send().await;
        let Ok(response) = response else {
            panic!("the server dropped the connection for {path:?}");
        };
        let status = response.status().as_u16();
        let _ = response.bytes().await?;
        assert!((200..500).contains(&status), "{path:?} produced an unexpected status {status}");
    }

    // The server must still be healthy after all of that.
    let response = hpx::get(format!("http://{}/health", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 200, "the server must survive hostile input");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn malformed_request_lines_get_a_problem_response() -> Result<(), Box<dyn std::error::Error>>
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    let cases: [&[u8]; 4] = [
        b"GET /pets/1 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nab",
        b"\r\n\r\n",
        b"GARBAGE\r\n\r\n",
        b"GET /pets/1 HTTP/9.9\r\n\r\n",
    ];

    for case in cases {
        let mut stream = tokio::net::TcpStream::connect(server.http_addr).await?;
        stream.write_all(case).await?;

        // Read whatever comes back; a malformed request may be answered with a
        // problem response, closed outright, or left to time out waiting for the
        // rest of a declared body. All three are acceptable — the invariant is
        // that nothing leaks that is not a valid HTTP response.
        let mut buffer = vec![0_u8; 1024];
        let received = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer)).await;

        if let Ok(Ok(count)) = received {
            let response = String::from_utf8_lossy(&buffer[..count]);
            if count > 0 {
                let first_line = response.lines().next().unwrap_or_default();
                assert!(
                    first_line.starts_with("HTTP/"),
                    "a partial or non-HTTP reply for {:?}: {response:?}",
                    String::from_utf8_lossy(case)
                );
            }
        }
    }

    // The server must still be serving.
    let response = hpx::get(format!("http://{}/health", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 200);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn concurrent_requests_are_all_served_correctly() -> Result<(), Box<dyn std::error::Error>> {
    const CONCURRENCY: usize = 32;

    let Some(server) = boot(base_config()).await? else {
        return Ok(());
    };

    let mut tasks = Vec::new();
    for index in 0..CONCURRENCY {
        let addr = server.http_addr;
        tasks.push(tokio::spawn(async move {
            let url = format!("http://{addr}/pets/{}", index + 1);
            let response = hpx::get(&url).send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|error| error.to_string())?.to_vec();
            Ok::<_, String>((status, body))
        }));
    }

    for (index, task) in tasks.into_iter().enumerate() {
        let Ok(joined) = tokio::time::timeout(Duration::from_secs(10), task).await else {
            return Err(format!("request {index} timed out").into());
        };
        let Ok(result) = joined else {
            return Err(format!("request {index} panicked").into());
        };
        let (status, body) = result.map_err(|error| format!("request {index} failed: {error}"))?;

        assert_eq!(status, 200, "request {index} failed");
        let pet: serde_json::Value = serde_json::from_slice(&body)?;
        assert!(
            pet.get("id").and_then(serde_json::Value::as_i64).is_some(),
            "request {index} returned a malformed pet: {pet}"
        );
    }

    server.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_rate_limit_is_enforced_after_the_burst_allowance()
-> Result<(), Box<dyn std::error::Error>> {
    // The limiter allows 100 messages per second per connection. Send far more
    // than that and confirm the overflow produces a structured rate_limit error
    // instead of tearing the connection down.
    const BURST: usize = 400;

    let config = ServerConfig { asyncapi_spec: Some(spec("asyncapi-chat.yaml")), ..base_config() };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.http_addr)).await?;
    let payload = r#"{"channel":"chat.send","payload":{"room":"r","text":"t"}}"#.to_owned();

    let mut rate_limited = false;
    for _ in 0..BURST {
        socket.send(WsMessage::Text(payload.clone().into())).await?;
        let Ok(Some(message)) = tokio::time::timeout(Duration::from_secs(5), socket.next()).await
        else {
            break;
        };
        let message = message?;
        let event: serde_json::Value = serde_json::from_str(message.to_text()?)?;
        if event["type"] == "error" && event["errors"][0]["keyword"] == "rate_limit" {
            rate_limited = true;
            break;
        }
    }

    assert!(
        rate_limited,
        "sending {BURST} messages in one second must trip the per-connection rate limit"
    );

    // The connection must survive a rate-limit rejection and keep processing
    // messages once the one-second window rolls over.
    loop {
        socket.send(WsMessage::Text(r#"not json"#.to_owned().into())).await?;
        let message =
            socket.next().await.ok_or("expected the connection to stay open")??.into_text()?;
        let event: serde_json::Value = serde_json::from_str(&message)?;

        match event["errors"][0]["keyword"].as_str() {
            Some("json") => break,
            Some("rate_limit") => continue,
            other => return Err(format!("unexpected keyword {other:?} in {event}").into()),
        }
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_binary_and_ping_frames_are_ignored_without_error()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { asyncapi_spec: Some(spec("asyncapi-chat.yaml")), ..base_config() };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.http_addr)).await?;

    // Binary frames carry no text payload, so the loop skips them silently.
    socket.send(WsMessage::Binary(vec![1, 2, 3].into())).await?;

    // A following text frame must still be answered.
    socket
        .send(WsMessage::Text(
            r#"{"channel":"chat.send","payload":{"room":"r","text":"t"}}"#.to_owned().into(),
        ))
        .await?;

    let message = socket
        .next()
        .await
        .ok_or("expected a reply after the ignored binary frame")??
        .into_text()?;
    let event: serde_json::Value = serde_json::from_str(&message)?;
    assert_eq!(event["type"], "mock", "got {event}");

    server.shutdown().await;
    Ok(())
}

/// A channel-pinned path must wrap the payload in the channel envelope, and a
/// payload that is not valid JSON must fall through to the structured parse
/// error rather than being silently dropped.
#[tokio::test]
async fn ws_pinned_channel_wraps_json_and_reports_non_json_payloads()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        asyncapi_spec: Some(spec("asyncapi-chat.yaml")),
        ..ServerConfig { openapi_spec: None, ..base_config() }
    };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let (mut socket, _) = connect_async(format!("ws://{}/ws/chat.send", server.http_addr)).await?;

    // A well-formed payload is wrapped in the pinned channel envelope.
    socket.send(WsMessage::Text(r#"{"room":"lobby","text":"hi"}"#.to_owned().into())).await?;
    let reply = socket.next().await.ok_or("expected a reply")??.into_text()?;
    let event: serde_json::Value = serde_json::from_str(&reply)?;
    assert_ne!(event["type"], "error", "a conforming payload must be accepted: {event}");

    // A payload that is not JSON cannot be wrapped, so it is handed to the
    // channel handler as-is and reported as a parse error.
    socket.send(WsMessage::Text("this is not json".to_owned().into())).await?;
    let reply = socket.next().await.ok_or("expected a reply")??.into_text()?;
    let event: serde_json::Value = serde_json::from_str(&reply)?;
    assert_eq!(event["type"], "error", "got {event}");
    assert_eq!(event["errors"][0]["keyword"], "json", "got {event}");

    server.shutdown().await;
    Ok(())
}

/// Pointing the WebSocket endpoint at an OpenAPI-only spec must refuse the
/// upgrade with 404 rather than accepting a socket it can never serve.
#[tokio::test]
async fn ws_upgrade_without_an_asyncapi_spec_is_refused() -> Result<(), Box<dyn std::error::Error>>
{
    let config = ServerConfig { asyncapi_spec: None, ..base_config() };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let error = connect_async(format!("ws://{}/ws", server.http_addr))
        .await
        .err()
        .ok_or("expected the upgrade to be refused")?;
    let rendered = error.to_string();
    assert!(
        rendered.contains("404") || rendered.contains("Not Found"),
        "expected a 404 refusal, got: {rendered}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ws_unknown_channel_and_bad_json_both_produce_structured_errors()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { asyncapi_spec: Some(spec("asyncapi-chat.yaml")), ..base_config() };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.http_addr)).await?;

    for (message, expected_keyword) in [
        (r#"{"channel":"no.such.channel","payload":{}}"#, "enum"),
        (r#"{"garbage":"payload"}"#, "routing"),
        ("definitely not json", "json"),
    ] {
        socket.send(WsMessage::Text(message.to_owned().into())).await?;
        let reply = socket.next().await.ok_or("expected a reply")??.into_text()?;
        let event: serde_json::Value = serde_json::from_str(&reply)?;
        assert_eq!(event["type"], "error", "for {message}");
        assert_eq!(event["errors"][0]["keyword"], expected_keyword, "for {message}: {event}");
        assert!(event["errors"][0]["instance_pointer"].is_string(), "for {message}");
        assert!(event["errors"][0]["schema_pointer"].is_string(), "for {message}");
    }

    server.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// gRPC error paths
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Configuration matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn asyncapi_only_serves_websocket_but_reports_no_http_operations()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: None,
        asyncapi_spec: Some(spec("asyncapi-chat.yaml")),
        ..ServerConfig {
            http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..ServerConfig::default()
        }
    };

    let Some(server) = boot(config).await? else {
        return Ok(());
    };

    let response = hpx::get(format!("http://{}/anything", server.http_addr)).send().await?;
    assert_eq!(
        response.status().as_u16(),
        404,
        "with no OpenAPI spec every HTTP request is unroutable"
    );
    let problem = json_body(&response.bytes().await?)?;
    assert_eq!(problem["detail"], "no OpenAPI runtime configured");

    // The WebSocket runtime is still active.
    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.http_addr)).await?;
    socket
        .send(WsMessage::Text(
            r#"{"channel":"chat.send","payload":{"room":"r","text":"t"}}"#.to_owned().into(),
        ))
        .await?;
    let message = socket.next().await.ok_or("expected a ws reply")??;
    let event: serde_json::Value = serde_json::from_str(message.to_text()?)?;
    assert_eq!(event["type"], "mock");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_invalid_spec_file_fails_startup_rather_than_serving_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    // A spec path that exists but is not a valid OpenAPI document.
    let config = ServerConfig { openapi_spec: Some(spec("greeter.proto")), ..base_config() };

    let result = start(config).await;
    assert!(result.is_err(), "an unparsable OpenAPI document must fail startup");
    Ok(())
}

#[tokio::test]
async fn malformed_spec_files_are_reported_with_a_parse_error()
-> Result<(), Box<dyn std::error::Error>> {
    // An AsyncAPI document has no `openapi` version field.
    let as_openapi =
        ServerConfig { openapi_spec: Some(spec("asyncapi-chat.yaml")), ..base_config() };
    let error = start(as_openapi).await.err().ok_or("startup should fail")?;
    assert!(error.to_string().contains("openapi version field missing"), "got {error}");

    // An OpenAPI document has no `channels` object.
    let as_asyncapi = ServerConfig {
        openapi_spec: None,
        asyncapi_spec: Some(spec("openapi-pets.yaml")),
        ..base_config()
    };
    let error = start(as_asyncapi).await.err().ok_or("startup should fail")?;
    assert!(error.to_string().contains("channels"), "got {error}");

    Ok(())
}

#[tokio::test]
async fn a_spec_path_colliding_with_the_websocket_base_path_fails_startup()
-> Result<(), Box<dyn std::error::Error>> {
    // `openapi-ws-conflict.yaml` declares `GET /ws`, which is the default
    // WebSocket base path. WebSocket paths are explicit axum routes and
    // OpenAPI paths are served by the fallback, so without a startup check the
    // declared operation silently becomes unreachable.
    let config =
        ServerConfig { openapi_spec: Some(spec("openapi-ws-conflict.yaml")), ..base_config() };

    let error = start(config).await.err().ok_or("startup should fail")?;
    let message = error.to_string();
    assert!(message.contains("/ws"), "the error must name the colliding path: {message}");
    assert!(
        message.contains("WebSocket"),
        "the error must name the colliding route family: {message}"
    );

    Ok(())
}

#[tokio::test]
async fn a_spec_path_colliding_with_a_websocket_channel_path_fails_startup()
-> Result<(), Box<dyn std::error::Error>> {
    // `openapi-ws-channel-conflict.yaml` declares only the template
    // `/ws/{name}`, so the WebSocket base path `/ws` is clear but the AsyncAPI
    // channel `chat.send` at `/ws/chat.send` is not.
    let config = ServerConfig {
        openapi_spec: Some(spec("openapi-ws-channel-conflict.yaml")),
        asyncapi_spec: Some(spec("asyncapi-chat.yaml")),
        ..base_config()
    };

    let error = start(config).await.err().ok_or("startup should fail")?;
    let message = error.to_string();
    assert!(
        message.contains("/ws/{name}") && message.contains("/ws/chat.send"),
        "the error must name both the declared template and the colliding channel path: {message}"
    );

    Ok(())
}

#[tokio::test]
async fn a_non_colliding_websocket_base_path_still_starts() -> Result<(), Box<dyn std::error::Error>>
{
    // A base path that matches neither declared route leaves startup working.
    let config = ServerConfig {
        openapi_spec: Some(spec("openapi-ws-conflict.yaml")),
        asyncapi_spec: Some(spec("asyncapi-chat.yaml")),
        ws_path: "/events".to_owned(),
        ..base_config()
    };

    let server = boot(config).await?;
    if let Some(server) = server {
        server.shutdown().await;
    }

    Ok(())
}
