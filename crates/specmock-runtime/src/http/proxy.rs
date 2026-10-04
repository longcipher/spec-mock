//! Proxy handler for forwarding requests to upstream servers.

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use serde_json::Value;
use specmock_core::{ValidationIssue, validate::validate_instance};

use super::{HttpRuntime, error_response, header_is_json};

const HEADER_HOST: &str = "host";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_AUTHORIZATION: &str = "authorization";
const HEADER_COOKIE: &str = "cookie";
const HEADER_PROXY_AUTHORIZATION: &str = "proxy-authorization";

/// Proxy a request to the upstream server and validate the response.
pub async fn proxy_request(
    runtime: &HttpRuntime,
    upstream: &url::Url,
    method: &Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body_bytes: &[u8],
    matched: &super::openapi::MatchedOperation<'_>,
) -> Response {
    let target_url = format!(
        "{}{}{}",
        upstream.as_str().trim_end_matches('/'),
        uri.path(),
        uri.query().map_or_else(String::new, |query| format!("?{query}"))
    );

    let mut request_builder =
        runtime.client.request(method.clone(), target_url).body(body_bytes.to_vec());
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == HEADER_HOST ||
            lower == HEADER_CONTENT_LENGTH ||
            lower == HEADER_AUTHORIZATION ||
            lower == HEADER_COOKIE ||
            lower == HEADER_PROXY_AUTHORIZATION
        {
            continue;
        }
        request_builder = request_builder.header(name, value);
    }

    // Set Host header from upstream URL so the proxy target receives the
    // correct virtual-host identity.
    if let Some(host) = upstream.host_str() {
        let host_value = if let Some(port) = upstream.port() {
            format!("{host}:{port}")
        } else {
            host.to_owned()
        };
        request_builder = request_builder.header("Host", host_value);
    }

    let upstream_response = match request_builder.send().await {
        Ok(response) => response,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                vec![ValidationIssue {
                    instance_pointer: "/proxy".to_owned(),
                    schema_pointer: "#".to_owned(),
                    keyword: "proxy".to_owned(),
                    message: format!("upstream request failed: {error}"),
                }],
            );
        }
    };

    let status = upstream_response.status();
    let response_headers = upstream_response.headers().clone();
    let response_bytes = match upstream_response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                vec![ValidationIssue {
                    instance_pointer: "/response/body".to_owned(),
                    schema_pointer: "#".to_owned(),
                    keyword: "proxy".to_owned(),
                    message: format!("failed to read upstream response body: {error}"),
                }],
            );
        }
    };

    if let Some(schema) = matched.operation.response_schema_for_status(status.as_u16()) &&
        header_is_json(&response_headers)
    {
        match serde_json::from_slice::<Value>(&response_bytes) {
            Ok(response_json) => match validate_instance(schema, &response_json) {
                Ok(issues) if !issues.is_empty() => {
                    return error_response(StatusCode::BAD_GATEWAY, issues);
                }
                Ok(_issues) => {}
                Err(error) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        vec![ValidationIssue {
                            instance_pointer: "/response".to_owned(),
                            schema_pointer: "#/responses".to_owned(),
                            keyword: "schema".to_owned(),
                            message: error.to_string(),
                        }],
                    );
                }
            },
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    vec![ValidationIssue {
                        instance_pointer: "/response/body".to_owned(),
                        schema_pointer: "#/responses".to_owned(),
                        keyword: "json".to_owned(),
                        message: format!("upstream response is not valid json: {error}"),
                    }],
                );
            }
        }
    }

    let mut builder = Response::builder().status(status);
    if let Some(target_headers) = builder.headers_mut() {
        for (name, value) in &response_headers {
            target_headers.append(name, value.clone());
        }
    }
    builder.body(Body::from(response_bytes)).unwrap_or_else(|_error| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use axum::{
        body::to_bytes,
        http::{HeaderMap, StatusCode},
    };
    use serde_json::Value;
    use specmock_core::MockMode;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::oneshot,
    };

    use super::{HttpRuntime, proxy_request};
    use crate::{RuntimeError, ServerConfig, http::openapi::MatchedOperation};

    fn spec_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("specs")
            .join("openapi-pets.yaml")
    }

    /// Build a mock runtime pointed at `upstream`, plus a matched GET /pets/{id}.
    ///
    /// `match_operation` lives on `OpenApiRuntime`, not on `HttpRuntime`, so the
    /// OpenAPI runtime is loaded separately and leaked to keep the returned
    /// `MatchedOperation` borrowing a `'static` reference.
    async fn runtime_and_match(
        upstream: &str,
    ) -> Result<(HttpRuntime, MatchedOperation<'static>), RuntimeError> {
        let config = ServerConfig {
            openapi_spec: Some(spec_path()),
            mode: MockMode::Proxy,
            upstream: Some(upstream.to_owned()),
            allow_private_upstream: true,
            ..ServerConfig::default()
        };
        let runtime = HttpRuntime::from_config(&config).await?;

        let openapi = crate::http::openapi::OpenApiRuntime::from_path(&spec_path())
            .map_err(|error| RuntimeError::Parse(error.to_string()))?;
        let openapi: &'static crate::http::openapi::OpenApiRuntime = Box::leak(Box::new(openapi));
        let matched = openapi
            .match_operation(&axum::http::Method::GET, "/pets/1")
            .ok_or_else(|| RuntimeError::Config("expected /pets/{id} to be matched".to_owned()))?;

        Ok((runtime, matched))
    }

    /// Start a one-shot HTTP/1.1 server that replies with `raw` then closes.
    async fn spawn_raw_upstream(
        raw: &'static [u8],
    ) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut stream, _peer)) = listener.accept().await {
                // Drain the request head so the client can finish writing.
                let mut buffer = vec![0_u8; 8192];
                let _read = AsyncReadExt::read(&mut stream, &mut buffer).await;
                let _write = AsyncWriteExt::write_all(&mut stream, raw).await;
                AsyncWriteExt::flush(&mut stream).await.ok();
            }
        });
        Ok(addr)
    }

    /// Decode a response into `(status, headers, body bytes)`.
    async fn decode(response: axum::response::Response) -> (StatusCode, HeaderMap, Vec<u8>) {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .map(|value| value.to_vec())
            .unwrap_or_default();
        (status, headers, bytes)
    }

    fn json_response(body: &'static str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    // ── happy path ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn forwards_to_upstream_and_returns_response() {
        let upstream = spawn_raw_upstream(Box::leak(
            json_response(r#"{"id":7,"name":"upstream"}"#).into_boxed_slice(),
        ))
        .await
        .expect("upstream should start");

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, headers, body) = decode(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").map(|v| v.to_str().unwrap_or_default()),
            Some("application/json")
        );
        let parsed: Value = serde_json::from_slice(&body).expect("upstream body should be json");
        assert_eq!(parsed["name"], "upstream");
    }

    #[tokio::test]
    async fn query_string_is_forwarded_to_upstream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 8192];
            let read = AsyncReadExt::read(&mut stream, &mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let raw = json_response(r#"{"id":1,"name":"q"}"#);
            let _write = AsyncWriteExt::write_all(&mut stream, &raw).await;
            let _sent = seen_tx.send(request);
        });

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let uri: axum::http::Uri = "/pets/1?include=owner&limit=5".parse().expect("uri");
        let _response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &uri,
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .expect("upstream must be contacted")
            .expect("request captured");
        assert!(request.contains("include=owner"), "query was dropped: {request}");
        assert!(request.contains("limit=5"), "query was dropped: {request}");
    }

    #[tokio::test]
    async fn trailing_slash_in_upstream_does_not_double_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 8192];
            let read = AsyncReadExt::read(&mut stream, &mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let raw = json_response(r#"{"id":1,"name":"x"}"#);
            let _write = AsyncWriteExt::write_all(&mut stream, &raw).await;
            let _sent = seen_tx.send(request);
        });

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}/")).await.expect("runtime should build");

        let _response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}/")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .expect("upstream contacted")
            .expect("captured");
        let request_line = request.lines().next().unwrap_or_default();
        assert_eq!(request_line, "GET /pets/1 HTTP/1.1", "path must not contain a doubled slash");
    }

    // ── header hygiene ─────────────────────────────────────────────────

    #[tokio::test]
    async fn hop_by_hop_and_credential_headers_are_stripped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 16384];
            let read = AsyncReadExt::read(&mut stream, &mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let raw = json_response(r#"{"id":1,"name":"x"}"#);
            let _write = AsyncWriteExt::write_all(&mut stream, &raw).await;
            let _sent = seen_tx.send(request);
        });

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().expect("value"));
        headers.insert("cookie", "session=abc".parse().expect("value"));
        headers.insert("proxy-authorization", "Basic pw".parse().expect("value"));
        headers.insert("x-trace-id", "trace-1".parse().expect("value"));
        headers.insert("accept", "application/json".parse().expect("value"));

        let _response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &headers,
            b"",
            &matched,
        )
        .await;

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .expect("upstream contacted")
            .expect("captured");
        let lower = request.to_ascii_lowercase();

        for forbidden in ["authorization:", "cookie:", "proxy-authorization:", "content-length:"] {
            assert!(!lower.contains(forbidden), "{forbidden} must not be forwarded: {request}");
        }
        assert!(lower.contains("x-trace-id:"), "end-to-end headers must be forwarded: {request}");
        assert!(lower.contains("accept:"), "negotiation headers must be forwarded: {request}");
    }

    #[tokio::test]
    async fn host_header_is_rewritten_to_the_upstream_authority() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 8192];
            let read = AsyncReadExt::read(&mut stream, &mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let raw = json_response(r#"{"id":1,"name":"x"}"#);
            let _write = AsyncWriteExt::write_all(&mut stream, &raw).await;
            let _sent = seen_tx.send(request);
        });

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        // Send a Host header naming the *mock* server; the proxy must replace
        // it with the upstream authority so the target sees a correct vhost.
        let mut headers = HeaderMap::new();
        headers.insert("host", "spec-mock.invalid:9999".parse().expect("value"));

        let _response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &headers,
            b"",
            &matched,
        )
        .await;

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .expect("upstream contacted")
            .expect("captured");
        let lower = request.to_ascii_lowercase();
        assert!(
            !lower.contains("spec-mock.invalid"),
            "the client's Host header must not reach the upstream: {request}"
        );
        assert!(
            lower.contains(&format!("host: {upstream}").to_ascii_lowercase()) ||
                lower.contains(&format!("host: 127.0.0.1:{}", upstream.port())),
            "the upstream authority must be sent as Host: {request}"
        );
    }

    // ── transport failures ─────────────────────────────────────────────

    #[tokio::test]
    async fn unreachable_upstream_returns_502() {
        let (runtime, matched) =
            runtime_and_match("http://127.0.0.1:1").await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse("http://127.0.0.1:1").expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, _headers, body) = decode(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let problem: Value = serde_json::from_slice(&body).expect("problem json");
        assert_eq!(problem["status"], 502);
        let issue = &problem["errors"][0];
        assert_eq!(issue["keyword"], "proxy");
        assert_eq!(issue["instance_pointer"], "/proxy");
    }

    #[tokio::test]
    async fn truncated_upstream_body_returns_502() {
        // Announce far more bytes than we actually send, then close.
        let raw: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{\"id\"";
        let upstream = spawn_raw_upstream(raw).await.expect("upstream should start");

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, _headers, _body) = decode(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "a truncated body must not be relayed");
    }

    // ── response validation ────────────────────────────────────────────

    #[tokio::test]
    async fn non_json_response_body_skips_schema_validation() {
        let raw: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nabc";
        let upstream = spawn_raw_upstream(raw).await.expect("upstream should start");

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, _headers, body) = decode(response).await;
        assert_eq!(status, StatusCode::OK, "non-JSON upstream payloads are relayed verbatim");
        assert_eq!(body, b"abc");
    }

    #[tokio::test]
    async fn invalid_json_with_json_content_type_returns_502() {
        let raw: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n{{{";
        let upstream = spawn_raw_upstream(raw).await.expect("upstream should start");

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, _headers, body) = decode(response).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let problem: Value = serde_json::from_slice(&body).expect("problem json");
        assert_eq!(problem["errors"][0]["keyword"], "json");
        assert_eq!(problem["errors"][0]["instance_pointer"], "/response/body");
    }

    // ── status passthrough ─────────────────────────────────────────────

    #[tokio::test]
    async fn upstream_status_and_headers_are_relayed() {
        let raw: &'static [u8] =
            b"HTTP/1.1 503 Service Unavailable\r\nX-Upstream: yes\r\nContent-Length: 0\r\n\r\n";
        let upstream = spawn_raw_upstream(raw).await.expect("upstream should start");

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::GET,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            b"",
            &matched,
        )
        .await;

        let (status, headers, body) = decode(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers.get("x-upstream").map(|v| v.to_str().unwrap_or_default()), Some("yes"));
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn request_body_is_forwarded_upstream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.expect("accept");
            let mut buffer = vec![0_u8; 16384];
            let read = AsyncReadExt::read(&mut stream, &mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let raw = json_response(r#"{"id":1,"name":"echo"}"#);
            let _write = AsyncWriteExt::write_all(&mut stream, &raw).await;
            let _sent = seen_tx.send(request);
        });

        let (runtime, matched) =
            runtime_and_match(&format!("http://{upstream}")).await.expect("runtime should build");

        let payload = br#"{"sku":"A-1"}"#.to_vec();
        let _response = proxy_request(
            &runtime,
            &url::Url::parse(&format!("http://{upstream}")).expect("url"),
            &axum::http::Method::POST,
            &"/pets/1".parse().expect("uri"),
            &HeaderMap::new(),
            &payload,
            &matched,
        )
        .await;

        let request = tokio::time::timeout(std::time::Duration::from_secs(5), seen_rx)
            .await
            .expect("upstream contacted")
            .expect("captured");
        assert!(
            request.contains(r#"{"sku":"A-1"}"#),
            "the request body must reach the upstream: {request}"
        );
    }

    // ── schema lookup helper ───────────────────────────────────────────

    #[test]
    fn response_schema_lookup_falls_back_to_default() {
        let runtime: &'static crate::http::openapi::OpenApiRuntime = Box::leak(Box::new(
            crate::http::openapi::OpenApiRuntime::from_resolved(serde_json::json!({
                "openapi": "3.1.0",
                "paths": {
                    "/pets": {
                        "get": {
                            "responses": {
                                "default": {
                                    "content": {
                                        "application/json": {"schema": {"type": "object"}}
                                    }
                                }
                            }
                        }
                    }
                }
            }))
            .expect("runtime should parse"),
        ));

        let operation = runtime
            .match_operation(&axum::http::Method::GET, "/pets")
            .expect("operation should match")
            .operation;

        assert!(
            operation.response_schema_for_status(200).is_some(),
            "an undeclared status must fall back to the `default` response"
        );
        assert!(
            operation.response_schema_for_status(503).is_some(),
            "any status falls back to `default` when no exact match exists"
        );
    }

    #[test]
    fn response_schema_lookup_prefers_the_exact_status() {
        let runtime: &'static crate::http::openapi::OpenApiRuntime = Box::leak(Box::new(
            crate::http::openapi::OpenApiRuntime::from_resolved(serde_json::json!({
                "openapi": "3.1.0",
                "paths": {
                    "/pets": {
                        "get": {
                            "responses": {
                                "200": {
                                    "content": {
                                        "application/json": {
                                            "schema": {"type": "object", "title": "exact"}
                                        }
                                    }
                                },
                                "default": {
                                    "content": {
                                        "application/json": {"schema": {"type": "string"}}
                                    }
                                }
                            }
                        }
                    }
                }
            }))
            .expect("runtime should parse"),
        ));

        let operation = runtime
            .match_operation(&axum::http::Method::GET, "/pets")
            .expect("operation should match")
            .operation;

        let schema = operation.response_schema_for_status(200).expect("exact match");
        assert_eq!(schema["title"], "exact");
        assert_eq!(
            operation.response_schema_for_status(404).expect("default").get("type"),
            Some(&Value::from("string"))
        );
    }

    #[test]
    fn response_schema_lookup_returns_none_without_a_match() {
        let runtime: &'static crate::http::openapi::OpenApiRuntime = Box::leak(Box::new(
            crate::http::openapi::OpenApiRuntime::from_resolved(serde_json::json!({
                "openapi": "3.1.0",
                "paths": {
                    "/pets": {"get": {"responses": {"200": {"description": "ok"}}}}
                }
            }))
            .expect("runtime should parse"),
        ));

        let operation = runtime
            .match_operation(&axum::http::Method::GET, "/pets")
            .expect("operation should match")
            .operation;
        assert!(
            operation.response_schema_for_status(404).is_none(),
            "no exact match and no default means no schema to validate against"
        );
    }
}
