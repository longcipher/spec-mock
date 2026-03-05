//! HTTP and WebSocket server runtime.

pub mod negotiate;
pub mod openapi;
pub mod router;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{
        Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    serve,
};
use futures_util::StreamExt;
use negotiate::PreferDirectives;
use openapi::{MatchedOperation, OpenApiRuntime};
use serde_json::Value;
use specmock_core::{
    MockMode, PROBLEM_JSON_CONTENT_TYPE, ProblemDetails, ValidationIssue,
    faker::generate_json_value, validate::validate_instance,
};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{
    RuntimeError,
    ws::{AsyncApiRuntime, WsOutcome},
};

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

    let mut app = Router::new().route(&state.ws_path, get(ws_upgrade_handler));
    for ws_channel_path in state.ws_channel_paths.keys() {
        app = app.route(ws_channel_path, get(ws_upgrade_handler));
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

async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(runtime): State<Arc<HttpRuntime>>,
    uri: axum::http::Uri,
) -> impl IntoResponse {
    if runtime.asyncapi.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"asyncapi runtime is not configured"})),
        )
            .into_response();
    }

    let pinned_channel = runtime.resolve_ws_channel(uri.path());
    ws.on_upgrade(move |socket| ws_socket_loop(socket, runtime, pinned_channel)).into_response()
}

async fn ws_socket_loop(
    mut socket: WebSocket,
    runtime: Arc<HttpRuntime>,
    pinned_channel: Option<String>,
) {
    while let Some(next_item) = socket.next().await {
        let Ok(message) = next_item else {
            break;
        };

        let Message::Text(text) = message else {
            continue;
        };

        let outcome = runtime.asyncapi.as_ref().map_or_else(
            || WsOutcome::Error {
                errors: vec![ValidationIssue {
                    instance_pointer: "/".to_owned(),
                    schema_pointer: "#".to_owned(),
                    keyword: "runtime".to_owned(),
                    message: "asyncapi runtime is not configured".to_owned(),
                }],
            },
            |asyncapi| {
                if let Some(channel) = &pinned_channel {
                    // Wrap raw payload in an explicit envelope for the pinned channel.
                    let envelope = match serde_json::from_str::<Value>(&text) {
                        Ok(payload) => serde_json::json!({"channel": channel, "payload": payload}),
                        Err(_error) => {
                            // If the raw text is not valid JSON, pass it through and let
                            // handle_message produce the parse-error outcome.
                            return asyncapi.handle_message(&text, runtime.seed);
                        }
                    };
                    asyncapi.handle_message(&envelope.to_string(), runtime.seed)
                } else {
                    asyncapi.handle_message(&text, runtime.seed)
                }
            },
        );

        let encoded = match serde_json::to_string(&outcome) {
            Ok(value) => value,
            Err(error) => {
                let fallback = serde_json::json!({
                    "type": "error",
                    "errors": [{
                        "instance_pointer": "/",
                        "schema_pointer": "#",
                        "keyword": "json",
                        "message": format!("failed to encode ws response: {error}")
                    }]
                });
                fallback.to_string()
            }
        };

        if socket.send(Message::Text(encoded.into())).await.is_err() {
            break;
        }
    }
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

    // Content-Type validation: if operation declares a request body schema and the body
    // is non-empty, require a JSON-compatible Content-Type.
    if matched.operation.request_body_schema.is_some() &&
        !body_bytes.is_empty() &&
        !header_is_json(&headers)
    {
        return problem_response(ProblemDetails::unsupported_media_type(
            "Content-Type must be application/json for this operation",
        ));
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

    let validation_issues =
        validate_http_request(&matched, &query_params, &headers, request_body_json.as_ref());
    if !validation_issues.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, validation_issues);
    }

    if runtime.mode == MockMode::Proxy &&
        let Some(upstream) = &runtime.upstream
    {
        return proxy_request(
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
    let seed = runtime.seed ^ hash_path_and_method(&path, &method);
    let response = match matched.operation.mock_response(seed, &prefer) {
        Ok(mock_response) => {
            if let Some(body) = mock_response.body {
                json_response(
                    StatusCode::from_u16(mock_response.status).unwrap_or(StatusCode::OK),
                    &body,
                )
            } else {
                Response::builder()
                    .status(StatusCode::from_u16(mock_response.status).unwrap_or(StatusCode::OK))
                    .body(Body::empty())
                    .unwrap_or_else(|_error| Response::new(Body::empty()))
            }
        }
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                vec![ValidationIssue {
                    instance_pointer: "/response".to_owned(),
                    schema_pointer: "#/responses".to_owned(),
                    keyword: "response".to_owned(),
                    message: error.to_string(),
                }],
            );
        }
    };

    // Fire callbacks asynchronously (fire-and-forget).
    if !matched.operation.callbacks.is_empty() {
        for callback in &matched.operation.callbacks {
            if let Some(url) = openapi::resolve_callback_url(
                &callback.callback_url_expression,
                request_body_json.as_ref(),
            ) {
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
) -> Vec<ValidationIssue> {
    matched.operation.validate_request(&matched.path_params, query_params, headers, body_json)
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

async fn proxy_request(
    runtime: &HttpRuntime,
    upstream: &url::Url,
    method: &Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body_bytes: &[u8],
    matched: &MatchedOperation<'_>,
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
        if lower == "host" || lower == "content-length" {
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

fn json_response(status: StatusCode, body: &Value) -> Response {
    (status, Json(body.clone())).into_response()
}

fn hash_path_and_method(path: &str, method: &Method) -> u64 {
    let method_hash = method
        .as_str()
        .bytes()
        .fold(0_u64, |acc, byte| acc.wrapping_mul(131).wrapping_add(u64::from(byte)));
    path.bytes().fold(method_hash, |acc, byte| acc.wrapping_mul(131).wrapping_add(u64::from(byte)))
}
