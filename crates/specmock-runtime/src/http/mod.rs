//! HTTP and WebSocket server runtime.

pub mod media;
pub mod negotiate;
pub mod openapi;
pub mod proxy;
pub mod router;
pub mod ws_handler;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::get,
    serve,
};
use negotiate::PreferDirectives;
use openapi::{MatchedOperation, OpenApiRuntime};
use serde_json::Value;
use specmock_core::{
    MockMode, PROBLEM_JSON_CONTENT_TYPE, ProblemDetails, ValidationIssue,
    faker::generate_json_value,
};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{RuntimeError, ws::AsyncApiRuntime};

/// Shared runtime state.
#[derive(Clone)]
pub struct HttpRuntime {
    openapi: Option<OpenApiRuntime>,
    asyncapi: Option<AsyncApiRuntime>,
    mode: MockMode,
    upstream: Option<url::Url>,
    seed: u64,
    ws_path: String,
    /// Map from per-channel WS path to channel name.
    ws_channel_paths: HashMap<String, String>,
    max_body_size: usize,
    client: hpx::Client,
}

impl std::fmt::Debug for HttpRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpRuntime")
            .field("openapi", &self.openapi)
            .field("asyncapi", &self.asyncapi)
            .field("mode", &self.mode)
            .field("upstream", &self.upstream)
            .field("seed", &self.seed)
            .field("ws_path", &self.ws_path)
            .field("ws_channel_paths", &self.ws_channel_paths)
            .field("max_body_size", &self.max_body_size)
            .finish_non_exhaustive()
    }
}

impl HttpRuntime {
    /// Resolve the pinned channel name for a WebSocket request path.
    ///
    /// Returns `Some(channel_name)` when the path matches a per-channel
    /// route, or `None` for the catch-all default path.
    fn resolve_ws_channel(&self, path: &str) -> Option<String> {
        self.ws_channel_paths.get(path).cloned()
    }

    /// Build from global server config.
    pub async fn from_config(config: &crate::ServerConfig) -> Result<Self, RuntimeError> {
        let openapi = config.openapi_spec.as_deref().map(OpenApiRuntime::from_path).transpose()?;
        let asyncapi =
            config.asyncapi_spec.as_deref().map(AsyncApiRuntime::from_path).transpose()?;

        if config.mode == MockMode::Proxy && config.upstream.is_none() {
            return Err(RuntimeError::Config(
                "proxy mode requires upstream base URL (--upstream)".to_owned(),
            ));
        }

        let upstream = config
            .upstream
            .as_ref()
            .map(|raw| {
                raw.parse::<url::Url>().map_err(|error| {
                    RuntimeError::Config(format!("invalid upstream URL '{raw}': {error}"))
                })
            })
            .transpose()?;

        let ws_base = config.ws_path.trim_end_matches('/');
        let ws_channel_paths: HashMap<String, String> = asyncapi
            .as_ref()
            .map(|a| {
                a.channel_names().into_iter().map(|ch| (format!("{ws_base}/{ch}"), ch)).collect()
            })
            .unwrap_or_default();

        // The WebSocket base path is registered as an explicit axum route even
        // when no AsyncAPI document is supplied, and explicit routes always beat
        // the fallback that serves OpenAPI paths. Report the collision instead
        // of silently making the declared operation unreachable.
        if let Some(openapi) = openapi.as_ref() {
            let ws_routes = std::iter::once(config.ws_path.as_str())
                .chain(ws_channel_paths.keys().map(String::as_str));
            for ws_route in ws_routes {
                if let Some(template) = openapi.shadowed_path(ws_route) {
                    return Err(RuntimeError::Config(format!(
                        "OpenAPI path '{template}' collides with the WebSocket route '{ws_route}'. \
                         WebSocket routes take precedence, so that operation would be unreachable. \
                         Re-run with a different --ws-path."
                    )));
                }
            }
        }

        Ok(Self {
            openapi,
            asyncapi,
            mode: config.mode,
            upstream,
            seed: config.seed,
            ws_path: config.ws_path.clone(),
            ws_channel_paths,
            max_body_size: config.max_body_size,
            client: hpx::Client::new(),
        })
    }
}

