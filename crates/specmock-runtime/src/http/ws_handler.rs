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

    // Scenario and session are chosen with query parameters because the WebSocket
    // handshake offers nowhere to put a header the client controls.
    let query_param = |name: &str| -> Option<String> {
        uri.query().and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == name)
                    .then(|| percent_decode(value).unwrap_or_else(|| value.to_owned()))
                    .filter(|value| !value.is_empty())
            })
        })
    };
    let requested_scenario = query_param("scenario");
    let requested_session = query_param("session");

    // A connection is its own session unless the client names one, which is the
    // WebSocket analogue of `Prefer: session=`. Two connections that want to
    // share a store have to say so; otherwise parallel clients silently trample
    // each other. An unnamed session is released when its connection closes, so
    // a long-lived server does not slowly fill its cap with dead conversations.
    let session = requested_session.unwrap_or_else(|| runtime.fresh_ws_session());

    ws.on_upgrade(move |socket| {
        ws_socket_loop(socket, runtime, pinned_channel, requested_scenario, session)
    })
    .into_response()
}

/// Minimal percent-decoding for a query value.
fn percent_decode(value: &str) -> Option<String> {
    if !value.contains('%') {
        return None;
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// WebSocket message processing loop.
async fn ws_socket_loop(
    mut socket: WebSocket,
    runtime: Arc<HttpRuntime>,
    pinned_channel: Option<String>,
    requested_scenario: Option<String>,
    session: String,
) {
    let mut rate_limiter = RateLimiter::new();
    let scenario_name = requested_scenario.or_else(|| {
        runtime
            .scenarios
            .as_ref()
            .and_then(|scenarios| scenarios.default_scenario_name().map(ToOwned::to_owned))
    });

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

        // A scenario may answer from its store instead of generating a payload.
        // The *incoming* message and the generated reply are kept apart: a create
        // keyed on the request must store what the client sent, not what the
        // faker produced.
        let incoming = serde_json::from_str::<Value>(&text).ok();
        let outcome = apply_scenario(
            &runtime,
            &outcome,
            scenario_name.as_deref(),
            &session,
            pinned_channel.as_deref(),
            incoming.as_ref(),
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

    // Release the connection's instance so it stops occupying the session cap.
    if let (Some(scenarios), Some(scenario)) = (&runtime.scenarios, &scenario_name) {
        scenarios.release(scenario, &session);
    }
}

/// Let a scenario answer from its store, or inject a failure.
///
/// Returns the original outcome untouched when no scenario governs the channel, so
/// a sidecar that mentions only some channels stays an additive overlay.
fn apply_scenario(
    runtime: &HttpRuntime,
    outcome: &WsOutcome,
    scenario_name: Option<&str>,
    session: &str,
    pinned_channel: Option<&str>,
    incoming: Option<&Value>,
) -> WsOutcome {
    let (Some(scenarios), Some(scenario)) = (&runtime.scenarios, scenario_name) else {
        return outcome.clone();
    };
    // Auto-routed messages carry no channel name, so only a pinned path can be
    // attributed to an operation.
    let Some(channel) = pinned_channel else {
        return outcome.clone();
    };

    let WsOutcome::Mock { payload, .. } = outcome else {
        return outcome.clone();
    };

    let resolution = scenarios.resolve_channel(scenario, session, channel, incoming, payload);

    match resolution {
        super::scenarios::Resolution::Body(value) => {
            WsOutcome::Mock { channel: channel.to_owned(), payload: value }
        }
        // An operation whose catalogue entry only pins a status still carries the
        // store's body through, so this is not a fall-through case.
        super::scenarios::Resolution::Forced { body: Some(value), .. } => {
            WsOutcome::Mock { channel: channel.to_owned(), payload: value }
        }
        super::scenarios::Resolution::Failed(failure) => WsOutcome::Error {
            errors: vec![ValidationIssue {
                instance_pointer: "/".to_owned(),
                schema_pointer: "#".to_owned(),
                keyword: "scenario".to_owned(),
                message: failure.detail(),
            }],
        },
        _ => outcome.clone(),
    }
}
