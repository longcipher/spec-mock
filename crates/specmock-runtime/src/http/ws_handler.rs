//! WebSocket handler for AsyncAPI runtime.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde_json::Value;
use specmock_core::ValidationIssue;
use tokio::time::{Duration, Instant};

use super::HttpRuntime;
use crate::ws::WsOutcome;

/// Maximum number of WebSocket messages per second per connection.
const MAX_WS_MESSAGES_PER_SECOND: u32 = 100;

/// WebSocket rate limiter state.
#[derive(Debug)]
struct RateLimiter {
    message_count: u32,
    window_start: Instant,
}

impl RateLimiter {
    fn new() -> Self {
        Self { message_count: 0, window_start: Instant::now() }
    }

    fn check_and_update(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.message_count = 1;
            return true;
        }

        if self.message_count >= MAX_WS_MESSAGES_PER_SECOND {
            return false;
        }

        self.message_count += 1;
        true
    }
}

/// Handle WebSocket upgrade requests.
pub async fn ws_upgrade_handler(
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

/// WebSocket message processing loop.
async fn ws_socket_loop(
    mut socket: WebSocket,
    runtime: Arc<HttpRuntime>,
    pinned_channel: Option<String>,
) {
    let mut rate_limiter = RateLimiter::new();

    while let Some(next_item) = socket.next().await {
        let Ok(message) = next_item else {
            break;
        };

        let Message::Text(text) = message else {
            continue;
        };

        // Check rate limit
        if !rate_limiter.check_and_update() {
            let error_response = serde_json::json!({
                "type": "error",
                "errors": [{
                    "instance_pointer": "/",
                    "schema_pointer": "#",
                    "keyword": "rate_limit",
                    "message": format!("rate limit exceeded: {} messages per second", MAX_WS_MESSAGES_PER_SECOND)
                }]
            });
            if socket.send(Message::Text(error_response.to_string().into())).await.is_err() {
                break;
            }
            continue;
        }

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
