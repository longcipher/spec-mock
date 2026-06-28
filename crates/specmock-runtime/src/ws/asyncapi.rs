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
#[derive(Debug, Clone, Serialize)]
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
}
