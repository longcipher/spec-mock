//! Integration tests for OpenAPI HTTP runtime behavior.

use std::{net::SocketAddr, path::PathBuf};

use specmock_core::MockMode;
use specmock_runtime::{RuntimeError, ServerConfig, start};

fn openapi_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join("openapi-pets.yaml")
}

fn openapi_multifile_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-multifile")
        .join("api.yaml")
}

fn openapi_negotiate_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-negotiate.yaml")
}

fn openapi_array_params_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-array-params.yaml")
}

#[tokio::test]
async fn invalid_request_returns_400_with_pointer_details() -> Result<(), Box<dyn std::error::Error>>
{
    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
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

    let response = hpx::get(format!("http://{}/pets/abc", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 400);
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    // RFC 7807 envelope
    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("about:blank"));
    assert_eq!(body.get("title").and_then(serde_json::Value::as_str), Some("Bad Request"));
    assert_eq!(body.get("status").and_then(serde_json::Value::as_u64), Some(400));

    let first_error = body
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .and_then(|errors| errors.first())
        .cloned();
    let first_error = first_error.ok_or("expected at least one validation error")?;
    assert!(first_error.get("instance_pointer").is_some(), "missing instance_pointer");
    assert!(first_error.get("schema_pointer").is_some(), "missing schema_pointer");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn valid_request_returns_schema_conforming_mock_response()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
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

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200);
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert!(body.get("id").and_then(serde_json::Value::as_i64).is_some());
    assert!(body.get("name").and_then(serde_json::Value::as_str).is_some());

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn multi_file_openapi_resolves_and_serves() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_multifile_spec_path()),
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

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200, "expected 200, got {status}");
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    // The schema was defined in a separate file (schemas/pet.yaml) and resolved via $ref.
    assert!(body.get("id").and_then(serde_json::Value::as_i64).is_some(), "missing id field");
    assert!(body.get("name").and_then(serde_json::Value::as_str).is_some(), "missing name field");

    server.shutdown().await;
    Ok(())
}

// ── Content negotiation / Prefer header ────────────────────────────────

async fn start_negotiate_server() -> Result<specmock_runtime::RunningServer, RuntimeError> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_negotiate_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };
    start(config).await
}

#[tokio::test]
async fn prefer_code_selects_404_response() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Prefer", "code=404")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 404);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(body.get("error").and_then(serde_json::Value::as_str), Some("pet not found"));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn prefer_code_selects_500_response() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Prefer", "code=500")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 500);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(
        body.get("error").and_then(serde_json::Value::as_str),
        Some("internal server error")
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn prefer_example_selects_named_example() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Prefer", "example=whiskers")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(body.get("id").and_then(serde_json::Value::as_i64), Some(7));
    assert_eq!(body.get("name").and_then(serde_json::Value::as_str), Some("Whiskers"));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn prefer_code_and_example_combined() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Select 200 explicitly and pick the "fluffy" named example.
    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Prefer", "code=200, example=fluffy")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(body.get("id").and_then(serde_json::Value::as_i64), Some(42));
    assert_eq!(body.get("name").and_then(serde_json::Value::as_str), Some("Fluffy"));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn prefer_dynamic_returns_generated_body() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Prefer", "dynamic=true")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    // dynamic=true should still produce a conforming object with id and name
    assert!(body.get("id").and_then(serde_json::Value::as_i64).is_some(), "missing id");
    assert!(body.get("name").and_then(serde_json::Value::as_str).is_some(), "missing name");
    // The dynamic faker result should differ from the named examples
    let name = body.get("name").and_then(serde_json::Value::as_str).unwrap_or_default();
    assert_ne!(name, "Fluffy", "dynamic should not return the static example");
    assert_ne!(name, "Whiskers", "dynamic should not return the static example");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn default_response_without_prefer_returns_200() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_negotiate_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 200);

    server.shutdown().await;
    Ok(())
}

