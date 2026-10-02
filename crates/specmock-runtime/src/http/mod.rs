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
    let mock_response = match matched.operation.mock_response(seed, &prefer) {
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
}