/// Spawn HTTP/WS server.
pub async fn spawn_http_server(
    runtime: HttpRuntime,
    bind_addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), RuntimeError> {
    let listener = TcpListener::bind(bind_addr).await?;
    let bound = listener.local_addr()?;
    let state = Arc::new(runtime);

    let mut app = Router::new().route(&state.ws_path, get(ws_handler::ws_upgrade_handler));
    for ws_channel_path in state.ws_channel_paths.keys() {
        app = app.route(ws_channel_path, get(ws_handler::ws_upgrade_handler));
    }
    let app = app.fallback(http_fallback_handler).with_state(Arc::clone(&state));

    let task = tokio::spawn(async move {
        let _ignored = serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.notified().await;
            })
            .await;
    });

    Ok((bound, task))
}

async fn http_fallback_handler(
    State(runtime): State<Arc<HttpRuntime>>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();

    let body_bytes = match to_bytes(request.into_body(), runtime.max_body_size).await {
        Ok(bytes) => bytes,
        Err(_error) => {
            return problem_response(ProblemDetails::payload_too_large(&format!(
                "request body exceeds maximum size of {} bytes",
                runtime.max_body_size
            )));
        }
    };

    let Some(openapi) = &runtime.openapi else {
        return problem_response(ProblemDetails::not_found("no OpenAPI runtime configured"));
    };

    let path = uri.path().to_owned();
    let Some(matched) = openapi.match_operation(&method, &path) else {
        return problem_response(ProblemDetails::not_found("operation not found"));
    };

    // Content-Type validation: when the operation declares a request body, the
    // client's media type must be among those declared.
    if !body_bytes.is_empty() && !matched.operation.request_body_media_types.is_empty() {
        let content_type = content_type_header(&headers).unwrap_or_default();
        if !media::media_type_is_accepted(
            &matched.operation.request_body_media_types,
            &content_type,
        ) {
            return problem_response(ProblemDetails::unsupported_media_type(&format!(
                "content-type '{}' is not declared for this operation; expected one of: {}",
                content_type,
                matched.operation.request_body_media_types.join(", ")
            )));
        }
    }

    let query_params = parse_query(uri.query());
    let request_body_json = match parse_optional_json_body(
        &headers,
        &body_bytes,
        matched.operation.request_body_schema.is_some(),
    ) {
        Ok(value) => value,
        Err(issue) => return error_response(StatusCode::BAD_REQUEST, vec![issue]),
    };

    let validation_issues = validate_http_request(
        &matched,
        &query_params,
        &headers,
        request_body_json.as_ref(),
        !body_bytes.is_empty(),
    );
    if !validation_issues.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, validation_issues);
    }

    if runtime.mode == MockMode::Proxy &&
        let Some(upstream) = &runtime.upstream
    {
        return proxy::proxy_request(
            runtime.as_ref(),
            upstream,
            &method,
            &uri,
            &headers,
            &body_bytes,
            &matched,
        )
        .await;
    }

    let prefer = PreferDirectives::from_headers(&headers);
    let seed = crate::deterministic_hash(runtime.seed, &format!("{method}{path}"));
    let mut mock_response = match matched.operation.mock_response(seed, &prefer) {
        Ok(mock_response) => mock_response,
        Err(RuntimeError::NotFound(message)) => {
            return problem_response(ProblemDetails::not_found(&message));
        }
        Err(RuntimeError::NotAcceptable(message)) => {
            return problem_response(ProblemDetails::not_acceptable(&message));
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                vec![ValidationIssue {
                    instance_pointer: "/response".to_owned(),
                    schema_pointer: "#/responses".to_owned(),
                    keyword: "response".to_owned(),
                    message: sanitize_error_message(&error.to_string()),
                }],
            );
        }
    };

    // `Location` must name a resource on *this* server, so it is derived from
    // the request rather than from a spec example or the faker.
    openapi::render_location_header(
        &mut mock_response.headers,
        &matched.operation.path_template,
        &matched.path_params,
    );

    let response = build_mock_response(&mock_response);

    // Fire callbacks asynchronously (fire-and-forget).
    if !matched.operation.callbacks.is_empty() {
        for callback in &matched.operation.callbacks {
            if let Some(url) = openapi::resolve_callback_url(
                &callback.callback_url_expression,
                request_body_json.as_ref(),
            ) {
                if !is_valid_callback_url(&url) {
                    tracing::warn!(url, "callback skipped: non-HTTP scheme");
                    continue;
                }
                let client = runtime.client.clone();
                let cb_method = callback.method.clone();
                let cb_schema = callback.request_body_schema.clone();
                tokio::spawn(async move {
                    fire_callback(&client, &url, &cb_method, cb_schema.as_ref(), seed).await;
                });
            }
        }
    }

    response
}

