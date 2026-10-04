//! Minimal AsyncAPI-driven WebSocket mock runtime.

use std::{collections::HashMap, path::Path};

use serde::Serialize;
use serde_json::Value;
use specmock_core::{
    ValidationIssue, faker::generate_json_value, ref_resolver::RefResolver,
    validate::validate_instance,
};

use crate::RuntimeError;

/// Runtime model for AsyncAPI channels.
#[derive(Debug, Clone)]
pub struct AsyncApiRuntime {
    channels: HashMap<String, ChannelSpec>,
}

#[derive(Debug, Clone)]
struct ChannelSpec {
    publish_schema: Option<Value>,
    subscribe_schema: Option<Value>,
    subscribe_example: Option<Value>,
}

/// Outgoing WS events from runtime.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsOutcome {
    /// Validation error event.
    Error {
        /// Aggregated errors.
        errors: Vec<ValidationIssue>,
    },
    /// Successful mock response event.
    Mock {
        /// Channel name.
        channel: String,
        /// Generated payload.
        payload: Value,
    },
}

impl AsyncApiRuntime {
    /// Load AsyncAPI document from YAML or JSON.
    ///
    /// The file is loaded, all `$ref` nodes are resolved via [`RefResolver`],
    /// and the fully-inlined document is then parsed into channel specs.
    pub fn from_path(path: &Path) -> Result<Self, RuntimeError> {
        let base_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        let mut resolver = RefResolver::new(base_dir);
        let resolved =
            resolver.resolve(path).map_err(|error| RuntimeError::Parse(error.to_string()))?;
        Self::from_document(resolved)
    }

    fn from_document(root: Value) -> Result<Self, RuntimeError> {
        let version = root.get("asyncapi").and_then(Value::as_str).unwrap_or("2.0.0");
        if version.starts_with("3.") { Self::from_v3(&root) } else { Self::from_v2(&root) }
    }

    fn from_v2(root: &Value) -> Result<Self, RuntimeError> {
        let mut channels = HashMap::new();
        let channel_map = root
            .get("channels")
            .and_then(Value::as_object)
            .ok_or_else(|| RuntimeError::Parse("asyncapi missing channels object".to_owned()))?;

        for (name, channel_node) in channel_map {
            let publish_schema = extract_message_payload_schema(channel_node, "publish");
            let (subscribe_schema, subscribe_example) = extract_subscribe_payload(channel_node);
            channels.insert(
                name.clone(),
                ChannelSpec { publish_schema, subscribe_schema, subscribe_example },
            );
        }

        Ok(Self { channels })
    }