// ── Multi-value query parameters ───────────────────────────────────────

async fn start_array_params_server() -> Result<specmock_runtime::RunningServer, RuntimeError> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_array_params_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };
    start(config).await
}

#[tokio::test]
async fn multi_value_query_param_accepted_for_array_schema()
-> Result<(), Box<dyn std::error::Error>> {
    let server = match start_array_params_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // ?tag=dog&tag=cat — both values should be preserved as an array.
    let response =
        hpx::get(format!("http://{}/search?tag=dog&tag=cat", server.http_addr)).send().await?;
    let status = response.status().as_u16();

    assert_eq!(status, 200, "expected 200 for valid array query params, got {status}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn single_value_query_param_accepted_for_array_schema()
-> Result<(), Box<dyn std::error::Error>> {
    let server = match start_array_params_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Single value should still form a valid one-element array.
    let response = hpx::get(format!("http://{}/search?tag=dog", server.http_addr)).send().await?;
    let status = response.status().as_u16();

    assert_eq!(status, 200, "expected 200 for single-element array query, got {status}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_required_array_query_param_returns_400() -> Result<(), Box<dyn std::error::Error>>
{
    let server = match start_array_params_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // No tag parameter at all — should be rejected.
    let response = hpx::get(format!("http://{}/search", server.http_addr)).send().await?;
    let status = response.status().as_u16();

    assert_eq!(status, 400, "expected 400 for missing required array param, got {status}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn non_array_query_param_uses_first_value() -> Result<(), Box<dyn std::error::Error>> {
    let server = match start_array_params_server().await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Duplicate non-array param "limit" — first value (5) should be used for validation.
    let response = hpx::get(format!("http://{}/search?tag=dog&limit=5&limit=10", server.http_addr))
        .send()
        .await?;
    let status = response.status().as_u16();

    assert_eq!(status, 200, "expected 200 when first non-array value is valid, got {status}");

    server.shutdown().await;
    Ok(())
}

// ── Body size limit and Content-Type validation ────────────────────────

fn openapi_body_limit_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-body-limit.yaml")
}

#[tokio::test]
async fn large_body_returns_413_payload_too_large() -> Result<(), Box<dyn std::error::Error>> {
    // Use a tiny max_body_size so we can trigger 413 without allocating megabytes.
    let config = ServerConfig {
        openapi_spec: Some(openapi_body_limit_spec_path()),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        max_body_size: 64,
        ..ServerConfig::default()
    };

    let server = match start(config).await {
        Ok(value) => value,
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(());
        }
        Err(error) => return Err(error.to_string().into()),
    };

    // Send a body larger than 64 bytes.
    let oversized = "x".repeat(128);
    let response = hpx::Client::new()
        .post(format!("http://{}/items", server.http_addr))
        .header("Content-Type", "application/json")
        .body(oversized)
        .send()
        .await?;

    assert_eq!(
        response.status().as_u16(),
        413,
        "expected 413 Payload Too Large for oversized body"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn wrong_content_type_returns_415_unsupported_media_type()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_body_limit_spec_path()),
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

    // POST with text/plain Content-Type on an endpoint that expects application/json.
    let response = hpx::Client::new()
        .post(format!("http://{}/items", server.http_addr))
        .header("Content-Type", "text/plain")
        .body(r#"{"name":"test"}"#)
        .send()
        .await?;

    assert_eq!(
        response.status().as_u16(),
        415,
        "expected 415 Unsupported Media Type for wrong Content-Type"
    );
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    // RFC 7807 envelope
    assert_eq!(body.get("type").and_then(serde_json::Value::as_str), Some("about:blank"));
    assert_eq!(
        body.get("title").and_then(serde_json::Value::as_str),
        Some("Unsupported Media Type")
    );
    assert_eq!(body.get("status").and_then(serde_json::Value::as_u64), Some(415));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn malformed_json_body_returns_400_bad_request() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_body_limit_spec_path()),
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

    let response = hpx::Client::new()
        .post(format!("http://{}/items", server.http_addr))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"oops""#)
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 400, "expected 400 Bad Request for malformed JSON body");

    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(body.get("status").and_then(serde_json::Value::as_u64), Some(400));
    assert_eq!(body.pointer("/errors/0/keyword").and_then(serde_json::Value::as_str), Some("json"));

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn correct_content_type_post_returns_success() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_body_limit_spec_path()),
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

    // POST with correct Content-Type should succeed.
    let response = hpx::Client::new()
        .post(format!("http://{}/items", server.http_addr))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"test"}"#)
        .send()
        .await?;

    let status = response.status().as_u16();
    assert_eq!(status, 201, "expected 201 for valid POST, got {status}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn empty_body_skips_content_type_check() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_body_limit_spec_path()),
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

    // GET with no body should not trigger Content-Type validation even though
    // the same spec has POST endpoints with request body schemas.
    let response = hpx::get(format!("http://{}/items", server.http_addr)).send().await?;

    let status = response.status().as_u16();
    assert_eq!(status, 200, "expected 200 for GET, got {status}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn proxy_mode_without_upstream_returns_config_error() {
    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
        mode: MockMode::Proxy,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };

    let result = start(config).await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Config(ref message))
                if message.contains("proxy mode requires upstream")
        ),
        "expected proxy config error, got {result:?}"
    );
}