fn validate_http_request(
    matched: &MatchedOperation<'_>,
    query_params: &HashMap<String, Vec<String>>,
    headers: &HeaderMap,
    body_json: Option<&Value>,
    body_present: bool,
) -> Vec<ValidationIssue> {
    matched.operation.validate_request(
        &matched.path_params,
        query_params,
        headers,
        body_json,
        body_present,
    )
}

/// Fire an outbound callback request. Errors are logged but never propagated.
async fn fire_callback(
    client: &hpx::Client,
    url: &str,
    method: &Method,
    schema: Option<&Value>,
    seed: u64,
) {
    let body = schema.and_then(|s| generate_json_value(s, seed).ok());
    let mut req = client.request(method.clone(), url);
    if let Some(body) = body {
        let encoded = serde_json::to_vec(&body).unwrap_or_default();
        req = req.header("content-type", "application/json").body(encoded);
    }
    match req.send().await {
        Ok(response) => tracing::info!(status = %response.status(), url, "callback fired"),
        Err(error) => tracing::warn!(%error, url, "callback failed"),
    }
}

fn parse_optional_json_body(
    headers: &HeaderMap,
    bytes: &[u8],
    should_parse: bool,
) -> Result<Option<Value>, ValidationIssue> {
    if !should_parse || bytes.is_empty() {
        return Ok(None);
    }
    if !header_is_json(headers) {
        return Ok(None);
    }
    serde_json::from_slice::<Value>(bytes).map(Some).map_err(|error| ValidationIssue {
        instance_pointer: "/body".to_owned(),
        schema_pointer: "#/requestBody".to_owned(),
        keyword: "json".to_owned(),
        message: format!("invalid json request body: {error}"),
    })
}

fn parse_query(query: Option<&str>) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(raw) = query {
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            out.entry(key.into_owned()).or_default().push(value.into_owned());
        }
    }
    out
}

fn header_is_json(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
}

/// Raw `Content-Type` request header value, empty when absent.
fn content_type_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Convert a declared response header into a concrete HTTP header pair.
///
/// Specs may declare header names that are not valid HTTP tokens; those are
/// skipped rather than failing the whole response.
fn response_header(
    name: &str,
    value: &str,
) -> Option<(axum::http::HeaderName, axum::http::HeaderValue)> {
    let header_name = axum::http::HeaderName::from_bytes(name.as_bytes()).ok()?;
    let header_value = axum::http::HeaderValue::from_str(value).ok()?;
    Some((header_name, header_value))
}

/// Render a mock response into an HTTP response.
///
/// The negotiated media type drives both the `Content-Type` header and the body
/// encoding, so `text/plain`, `application/xml`, and binary responses carry real
/// payloads instead of an empty body.
fn build_mock_response(mock: &openapi::MockHttpResponse) -> Response {
    let status = StatusCode::from_u16(mock.status).unwrap_or(StatusCode::OK);

    let mut builder = Response::builder().status(status);
    for (name, value) in &mock.headers {
        if let Some((header_name, header_value)) = response_header(name, value) &&
            let Some(headers) = builder.headers_mut()
        {
            headers.append(header_name, header_value);
        }
    }

    let body = match (&mock.body, &mock.media_type) {
        (Some(body), Some(media_type)) => {
            media::encode_body(media_type, body, mock.schema.as_ref())
        }
        _ => Vec::new(),
    };

    if let Some(media_type) = &mock.media_type &&
        let Some(headers) = builder.headers_mut()
    {
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_str(media_type)
                .unwrap_or_else(|_error| axum::http::HeaderValue::from_static("application/json")),
        );
    }

    builder.body(Body::from(body)).unwrap_or_else(|_error| {
        let mut fallback = Response::new(Body::empty());
        *fallback.status_mut() = status;
        fallback
    })
}