    fn from_v3(root: &Value) -> Result<Self, RuntimeError> {
        let channels_map = root
            .get("channels")
            .and_then(Value::as_object)
            .ok_or_else(|| RuntimeError::Parse("asyncapi v3 missing channels".to_owned()))?;

        // Build channel_value → channel_name lookup for matching inlined refs.
        let value_to_name: Vec<(String, &Value)> =
            channels_map.iter().map(|(name, value)| (name.clone(), value)).collect();

        // Initialise empty specs for every declared channel.
        let mut channel_specs: HashMap<String, ChannelSpec> = channels_map
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    ChannelSpec {
                        publish_schema: None,
                        subscribe_schema: None,
                        subscribe_example: None,
                    },
                )
            })
            .collect();

        // Parse operations — optional because a spec might declare channels only.
        if let Some(ops) = root.get("operations").and_then(Value::as_object) {
            for (_op_name, op_node) in ops {
                let action = op_node.get("action").and_then(Value::as_str);

                // After RefResolver inlining, `operation.channel` holds the full
                // channel object.  Match it by value equality to recover the name.
                let channel_name = op_node.get("channel").and_then(|ch| {
                    value_to_name.iter().find(|(_, v)| *v == ch).map(|(n, _)| n.clone())
                });

                let Some(channel_name) = channel_name else {
                    continue;
                };

                let (schema, example) = extract_v3_operation_messages(op_node);

                if let Some(spec) = channel_specs.get_mut(&channel_name) {
                    match action {
                        Some("send") => spec.publish_schema = schema,
                        Some("receive") => {
                            spec.subscribe_schema = schema;
                            spec.subscribe_example = example;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(Self { channels: channel_specs })
    }

    /// Return the names of all declared channels.
    pub fn channel_names(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }

    /// Process one incoming WS message and produce one structured response.
    pub fn handle_message(&self, text: &str, seed: u64) -> WsOutcome {
        let raw_message: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(error) => {
                return WsOutcome::Error {
                    errors: vec![ValidationIssue {
                        instance_pointer: "/".to_owned(),
                        schema_pointer: "#".to_owned(),
                        keyword: "json".to_owned(),
                        message: format!("invalid websocket json message: {error}"),
                    }],
                };
            }
        };

        let resolved = match self.resolve_channel_and_payload(&raw_message) {
            Ok(value) => value,
            Err(errors) => return WsOutcome::Error { errors },
        };

        let Some(channel) = self.channels.get(&resolved.channel_name) else {
            return WsOutcome::Error {
                errors: vec![ValidationIssue {
                    instance_pointer: "/channel".to_owned(),
                    schema_pointer: "#/channels".to_owned(),
                    keyword: "enum".to_owned(),
                    message: format!("unknown channel '{}'", resolved.channel_name),
                }],
            };
        };

        if let Some(publish_schema) = &channel.publish_schema {
            match validate_instance(publish_schema, &resolved.payload) {
                Ok(issues) if !issues.is_empty() => return WsOutcome::Error { errors: issues },
                Ok(_issues) => {}
                Err(error) => {
                    return WsOutcome::Error {
                        errors: vec![ValidationIssue {
                            instance_pointer: "/payload".to_owned(),
                            schema_pointer: "#".to_owned(),
                            keyword: "schema".to_owned(),
                            message: error.to_string(),
                        }],
                    };
                }
            }
        }

        if let Some(example) = &channel.subscribe_example {
            return WsOutcome::Mock { channel: resolved.channel_name, payload: example.clone() };
        }

        if let Some(schema) = &channel.subscribe_schema {
            let derived_seed = crate::deterministic_hash(seed, &resolved.channel_name);
            match generate_json_value(schema, derived_seed) {
                Ok(payload) => WsOutcome::Mock { channel: resolved.channel_name, payload },
                Err(error) => WsOutcome::Error {
                    errors: vec![ValidationIssue {
                        instance_pointer: "/payload".to_owned(),
                        schema_pointer: "#".to_owned(),
                        keyword: "faker".to_owned(),
                        message: error.to_string(),
                    }],
                },
            }
        } else {
            WsOutcome::Mock {
                channel: resolved.channel_name,
                payload: Value::Object(serde_json::Map::new()),
            }
        }
    }

    fn resolve_channel_and_payload(
        &self,
        raw_message: &Value,
    ) -> Result<ResolvedIncomingMessage, Vec<ValidationIssue>> {
        if let Some(explicit) = parse_explicit_envelope(raw_message) {
            return Ok(explicit);
        }

        // Auto route: treat the whole message as payload and match by publish schema.
        let mut matched_channels = Vec::new();
        for (channel_name, channel_spec) in &self.channels {
            if let Some(schema) = &channel_spec.publish_schema {
                if let Ok(issues) = validate_instance(schema, raw_message) &&
                    issues.is_empty()
                {
                    matched_channels.push(channel_name.clone());
                }
            } else {
                matched_channels.push(channel_name.clone());
            }
        }

        match matched_channels.len() {
            1 => Ok(ResolvedIncomingMessage {
                channel_name: matched_channels[0].clone(),
                payload: raw_message.clone(),
            }),
            0 => Err(vec![ValidationIssue {
                instance_pointer: "/".to_owned(),
                schema_pointer: "#/channels".to_owned(),
                keyword: "routing".to_owned(),
                message: "unable to route websocket message to any channel".to_owned(),
            }]),
            _ => Err(vec![ValidationIssue {
                instance_pointer: "/".to_owned(),
                schema_pointer: "#/channels".to_owned(),
                keyword: "routing".to_owned(),
                message: format!(
                    "ambiguous websocket message, matched multiple channels: {}",
                    matched_channels.join(", ")
                ),
            }]),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedIncomingMessage {
    channel_name: String,
    payload: Value,
}

fn parse_explicit_envelope(raw: &Value) -> Option<ResolvedIncomingMessage> {
    let object = raw.as_object()?;
    let channel_name =
        object.get("channel").or_else(|| object.get("topic")).and_then(Value::as_str)?.to_owned();
    let payload = object
        .get("payload")
        .or_else(|| object.get("data"))
        .or_else(|| object.get("message"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    Some(ResolvedIncomingMessage { channel_name, payload })
}

fn extract_subscribe_payload(channel_node: &Value) -> (Option<Value>, Option<Value>) {
    let Some(subscribe) = channel_node.get("subscribe") else {
        return (None, None);
    };
    let Some(message) = subscribe.get("message") else {
        return (None, None);
    };
    let schema = message.get("payload").cloned();
    let example = message.get("example").cloned().or_else(|| {
        message.get("examples").and_then(Value::as_array).and_then(|items| items.first().cloned())
    });
    (schema, example)
}

fn extract_message_payload_schema(channel_node: &Value, op_name: &str) -> Option<Value> {
    let operation = channel_node.get(op_name)?;
    let message = operation.get("message")?;
    message.get("payload").cloned()
}

/// Extract the first message's payload schema and example from a v3 operation.
///
/// In AsyncAPI v3, `operation.messages` is an array of message objects
/// (already inlined by `RefResolver`).  We take the first entry.
fn extract_v3_operation_messages(op_node: &Value) -> (Option<Value>, Option<Value>) {
    let first_message = op_node.get("messages").and_then(Value::as_array).and_then(|m| m.first());

    let schema = first_message.and_then(|m| m.get("payload").cloned());
    let example = first_message
        .and_then(|m| m.get("examples").and_then(Value::as_array))
        .and_then(|examples| examples.first())
        .and_then(|ex| ex.get("payload").cloned());

    (schema, example)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AsyncApiRuntime, WsOutcome};

    #[expect(clippy::panic, reason = "test helper: panics on unexpected build failure")]
    fn build_v2_runtime() -> AsyncApiRuntime {
        let root = json!({
            "asyncapi": "2.3.0",
            "info": {"title": "ws", "version": "1.0.0"},
            "channels": {
                "chat.send": {
                    "publish": {
                        "message": {
                            "payload": {
                                "type": "object",
                                "required": ["room", "text"],
                                "properties": {
                                    "room": {"type": "string"},
                                    "text": {"type": "string"}
                                }
                            }
                        }
                    },
                    "subscribe": {
                        "message": {
                            "example": {"ok": true}
                        }
                    }
                },
                "metric.push": {
                    "publish": {
                        "message": {
                            "payload": {
                                "type": "object",
                                "required": ["metric", "value"],
                                "properties": {
                                    "metric": {"type": "string"},
                                    "value": {"type": "number"}
                                }
                            }
                        }
                    },
                    "subscribe": {
                        "message": {
                            "example": {"accepted": true}
                        }
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root);
        match runtime {
            Ok(value) => value,
            Err(error) => panic!("failed to build runtime: {error}"),
        }
    }

    /// Build a v3 runtime with the same logical channels as v2 but using
    /// the AsyncAPI v3 structure (separate channels + operations).
    ///
    /// After `RefResolver` inlining the `$ref` values get replaced with
    /// the full channel/message objects, so we simulate that here.
    #[expect(clippy::panic, reason = "test helper: panics on unexpected build failure")]
    fn build_v3_runtime() -> AsyncApiRuntime {
        let chat_message_payload = json!({
            "type": "object",
            "required": ["room", "text"],
            "properties": {
                "room": {"type": "string"},
                "text": {"type": "string"}
            }
        });
        let chat_reply_payload = json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            }
        });

        // Build the channel object that both the channels map and the
        // inlined operation.channel will point to *the same* value.
        let chat_channel = json!({
            "messages": {
                "chatMessage": {
                    "payload": chat_message_payload
                },
                "chatReply": {
                    "payload": chat_reply_payload,
                    "examples": [{"payload": {"ok": true}}]
                }
            }
        });

        let root = json!({
            "asyncapi": "3.0.0",
            "info": {"title": "ws", "version": "1.0.0"},
            "channels": {
                "chatChannel": chat_channel
            },
            "operations": {
                "sendChat": {
                    "action": "send",
                    "channel": chat_channel,
                    "messages": [
                        {"payload": chat_message_payload}
                    ]
                },
                "receiveReply": {
                    "action": "receive",
                    "channel": chat_channel,
                    "messages": [
                        {
                            "payload": chat_reply_payload,
                            "examples": [{"payload": {"ok": true}}]
                        }
                    ]
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root);
        match runtime {
            Ok(value) => value,
            Err(error) => panic!("failed to build v3 runtime: {error}"),
        }
    }

    #[test]
    #[expect(clippy::panic, reason = "test assertion: unexpected outcome variant")]
    fn explicit_channel_message_is_supported() {
        let runtime = build_v2_runtime();
        let input = json!({
            "channel": "chat.send",
            "payload": {"room": "general", "text": "hello"}
        });
        let text = input.to_string();
        let outcome = runtime.handle_message(&text, 1);
        match outcome {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "chat.send");
                assert_eq!(payload, json!({"ok": true}));
            }
            WsOutcome::Error { .. } => panic!("expected mock outcome"),
        }
    }

    #[test]
    #[expect(clippy::panic, reason = "test assertion: unexpected outcome variant")]
    fn auto_routing_by_schema_is_supported() {
        let runtime = build_v2_runtime();
        let input = json!({"metric": "cpu", "value": 0.95});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        match outcome {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "metric.push");
                assert_eq!(payload, json!({"accepted": true}));
            }
            WsOutcome::Error { .. } => panic!("expected mock outcome"),
        }
    }

    // ── AsyncAPI v3 tests ──────────────────────────────────────────────

    #[test]
    #[expect(clippy::panic, reason = "test assertion: unexpected outcome variant")]
    fn v3_explicit_channel_message_returns_example() {
        let runtime = build_v3_runtime();
        let input = json!({
            "channel": "chatChannel",
            "payload": {"room": "general", "text": "hello"}
        });
        let outcome = runtime.handle_message(&input.to_string(), 1);
        match outcome {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "chatChannel");
                assert_eq!(payload, json!({"ok": true}));
            }
            other @ WsOutcome::Error { .. } => panic!("expected mock outcome, got {other:?}"),
        }
    }

    #[test]
    #[expect(clippy::panic, reason = "test assertion: unexpected outcome variant")]
    fn v3_auto_routing_by_publish_schema() {
        let runtime = build_v3_runtime();
        let input = json!({"room": "general", "text": "hello"});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        match outcome {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "chatChannel");
                assert_eq!(payload, json!({"ok": true}));
            }
            other @ WsOutcome::Error { .. } => panic!("expected mock outcome, got {other:?}"),
        }
    }

    #[test]
    #[expect(clippy::panic, reason = "test assertion: unexpected outcome variant")]
    fn v3_validation_rejects_bad_payload() {
        let runtime = build_v3_runtime();
        let input = json!({
            "channel": "chatChannel",
            "payload": {"room": 123}
        });
        let outcome = runtime.handle_message(&input.to_string(), 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert!(!errors.is_empty(), "expected validation errors");
            }
            other @ WsOutcome::Mock { .. } => panic!("expected error outcome, got {other:?}"),
        }
    }

    #[test]
    fn v3_missing_channels_returns_error() {
        let root = json!({
            "asyncapi": "3.0.0",
            "info": {"title": "bad", "version": "1.0.0"}
        });
        let result = AsyncApiRuntime::from_document(root);
        assert!(result.is_err());
    }

    #[test]
    fn channel_names_returns_all_channels() {
        let runtime = build_v2_runtime();
        let mut names = runtime.channel_names();
        names.sort();
        assert_eq!(names, vec!["chat.send", "metric.push"]);
    }

    #[test]
    fn v3_channel_names_returns_all_channels() {
        let runtime = build_v3_runtime();
        let names = runtime.channel_names();
        assert_eq!(names, vec!["chatChannel"]);
    }

    // ── message handling ───────────────────────────────────────────────

    #[test]
    fn invalid_json_message_is_reported_as_a_parse_error() {
        let runtime = build_v2_runtime();
        let outcome = runtime.handle_message("not json at all", 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].keyword, "json");
                assert_eq!(errors[0].instance_pointer, "/");
                assert!(
                    errors[0].message.contains("invalid websocket json message"),
                    "{:?}",
                    errors[0]
                );
            }
            other @ WsOutcome::Mock { .. } => panic!("expected an error outcome, got {other:?}"),
        }
    }

    #[test]
    fn empty_message_body_is_reported() {
        let runtime = build_v2_runtime();
        let outcome = runtime.handle_message("", 1);
        assert!(
            matches!(outcome, WsOutcome::Error { ref errors } if errors[0].keyword == "json"),
            "an empty string is not valid JSON"
        );
    }

    #[test]
    fn unknown_channel_is_reported() {
        let runtime = build_v2_runtime();
        let input = json!({"channel": "does.not.exist", "payload": {}});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert_eq!(errors[0].keyword, "enum");
                assert!(errors[0].message.contains("unknown channel"), "{:?}", errors[0]);
            }
            other @ WsOutcome::Mock { .. } => panic!("expected an error outcome, got {other:?}"),
        }
    }

    #[test]
    fn unroutable_message_is_reported() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "typed": {
                    "publish": {
                        "message": {
                            "payload": {
                                "type": "object",
                                "required": ["only_this"],
                                "properties": {"only_this": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"totally": "different"}).to_string(), 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert_eq!(errors[0].keyword, "routing");
                assert!(errors[0].message.contains("unable to route"), "{:?}", errors[0]);
            }
            other @ WsOutcome::Mock { .. } => panic!("expected an error outcome, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_message_is_reported() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "left": {},
                "right": {}
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        // Both channels have no publish schema, so anything matches both.
        let outcome = runtime.handle_message(&json!({"a": 1}).to_string(), 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert_eq!(errors[0].keyword, "routing");
                assert!(errors[0].message.contains("ambiguous"), "{:?}", errors[0]);
                assert!(errors[0].message.contains("left"), "both channels are named");
                assert!(errors[0].message.contains("right"), "both channels are named");
            }
            other @ WsOutcome::Mock { .. } => panic!("expected an error outcome, got {other:?}"),
        }
    }

    #[test]
    fn topic_is_accepted_as_an_alias_for_channel() {
        let runtime = build_v2_runtime();
        let input = json!({"topic": "chat.send", "payload": {"room": "r", "text": "t"}});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        assert!(
            matches!(outcome, WsOutcome::Mock { ref channel, .. } if channel == "chat.send"),
            "`topic` must alias `channel`"
        );
    }

    #[test]
    fn data_is_accepted_as_an_alias_for_payload() {
        let runtime = build_v2_runtime();
        let input = json!({"channel": "chat.send", "data": {"room": "r", "text": "t"}});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        assert!(
            matches!(outcome, WsOutcome::Mock { ref channel, .. } if channel == "chat.send"),
            "`data` must alias `payload`"
        );
    }

    #[test]
    fn message_key_is_accepted_as_an_alias_for_payload() {
        let runtime = build_v2_runtime();
        let input = json!({"channel": "chat.send", "message": {"room": "r", "text": "t"}});
        let outcome = runtime.handle_message(&input.to_string(), 1);
        assert!(matches!(outcome, WsOutcome::Mock { .. }), "`message` must alias `payload`");
    }

    #[test]
    fn envelope_without_a_payload_defaults_to_an_empty_object() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {"bare": {}}
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"channel": "bare"}).to_string(), 1);
        assert_eq!(
            outcome,
            WsOutcome::Mock { channel: "bare".to_owned(), payload: json!({}) },
            "a missing payload defaults to an empty object"
        );
    }

    #[test]
    fn non_object_envelope_falls_back_to_auto_routing() {
        let runtime = build_v2_runtime();
        // A bare string is not an envelope, so it is auto-routed by publish schema.
        let outcome = runtime.handle_message(&json!("metric.push").to_string(), 1);
        // `"metric.push"` does not satisfy either publish schema, so it fails to route.
        match outcome {
            WsOutcome::Error { errors } => assert_eq!(errors[0].keyword, "routing"),
            other @ WsOutcome::Mock { .. } => {
                panic!("expected a routing error, got {other:?}")
            }
        }
    }

    #[test]
    fn channel_with_a_non_string_name_is_not_an_envelope() {
        let runtime = build_v2_runtime();
        let input = json!({"channel": 42, "payload": {"room": "r", "text": "t"}});

        // `channel` is not a string, so this is not treated as an envelope and
        // the whole object is auto-routed against each publish schema instead.
        // It satisfies neither, so routing fails rather than silently using the
        // numeric value as a channel name.
        match runtime.handle_message(&input.to_string(), 1) {
            WsOutcome::Error { errors } => {
                assert_eq!(errors[0].keyword, "routing");
                assert!(
                    errors[0].message.contains("unable to route"),
                    "expected an unroutable message, got {:?}",
                    errors[0]
                );
            }
            other @ WsOutcome::Mock { .. } => {
                panic!("a non-string channel name must not be accepted; got {other:?}")
            }
        }
    }

    #[test]
    fn subscribe_schema_without_an_example_is_generated() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "gen": {
                    "subscribe": {
                        "message": {
                            "payload": {
                                "type": "object",
                                "required": ["ack"],
                                "properties": {"ack": {"type": "boolean"}}
                            }
                        }
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"channel": "gen"}).to_string(), 1);
        match outcome {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "gen");
                assert!(payload.get("ack").is_some(), "the subscribe schema must be generated");
            }
            other @ WsOutcome::Error { .. } => panic!("expected a mock outcome, got {other:?}"),
        }
    }

    #[test]
    fn subscribe_examples_array_supplies_the_example() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "arr": {
                    "subscribe": {
                        "message": {"examples": [{"n": 1}, {"n": 2}]}
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"channel": "arr"}).to_string(), 1);
        assert_eq!(
            outcome,
            WsOutcome::Mock { channel: "arr".to_owned(), payload: json!({"n": 1}) }
        );
    }

    #[test]
    fn subscribe_example_beats_the_subscribe_schema() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "both": {
                    "subscribe": {
                        "message": {
                            "payload": {"type": "object", "required": ["generated"]},
                            "example": {"static": true}
                        }
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"channel": "both"}).to_string(), 1);
        assert_eq!(
            outcome,
            WsOutcome::Mock { channel: "both".to_owned(), payload: json!({"static": true}) }
        );
    }

    #[test]
    fn faker_failure_is_reported_as_a_faker_error() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "bad": {
                    "subscribe": {"message": {"payload": {"type": "banana"}}}
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        let outcome = runtime.handle_message(&json!({"channel": "bad"}).to_string(), 1);
        match outcome {
            WsOutcome::Error { errors } => {
                assert_eq!(errors[0].keyword, "faker");
                assert_eq!(errors[0].instance_pointer, "/payload");
            }
            other @ WsOutcome::Mock { .. } => panic!("expected an error outcome, got {other:?}"),
        }
    }

    #[test]
    fn mock_payloads_are_deterministic_for_a_seed() {
        let root = json!({
            "asyncapi": "2.3.0",
            "channels": {
                "det": {
                    "subscribe": {
                        "message": {
                            "payload": {"type": "string", "minLength": 12, "maxLength": 12}
                        }
                    }
                }
            }
        });
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        let message = json!({"channel": "det"}).to_string();

        let first = runtime.handle_message(&message, 5);
        let second = runtime.handle_message(&message, 5);
        assert_eq!(first, second, "the same seed must reproduce the same payload");

        let other = runtime.handle_message(&message, 6);
        assert_ne!(first, other, "a different seed must produce a different payload");
    }

    #[test]
    fn channel_without_any_declaration_returns_an_empty_payload() {
        let root = json!({"asyncapi": "2.3.0", "channels": {"bare": {}}});
        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        assert_eq!(runtime.channel_names(), vec!["bare"]);
        assert_eq!(
            runtime.handle_message(&json!({"channel": "bare"}).to_string(), 1),
            WsOutcome::Mock { channel: "bare".to_owned(), payload: json!({}) }
        );
    }

    #[test]
    fn empty_channel_map_is_allowed() {
        let runtime = AsyncApiRuntime::from_document(json!({"asyncapi": "2.3.0", "channels": {}}))
            .expect("an empty channel map is valid");
        assert!(runtime.channel_names().is_empty());

        let outcome = runtime.handle_message(&json!({"a": 1}).to_string(), 1);
        assert!(
            matches!(outcome, WsOutcome::Error { .. }),
            "with no channels nothing can be routed"
        );
    }

    // ── v2 document parsing ────────────────────────────────────────────

    #[test]
    fn v2_missing_channels_is_rejected() {
        let result = AsyncApiRuntime::from_document(json!({"asyncapi": "2.3.0"}));
        assert!(result.is_err());
    }

    #[test]
    fn non_object_channels_is_rejected() {
        let result = AsyncApiRuntime::from_document(json!({"asyncapi": "2.3.0", "channels": []}));
        assert!(result.is_err());
    }

    #[test]
    fn missing_version_defaults_to_v2() {
        let runtime = AsyncApiRuntime::from_document(json!({
            "channels": {"a": {"publish": {"message": {"payload": {"type": "string"}}}}}
        }))
        .expect("a version-less document is treated as v2");
        assert_eq!(runtime.channel_names(), vec!["a"]);
    }

    #[test]
    fn malformed_channel_nodes_yield_empty_schemas() {
        let runtime = AsyncApiRuntime::from_document(json!({
            "asyncapi": "2.3.0",
            "channels": {
                "a": {"publish": "not an object"},
                "b": {"publish": {"message": "not an object"}},
                "c": {"subscribe": {}},
                "d": {"subscribe": {"message": {}}}
            }
        }))
        .expect("malformed channels must not abort parsing");

        assert_eq!(runtime.channel_names().len(), 4);
        for channel in ["a", "b", "c", "d"] {
            assert_eq!(
                runtime.handle_message(&json!({"channel": channel}).to_string(), 1),
                WsOutcome::Mock { channel: channel.to_owned(), payload: json!({}) },
                "{channel} should degrade to an empty mock"
            );
        }
    }

    // ── v3 document parsing ────────────────────────────────────────────

    #[test]
    fn v3_operation_with_an_unknown_channel_is_skipped() {
        let channel = json!({"messages": {"m": {"payload": {"type": "string"}}}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"known": channel},
            "operations": {
                "op": {"action": "send", "channel": {"never": "declared"}}
            }
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        assert_eq!(runtime.channel_names(), vec!["known"]);
        assert_eq!(
            runtime.handle_message(&json!({"channel": "known"}).to_string(), 1),
            WsOutcome::Mock { channel: "known".to_owned(), payload: json!({}) },
            "an operation pointing at an undeclared channel must be ignored"
        );
    }

    #[test]
    fn v3_operations_are_optional() {
        let runtime = AsyncApiRuntime::from_document(json!({
            "asyncapi": "3.0.0",
            "channels": {"only": {"messages": {}}}
        }))
        .expect("a channel-only document is valid");
        assert_eq!(runtime.channel_names(), vec!["only"]);
    }

    #[test]
    fn v3_unknown_action_is_ignored() {
        let channel = json!({"messages": {}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"c": channel},
            "operations": {
                "op": {"action": "broadcast", "channel": channel},
                "no_action": {"channel": channel}
            }
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        assert_eq!(
            runtime.handle_message(&json!({"channel": "c"}).to_string(), 1),
            WsOutcome::Mock { channel: "c".to_owned(), payload: json!({}) },
            "an unrecognized action must not populate either schema"
        );
    }

    #[test]
    fn v3_send_populates_the_publish_schema_only() {
        let channel = json!({"messages": {}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"c": channel},
            "operations": {
                "op": {
                    "action": "send",
                    "channel": channel,
                    "messages": [{"payload": {
                        "type": "object",
                        "required": ["out"],
                        "properties": {"out": {"type": "integer"}}
                    }}]
                }
            }
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");

        // The publish schema rejects a payload missing `out`.
        let outcome =
            runtime.handle_message(&json!({"channel": "c", "payload": {}}).to_string(), 1);
        assert!(matches!(outcome, WsOutcome::Error { .. }), "the send schema must be enforced");

        // There is no subscribe schema, so a valid message yields an empty mock.
        let valid = json!({"channel": "c", "payload": {"out": 1}}).to_string();
        assert_eq!(
            runtime.handle_message(&valid, 1),
            WsOutcome::Mock { channel: "c".to_owned(), payload: json!({}) }
        );
    }

    #[test]
    fn v3_only_the_first_message_is_used() {
        let channel = json!({"messages": {}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"c": channel},
            "operations": {
                "op": {
                    "action": "receive",
                    "channel": channel,
                    "messages": [
                        {
                            "payload": {
                                "type": "object",
                                "required": ["first"],
                                "properties": {"first": {"type": "integer"}}
                            }
                        },
                        {
                            "payload": {
                                "type": "object",
                                "required": ["second"],
                                "properties": {"second": {"type": "integer"}}
                            }
                        }
                    ]
                }
            }
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        match runtime.handle_message(&json!({"channel": "c"}).to_string(), 1) {
            WsOutcome::Mock { channel, payload } => {
                assert_eq!(channel, "c");
                assert!(
                    payload.get("first").is_some(),
                    "only the first declared message contributes to the mock; got {payload}"
                );
                assert!(
                    payload.get("second").is_none(),
                    "the second message must be ignored; got {payload}"
                );
            }
            other @ WsOutcome::Error { .. } => panic!("expected a mock outcome, got {other:?}"),
        }
    }

    #[test]
    fn v3_non_array_messages_are_ignored() {
        let channel = json!({"messages": {}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"c": channel},
            "operations": {
                "op": {"action": "send", "channel": channel, "messages": "nope"}
            }
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        assert_eq!(
            runtime.handle_message(&json!({"channel": "c"}).to_string(), 1),
            WsOutcome::Mock { channel: "c".to_owned(), payload: json!({}) }
        );
    }

    #[test]
    fn v3_non_object_operations_are_skipped() {
        let channel = json!({"messages": {}});
        let root = json!({
            "asyncapi": "3.0.0",
            "channels": {"c": channel},
            "operations": {"op": "not an object", "missing_channel": {"action": "send"}}
        });

        let runtime = AsyncApiRuntime::from_document(root).expect("runtime");
        assert_eq!(runtime.channel_names(), vec!["c"]);
    }

    #[test]
    fn v3_missing_channels_is_rejected() {
        let result = AsyncApiRuntime::from_document(json!({"asyncapi": "3.0.0"}));
        assert!(result.is_err());
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

    /// A document with N channels, each declaring a required-string payload.
    fn any_channel_document(count: usize) -> serde_json::Value {
        let mut channels = serde_json::Map::new();
        for index in 0..count {
            channels.insert(
                format!("ch{index}"),
                serde_json::json!({
                    "publish": {
                        "message": {
                            "payload": {
                                "type": "object",
                                "required": ["field"],
                                "properties": {"field": {"type": "integer"}}
                            }
                        }
                    }
                }),
            );
        }
        serde_json::json!({"asyncapi": "2.3.0", "channels": channels})
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Every message must produce a well-formed outcome: either a mock
        /// naming a real channel, or errors that are fully populated. This is
        /// the invariant the WebSocket handler relies on for hostile frames.
        #[test]
        fn handle_message_always_returns_a_well_formed_outcome(
            count in 1usize..4,
            message in any_json(),
            seed in any::<u64>(),
        ) {
            let runtime = AsyncApiRuntime::from_document(any_channel_document(count))
                .expect("runtime");
            let declared: Vec<String> = runtime.channel_names();

            match runtime.handle_message(&message.to_string(), seed) {
                WsOutcome::Mock { channel, payload } => {
                    prop_assert!(
                        declared.contains(&channel),
                        "mock named undeclared channel {channel:?}; declared {declared:?}"
                    );
                    let _ = payload;
                }
                WsOutcome::Error { errors } => {
                    prop_assert!(!errors.is_empty(), "an error outcome must carry at least one issue");
                    for issue in errors {
                        prop_assert!(!issue.keyword.is_empty());
                        prop_assert!(!issue.message.is_empty());
                    }
                }
            }
        }

        /// Whatever raw text arrives on the socket, handling must never panic.
        #[test]
        fn handle_message_never_panics_on_hostile_text(
            count in 1usize..4,
            text in ".{0,40}",
        ) {
            let runtime = AsyncApiRuntime::from_document(any_channel_document(count))
                .expect("runtime");
            let _ = runtime.handle_message(&text, 1);
        }

        /// The same seed always produces the same outcome.
        #[test]
        fn outcomes_are_deterministic_per_seed(
            count in 1usize..4,
            message in any_json(),
            seed in any::<u64>(),
        ) {
            let runtime = AsyncApiRuntime::from_document(any_channel_document(count))
                .expect("runtime");
            let text = message.to_string();
            prop_assert_eq!(
                format!("{:?}", runtime.handle_message(&text, seed)),
                format!("{:?}", runtime.handle_message(&text, seed))
            );
        }

        /// Parsing any AsyncAPI document never panics.
        #[test]
        fn document_parsing_never_panics(document in any_json()) {
            let _ = AsyncApiRuntime::from_document(document);
        }

        /// A declared channel name always round-trips through `channel_names`.
        #[test]
        fn channel_names_round_trip(count in 1usize..5) {
            let document = any_channel_document(count);
            let runtime = AsyncApiRuntime::from_document(document).expect("runtime");

            let mut returned = runtime.channel_names();
            returned.sort();

            let mut expected: Vec<String> = (0..count).map(|index| format!("ch{index}")).collect();
            expected.sort();
            prop_assert_eq!(returned, expected);
        }
    }
}