fn openapi_callbacks_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-callbacks.yaml")
}

fn openapi_polymorphic_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-polymorphic.yaml")
}

fn openapi_content_types_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("specs")
        .join("openapi-content-types.yaml")
}

#[tokio::test]
async fn callbacks_are_fired_after_mock_response() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use tokio::sync::Notify;

    // 1. Start a small capture server that records incoming callback requests.
    let callback_received = Arc::new(Notify::new());
    let callback_received_clone = Arc::clone(&callback_received);

    let capture_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let capture_addr = capture_listener.local_addr()?;

    let capture_task = tokio::spawn(async move {
        // Accept a single connection, read enough to confirm a POST arrived,
        // then signal via Notify.
        if let Ok((mut stream, _addr)) = capture_listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            // Send a minimal HTTP 200 response so the client doesn't error.
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let _w = tokio::io::AsyncWriteExt::write_all(&mut stream, response).await;
            callback_received_clone.notify_one();
        }
    });

    // 2. Start spec-mock with the callbacks spec.
    let config = ServerConfig {
        openapi_spec: Some(openapi_callbacks_spec_path()),
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

    // 3. POST to /subscribe with a callbackUrl pointing to our capture server.
    let callback_url = format!("http://{capture_addr}");
    let body = serde_json::json!({ "callbackUrl": callback_url });
    let response = hpx::Client::new()
        .post(format!("http://{}/subscribe", server.http_addr))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await?;

    let status = response.status().as_u16();
    assert_eq!(status, 201, "expected 201 for subscribe, got {status}");

    // 4. Wait for the callback to arrive (with timeout).
    let received =
        tokio::time::timeout(std::time::Duration::from_secs(5), callback_received.notified()).await;
    assert!(received.is_ok(), "callback was not received within timeout");

    capture_task.abort();
    server.shutdown().await;
    Ok(())
}

// ── Polymorphic (oneOf + discriminator) ────────────────────────────────

#[tokio::test]
async fn polymorphic_discriminator_returns_valid_shape() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_polymorphic_spec_path()),
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

    let response = hpx::get(format!("http://{}/shapes/1", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200, "expected 200 for GET /shapes/1, got {status}");
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    // The mock response should be a valid oneOf variant with a discriminator property
    assert!(body.is_object(), "expected an object body");
    let shape_type = body.get("shapeType").and_then(serde_json::Value::as_str);
    assert!(shape_type.is_some(), "missing shapeType discriminator field");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn polymorphic_list_returns_array_of_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_polymorphic_spec_path()),
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

    let response = hpx::get(format!("http://{}/shapes", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200, "expected 200 for GET /shapes, got {status}");
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert!(body.is_array(), "expected an array body for list endpoint");

    server.shutdown().await;
    Ok(())
}