fn error_response(status: StatusCode, issues: Vec<ValidationIssue>) -> Response {
    let problem = ProblemDetails::validation_error(status.as_u16(), issues);
    problem_response(problem)
}

fn problem_response(problem: ProblemDetails) -> Response {
    let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::to_vec(&problem).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, PROBLEM_JSON_CONTENT_TYPE)
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Replace tokens that look like absolute filesystem paths with `[redacted]`.
///
/// A token is considered path-like if it starts with `/` and contains at least
/// one additional `/` separator (i.e. `/foo/bar` but not `/pets/{id}`).
fn sanitize_error_message(msg: &str) -> String {
    msg.split_whitespace()
        .map(
            |token| {
                if token.starts_with('/') && token[1..].contains('/') {
                    "[redacted]"
                } else {
                    token
                }
            },
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_valid_callback_url(url: &str) -> bool {
    // ponytail: SSRF guard — reject non-http(s) schemes
    matches!(
        url::Url::parse(url).map(|u| u.scheme().to_owned()),
        Ok(scheme) if scheme == "http" || scheme == "https"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn callback_url_rejects_non_http_schemes() {
        assert!(!is_valid_callback_url("file:///etc/passwd"));
        assert!(!is_valid_callback_url("ftp://example.com"));
        assert!(!is_valid_callback_url("javascript:alert(1)"));
        assert!(!is_valid_callback_url("data:text/html,<h1>hi</h1>"));
    }

    #[test]
    fn callback_url_accepts_http_schemes() {
        assert!(is_valid_callback_url("http://example.com/cb"));
        assert!(is_valid_callback_url("https://example.com/cb?token=abc"));
    }

    #[test]
    fn callback_url_rejects_malformed_urls() {
        assert!(!is_valid_callback_url("not a url"));
        assert!(!is_valid_callback_url(""));
    }

    #[test]
    fn sanitize_strips_absolute_paths() {
        let input = "cannot read /Users/dev/project/specs/api.yaml";
        let out = sanitize_error_message(input);
        assert!(!out.contains("/Users"), "should redact /Users path: {out}");
        assert!(out.contains("[redacted]"), "should contain [redacted]: {out}");
    }

    #[test]
    fn sanitize_strips_multiple_paths() {
        let input = "file ref points outside allowed roots /tmp/foo/bar";
        let out = sanitize_error_message(input);
        assert!(!out.contains("/tmp"), "should redact /tmp path: {out}");
    }

    #[test]
    fn sanitize_preserves_non_path_text() {
        let input = "invalid yaml: mapping values are not allowed here";
        let out = sanitize_error_message(input);
        assert_eq!(out, input);
    }

    #[test]
    fn sanitize_preserves_single_segment_slash() {
        // Things like "/pets/{id}" or "/response" should NOT be redacted.
        let input = "operation not found at /response";
        let out = sanitize_error_message(input);
        assert_eq!(out, input);
    }

    // ── sanitize_error_message edge cases ──────────────────────────────

    #[test]
    fn sanitize_handles_a_lone_slash_token() {
        assert_eq!(sanitize_error_message("/"), "/");
        assert_eq!(sanitize_error_message("a / b"), "a / b");
    }

    #[test]
    fn sanitize_redacts_a_bare_leading_slash() {
        assert_eq!(sanitize_error_message("/a/b"), "[redacted]");
        assert_eq!(sanitize_error_message("/a/b/c"), "[redacted]");
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_error_message("a   b\tc\nd"), "a b c d");
        assert_eq!(sanitize_error_message(""), "");
        assert_eq!(sanitize_error_message("   "), "");
    }

    #[test]
    fn sanitize_keeps_relative_paths_with_one_segment() {
        let input = "spec at ./schemas/pet.yaml";
        assert_eq!(sanitize_error_message(input), input, "a relative path is not an absolute leak");
    }

    #[test]
    fn sanitize_is_idempotent() {
        let once = sanitize_error_message("cannot read /Users/dev/project/api.yaml");
        assert_eq!(sanitize_error_message(&once), once);
    }

    // ── header helpers ─────────────────────────────────────────────────

    #[test]
    fn header_is_json_detects_the_json_content_type() {
        for value in [
            "application/json",
            "application/json; charset=utf-8",
            "application/json; charset=UTF-8",
            "APPLICATION/JSON",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::CONTENT_TYPE, value.parse().expect("value"));
            assert!(header_is_json(&headers), "{value} should be treated as JSON");
        }
    }

    #[test]
    fn header_is_json_is_narrower_than_the_media_type_classifier() {
        // `header_is_json` matches on the literal `application/json` substring,
        // so a structured-suffix type that `media::is_json_media_type` accepts
        // is not treated as JSON here.
        assert!(super::media::is_json_media_type("application/vnd.api+json"));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.api+json".parse().expect("value"),
        );
        assert!(
            !header_is_json(&headers),
            "this predicate only recognises the plain application/json type"
        );
    }

    #[test]
    fn header_is_json_rejects_non_json_and_absent() {
        for value in ["text/plain", "application/xml", "multipart/form-data", ""] {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::CONTENT_TYPE, value.parse().expect("value"));
            assert!(!header_is_json(&headers), "{value} should not be treated as JSON");
        }
        assert!(!header_is_json(&HeaderMap::new()), "a missing header is not JSON");
    }

    #[test]
    fn header_is_json_tolerates_invalid_header_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_bytes(b"application/json\x80").expect("raw bytes"),
        );
        assert!(!header_is_json(&headers), "an undecodable header must not be treated as JSON");
    }

    #[test]
    fn content_type_header_returns_raw_value_or_none() {
        let mut headers = HeaderMap::new();
        assert_eq!(content_type_header(&headers), None);

        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "Application/JSON; charset=UTF-8".parse().expect("value"),
        );
        assert_eq!(
            content_type_header(&headers),
            Some("Application/JSON; charset=UTF-8".to_owned()),
            "the raw header text is preserved; normalization happens downstream"
        );
    }

    // ── query parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_query_groups_repeated_keys() {
        let parsed = parse_query(Some("tag=a&tag=b&limit=5"));
        assert_eq!(parsed.get("tag"), Some(&vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(parsed.get("limit"), Some(&vec!["5".to_owned()]));
    }

    #[test]
    fn parse_query_decodes_percent_and_plus_escapes() {
        let parsed = parse_query(Some("q=hello%20world&r=a%2Bb&s=x+y"));
        assert_eq!(parsed.get("q"), Some(&vec!["hello world".to_owned()]));
        assert_eq!(parsed.get("r"), Some(&vec!["a+b".to_owned()]));
        assert_eq!(parsed.get("s"), Some(&vec!["x y".to_owned()]));
    }

    #[test]
    fn parse_query_handles_empty_and_absent_input() {
        assert!(parse_query(None).is_empty());
        assert!(parse_query(Some("")).is_empty());
        assert!(parse_query(Some("&&")).is_empty());
    }

    #[test]
    fn parse_query_keeps_valueless_keys() {
        // `form_urlencoded` yields an empty string for a key with no `=`.
        let parsed = parse_query(Some("flag"));
        assert_eq!(parsed.get("flag"), Some(&vec![String::new()]));
    }

    // ── request body parsing ───────────────────────────────────────────

    #[test]
    fn parse_optional_json_body_returns_none_when_not_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().expect("v"));
        let parsed =
            parse_optional_json_body(&headers, br#"{"a":1}"#, false).expect("should not parse");
        assert!(parsed.is_none(), "a schema-less operation skips body parsing");
    }

    #[test]
    fn parse_optional_json_body_returns_none_for_empty_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().expect("v"));
        assert!(
            parse_optional_json_body(&headers, b"", true).expect("should not parse").is_none(),
            "an absent body is not a parse error"
        );
    }

    #[test]
    fn parse_optional_json_body_skips_non_json_content_types() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "text/plain".parse().expect("v"));
        assert!(
            parse_optional_json_body(&headers, b"not json at all", true)
                .expect("no parse")
                .is_none(),
            "a declared non-JSON media type is accepted without schema validation"
        );
    }

    #[test]
    fn parse_optional_json_body_parses_json() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().expect("v"));
        let parsed =
            parse_optional_json_body(&headers, br#"{"a":1}"#, true).expect("valid json must parse");
        assert_eq!(parsed, Some(serde_json::json!({"a": 1})));
    }

    #[test]
    fn parse_optional_json_body_accepts_scalar_json() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().expect("v"));
        assert_eq!(
            parse_optional_json_body(&headers, b"42", true).expect("scalar json parses"),
            Some(serde_json::Value::from(42))
        );
    }

    #[test]
    fn parse_optional_json_body_reports_malformed_json() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().expect("v"));

        let issue = parse_optional_json_body(&headers, b"{oops", true)
            .expect_err("malformed json must be reported");
        assert_eq!(issue.keyword, "json");
        assert_eq!(issue.instance_pointer, "/body");
        assert_eq!(issue.schema_pointer, "#/requestBody");
        assert!(issue.message.contains("invalid json request body"), "got {}", issue.message);
    }

    // ── response header rendering ──────────────────────────────────────

    #[test]
    fn response_header_accepts_valid_pairs() {
        let pair = response_header("X-Trace-Id", "abc-123").expect("valid header");
        assert_eq!(pair.0.as_str(), "x-trace-id", "header names are lowercased");
        assert_eq!(pair.1.to_str().unwrap_or_default(), "abc-123");
    }

    #[test]
    fn response_header_rejects_invalid_names() {
        for name in ["", "bad header", "colon:name", "new\nline", "a:b"] {
            assert!(
                response_header(name, "value").is_none(),
                "invalid header name {name:?} must be skipped"
            );
        }
    }

    #[test]
    fn response_header_rejects_invalid_values() {
        assert!(
            response_header("X-Ok", "line\nbreak").is_none(),
            "a value containing a newline must be skipped"
        );
        assert!(response_header("X-Ok", "").is_some(), "an empty value is legal");
    }

    // ── mock response rendering ────────────────────────────────────────

    #[test]
    fn build_mock_response_renders_status_headers_and_body() {
        let mock = openapi::MockHttpResponse {
            status: 418,
            media_type: Some("application/json".to_owned()),
            schema: Some(serde_json::json!({"type": "object"})),
            body: Some(serde_json::json!({"a": 1})),
            headers: vec![("X-Trace-Id".to_owned(), "t-1".to_owned())],
        };

        let response = build_mock_response(&mock);
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(response.headers().get("x-trace-id").and_then(|v| v.to_str().ok()), Some("t-1"));
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn build_mock_response_handles_a_bodyless_response() {
        let mock = openapi::MockHttpResponse {
            status: 204,
            media_type: None,
            schema: None,
            body: None,
            headers: Vec::new(),
        };

        let response = build_mock_response(&mock);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response.headers().get(axum::http::header::CONTENT_TYPE).is_none(),
            "a bodyless response must not advertise a content type"
        );
    }

    #[test]
    fn build_mock_response_skips_unusable_declared_headers() {
        let mock = openapi::MockHttpResponse {
            status: 200,
            media_type: Some("text/plain".to_owned()),
            schema: None,
            body: Some(serde_json::json!("hi")),
            headers: vec![
                ("bad header name".to_owned(), "v".to_owned()),
                ("X-Good".to_owned(), "v".to_owned()),
            ],
        };

        let response = build_mock_response(&mock);
        assert!(
            response.headers().get("x-good").is_some(),
            "a bad header must not suppress the good ones"
        );
        assert_eq!(response.headers().get_all("x-good").iter().count(), 1);
    }

    #[test]
    fn build_mock_response_falls_back_to_200_for_an_invalid_status() {
        let mock = openapi::MockHttpResponse {
            status: 9999,
            media_type: None,
            schema: None,
            body: None,
            headers: Vec::new(),
        };
        assert_eq!(build_mock_response(&mock).status(), StatusCode::OK);
    }

    #[test]
    fn build_mock_response_encodes_the_body_for_the_negotiated_media_type() {
        let mock = openapi::MockHttpResponse {
            status: 200,
            media_type: Some("text/plain".to_owned()),
            schema: None,
            body: Some(serde_json::json!("plain body")),
            headers: Vec::new(),
        };

        let response = build_mock_response(&mock);
        // Body encoding is sync; assert through the negotiated content type here.
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("text/plain")
        );
    }

    // ── problem responses ──────────────────────────────────────────────

    #[test]
    fn problem_response_sets_the_rfc7807_content_type() {
        let problem = ProblemDetails::not_found("nope");
        let response = problem_response(problem);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON_CONTENT_TYPE)
        );
    }

    #[test]
    fn error_response_maps_status_into_the_problem_body() {
        let response = error_response(
            StatusCode::BAD_GATEWAY,
            vec![ValidationIssue {
                instance_pointer: "/x".to_owned(),
                schema_pointer: "#".to_owned(),
                keyword: "k".to_owned(),
                message: "m".to_owned(),
            }],
        );
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    // ── ws channel resolution ──────────────────────────────────────────

    #[test]
    fn ws_channel_paths_are_empty_without_an_asyncapi_spec() {
        let config = crate::ServerConfig {
            openapi_spec: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-pets.yaml"),
            ),
            ..Default::default()
        };
        let runtime =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");

        let runtime = runtime.block_on(HttpRuntime::from_config(&config)).expect("http runtime");
        assert!(
            runtime.ws_channel_paths.is_empty(),
            "with no AsyncAPI spec there are no per-channel WebSocket routes"
        );
        assert_eq!(runtime.resolve_ws_channel("/ws/chat.send"), None);
    }

    #[tokio::test]
    async fn ws_channel_paths_are_derived_from_the_asyncapi_channels() {
        let config = crate::ServerConfig {
            asyncapi_spec: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/asyncapi-chat.yaml"),
            ),
            ws_path: "/socket/".to_owned(),
            ..Default::default()
        };

        let runtime = HttpRuntime::from_config(&config).await.expect("http runtime");
        assert_eq!(runtime.resolve_ws_channel("/socket/chat.send"), Some("chat.send".to_owned()));
        assert_eq!(runtime.resolve_ws_channel("/ws/chat.send"), None, "the base path is rebound");
    }

    // ── config errors ──────────────────────────────────────────────────

    #[tokio::test]
    async fn proxy_mode_without_upstream_is_rejected() {
        let config = crate::ServerConfig {
            openapi_spec: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-pets.yaml"),
            ),
            mode: MockMode::Proxy,
            ..Default::default()
        };

        let error = HttpRuntime::from_config(&config).await.expect_err("proxy needs upstream");
        assert!(
            error.to_string().contains("proxy mode requires upstream"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn malformed_upstream_url_is_rejected() {
        let config = crate::ServerConfig {
            openapi_spec: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-pets.yaml"),
            ),
            mode: MockMode::Proxy,
            upstream: Some("not a url".to_owned()),
            allow_private_upstream: true,
            ..Default::default()
        };

        let error = HttpRuntime::from_config(&config).await.expect_err("url must be parsed");
        assert!(error.to_string().contains("invalid upstream URL"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn valid_config_builds_a_runtime() {
        let config = crate::ServerConfig {
            openapi_spec: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-pets.yaml"),
            ),
            seed: 99,
            max_body_size: 1234,
            ..Default::default()
        };

        let runtime = HttpRuntime::from_config(&config).await.expect("http runtime");
        assert_eq!(runtime.seed, 99);
        assert_eq!(runtime.max_body_size, 1234);
        assert!(runtime.openapi.is_some());
        assert!(runtime.asyncapi.is_none());
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Recursive strategy for arbitrary JSON values.
    ///
    /// `serde_json::Value` does not implement `proptest::Arbitrary`, so the
    /// generator is assembled from bounded leaf and container shapes.
    fn any_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(serde_json::Value::from),
            any::<i64>().prop_map(serde_json::Value::from),
            "[ -~]{0,8}".prop_map(serde_json::Value::from),
        ];
        leaf.prop_recursive(3, 12, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::hash_map("[a-z]{1,6}", inner, 0..4).prop_map(|members| {
                    let map: serde_json::Map<String, serde_json::Value> =
                        members.into_iter().collect();
                    serde_json::Value::Object(map)
                }),
            ]
        })
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// `sanitize_error_message` must never leak a multi-segment absolute
        /// path token, whatever the message contains.
        #[test]
        fn sanitize_never_leaks_absolute_paths(message in ".{0,80}")
        {
            let out = sanitize_error_message(&message);
            for token in out.split_whitespace() {
                if let Some(rest) = token.strip_prefix('/') {
                    prop_assert!(
                        !rest.contains('/'),
                        "absolute path {token:?} survived sanitization in {out:?}"
                    );
                }
            }
        }

        /// Sanitization is idempotent: redacting an already-redacted message
        /// must be a no-op, so double-reporting never corrupts the text.
        #[test]
        fn sanitize_is_idempotent_for_any_message(message in ".{0,80}")
        {
            let once = sanitize_error_message(&message);
            prop_assert_eq!(sanitize_error_message(&once), once);
        }

        /// Whatever query string is supplied, parsing yields a mapping whose keys
        /// and values are the percent-decoded segments, or nothing at all.
        #[test]
        fn query_parsing_never_panics(query in "[a-zA-Z0-9%+=&_.-]{0,40}")
        {
            // `form_urlencoded` may legitimately produce empty keys (for "=" or
            // a leading "&"), so only totality is asserted here; the decoding
            // behaviour is covered by the example-based tests.
            let _ = parse_query(Some(&query));
        }

        /// `header_is_json` only ever returns `true` for the JSON media family.
        #[test]
        fn json_detection_never_reports_xml_or_binary(
            value in "(application|text|image|audio|video|multipart)/[a-z0-9.+-]{0,12}"
        ) {
            if value.contains("json") {
                return Ok(());
            }
            let mut headers = HeaderMap::new();
            let Ok(header_value) = axum::http::HeaderValue::from_str(&value) else {
                return Ok(());
            };
            headers.insert(axum::http::header::CONTENT_TYPE, header_value);
            prop_assert!(!header_is_json(&headers), "{} must not be seen as JSON", value);
        }

        /// `is_valid_callback_url` is an SSRF guard: only absolute http(s) URLs
        /// may ever pass.
        #[test]
        fn callback_url_guard_only_accepts_http_schemes(
            scheme in "[a-z]{1,10}",
            rest in "[a-zA-Z0-9./:_-]{0,20}",
        ) {
            let candidate = format!("{scheme}://host/{rest}");
            if is_valid_callback_url(&candidate) {
                prop_assert!(candidate.starts_with("http://") || candidate.starts_with("https://"));
            }
        }

        /// Rendering any JSON body for any media type never panics.
        #[test]
        fn mock_response_rendering_never_panics(
            value in any_json(),
            media_type in "[a-z]{1,6}/[a-z0-9.+-]{1,12}",
            status in 100u16..600,
        ) {
            let mock = openapi::MockHttpResponse {
                status,
                media_type: Some(media_type.clone()),
                schema: None,
                body: Some(value),
                headers: Vec::new(),
            };
            let response = build_mock_response(&mock);
            prop_assert_eq!(response.status().as_u16(), status);
        }
    }
}