// ── Multiple content types per response ────────────────────────────────

#[tokio::test]
async fn content_types_returns_json_by_default() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_content_types_spec_path()),
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

    let response = hpx::get(format!("http://{}/report", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200, "expected 200 for GET /report, got {status}");
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    // Default content negotiation should produce JSON
    assert!(body.get("title").and_then(serde_json::Value::as_str).is_some(), "missing title");
    assert!(body.get("data").and_then(serde_json::Value::as_array).is_some(), "missing data");

    server.shutdown().await;
    Ok(())
}

// ── Proxy strips sensitive headers ─────────────────────────────────────

#[tokio::test]
async fn proxy_strips_auth_headers() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Start a mock upstream that echoes received headers back in the body.
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream_addr = upstream_listener.local_addr()?;

    let upstream_task = tokio::spawn(async move {
        let (mut stream, _addr) = upstream_listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await.expect("read");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();

        let body = r#"{"id":1,"name":"mock"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await.expect("write");
        request
    });

    // 2. Start spec-mock in proxy mode.
    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
        mode: MockMode::Proxy,
        upstream: Some(format!("http://{upstream_addr}")),
        allow_private_upstream: true,
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

    // 3. Send request with sensitive headers.
    let response = hpx::get(format!("http://{}/pets/1", server.http_addr))
        .header("Authorization", "Bearer secret-token")
        .header("Cookie", "session=abc")
        .header("Proxy-Authorization", "Basic proxy-secret")
        .send()
        .await?;

    assert_eq!(response.status().as_u16(), 200);

    // 4. Verify upstream did NOT receive sensitive headers.
    let upstream_request = tokio::time::timeout(std::time::Duration::from_secs(5), upstream_task)
        .await
        .expect("upstream task timeout")
        .expect("upstream task join");

    let lower = upstream_request.to_ascii_lowercase();
    assert!(!lower.contains("authorization:"), "upstream should not receive Authorization header");
    assert!(!lower.contains("cookie:"), "upstream should not receive Cookie header");
    assert!(
        !lower.contains("proxy-authorization:"),
        "upstream should not receive Proxy-Authorization header"
    );

    server.shutdown().await;
    Ok(())
}

// ── Proxy forwards upstream responses ─────────────────────────────────

#[tokio::test]
async fn proxy_forwards_request_and_returns_upstream_response()
-> Result<(), Box<dyn std::error::Error>> {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream_addr = upstream_listener.local_addr()?;

    let upstream_task = tokio::spawn(async move {
        let (mut stream, _addr) = upstream_listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await.expect("read");
        let body = r#"{"id":99,"name":"upstream-pet"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await.expect("write");
    });

    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
        mode: MockMode::Proxy,
        upstream: Some(format!("http://{upstream_addr}")),
        allow_private_upstream: true,
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

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    assert_eq!(body.get("name").and_then(serde_json::Value::as_str), Some("upstream-pet"));

    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), upstream_task).await;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn proxy_returns_502_when_upstream_connection_refused()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_spec_path()),
        mode: MockMode::Proxy,
        upstream: Some("http://127.0.0.1:1".into()),
        allow_private_upstream: true,
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

    let response = hpx::get(format!("http://{}/pets/1", server.http_addr)).send().await?;
    assert_eq!(response.status().as_u16(), 502);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn content_types_health_check() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(openapi_content_types_spec_path()),
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

    let response = hpx::get(format!("http://{}/health", server.http_addr)).send().await?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await?;

    assert_eq!(status, 200, "expected 200 for GET /health, got {status}");
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body.get("status").and_then(serde_json::Value::as_str), Some("ok"));

    server.shutdown().await;
    Ok(())
}
