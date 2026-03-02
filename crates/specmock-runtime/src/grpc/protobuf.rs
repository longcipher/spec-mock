//! Dynamic protobuf gRPC runtime using tonic HTTP/2 transport with proper trailers.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use base64::Engine;
use bytes::Bytes;
use http_body::Frame;
use http_body_util::BodyExt;
use prost::Message;
use prost_reflect::{Cardinality, DescriptorPool, DynamicMessage, Kind, MethodDescriptor, Value};
use serde_json::json;
use tokio::{net::TcpListener, task::JoinHandle};

use crate::RuntimeError;

/// Body type that tonic's `Server::add_service` expects.
type BoxBody = http_body_util::combinators::UnsyncBoxBody<Bytes, tonic::Status>;

/// gRPC runtime state.
#[derive(Debug, Clone)]
pub struct GrpcRuntime {
    methods: Arc<HashMap<String, MethodDescriptor>>,
    seed: u64,
}

impl GrpcRuntime {
    /// Build runtime from config.
    pub async fn from_config(config: &crate::ServerConfig) -> Result<Self, RuntimeError> {
        let proto_spec = config.proto_spec.as_deref().ok_or_else(|| {
            RuntimeError::Config("proto_spec must be set for gRPC runtime".to_owned())
        })?;

        let include_dir = proto_spec.parent().unwrap_or_else(|| Path::new("."));
        let descriptor_set = protox::compile([proto_spec], [include_dir])
            .map_err(|error| RuntimeError::Parse(error.to_string()))?;
        let pool = DescriptorPool::from_file_descriptor_set(descriptor_set)
            .map_err(|error| RuntimeError::Parse(error.to_string()))?;

        let mut methods = HashMap::new();
        for service in pool.services() {
            for method in service.methods() {
                let path = format!("/{}/{}", service.full_name(), method.name());
                methods.insert(path, method);
            }
        }

        Ok(Self { methods: Arc::new(methods), seed: config.seed })
    }
}

// ---------------------------------------------------------------------------
// Tower service that handles all incoming gRPC requests dynamically
// ---------------------------------------------------------------------------

/// A dynamic gRPC service that routes all incoming requests by path, generating
/// mock responses from protobuf descriptors.  Served directly by the hyper
/// HTTP/2 server – every request reaches this service.
#[derive(Debug, Clone)]
struct DynamicGrpcService {
    methods: Arc<HashMap<String, MethodDescriptor>>,
    seed: u64,
}

impl tower::Service<http::Request<hyper::body::Incoming>> for DynamicGrpcService {
    type Response = http::Response<BoxBody>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<hyper::body::Incoming>) -> Self::Future {
        let methods = Arc::clone(&self.methods);
        let seed = self.seed;

        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_owned();

            // --- content-type check ---
            if !is_grpc_content_type(&parts.headers) {
                return Ok(grpc_error_response(3, "content-type must be application/grpc", None));
            }

            // --- method lookup ---
            let Some(method) = methods.get(&path) else {
                return Ok(grpc_error_response(12, "method not found", None));
            };

            // Client-streaming is not supported.
            if method.is_client_streaming() {
                return Ok(grpc_error_response(
                    12,
                    "client streaming methods are not supported in this runtime",
                    None,
                ));
            }

            // --- read body ---
            let body_bytes = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(_error) => {
                    return Ok(grpc_error_response(
                        3,
                        "failed to read grpc request body",
                        Some(json!({
                            "errors": [{
                                "instance_pointer": "/body",
                                "schema_pointer": "#",
                                "keyword": "grpc",
                                "message": "body read failure"
                            }]
                        })),
                    ));
                }
            };

            // --- decode gRPC frame ---
            let request_payload = match decode_grpc_unary_frame(&body_bytes) {
                Ok(value) => value,
                Err(message) => {
                    return Ok(grpc_error_response(
                        3,
                        &message,
                        Some(json!({
                            "errors": [{
                                "instance_pointer": "/body",
                                "schema_pointer": "#",
                                "keyword": "grpc_frame",
                                "message": message
                            }]
                        })),
                    ));
                }
            };

            // --- decode protobuf request ---
            if let Err(error) = DynamicMessage::decode(method.input(), request_payload.as_slice()) {
                return Ok(grpc_error_response(
                    3,
                    "protobuf request decode failed",
                    Some(json!({
                        "errors": [{
                            "instance_pointer": "/body",
                            "schema_pointer": "#",
                            "keyword": "protobuf",
                            "message": error.to_string()
                        }]
                    })),
                ));
            }

            let response_seed = seed ^ hash_path(&path);

            // --- server streaming ---
            if method.is_server_streaming() {
                return Ok(build_streaming_response(method, response_seed));
            }

            // --- unary response ---
            let response_message = match generate_dynamic_message(method.output(), response_seed) {
                Ok(message) => message,
                Err(error) => {
                    return Ok(grpc_error_response(
                        13,
                        "failed to generate protobuf response",
                        Some(json!({
                            "errors": [{
                                "instance_pointer": "/response",
                                "schema_pointer": "#",
                                "keyword": "faker",
                                "message": error
                            }]
                        })),
                    ));
                }
            };

            let encoded = response_message.encode_to_vec();
            let framed = encode_grpc_unary_frame(&encoded);

            Ok(build_success_response(vec![Bytes::from(framed)]))
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP/2 response helpers (proper gRPC trailers)
// ---------------------------------------------------------------------------

/// Number of mock messages to generate for server-streaming methods.
const STREAMING_COUNT: u64 = 3;

/// Build a streaming gRPC response with multiple framed messages.
fn build_streaming_response(method: &MethodDescriptor, base_seed: u64) -> http::Response<BoxBody> {
    let mut frames = Vec::with_capacity(STREAMING_COUNT as usize);
    for i in 0..STREAMING_COUNT {
        match generate_dynamic_message(method.output(), base_seed.wrapping_add(i)) {
            Ok(msg) => {
                let encoded = msg.encode_to_vec();
                frames.push(Bytes::from(encode_grpc_unary_frame(&encoded)));
            }
            Err(error) => {
                return grpc_error_response(
                    13,
                    "failed to generate protobuf response",
                    Some(json!({
                        "errors": [{
                            "instance_pointer": "/response",
                            "schema_pointer": "#",
                            "keyword": "faker",
                            "message": error
                        }]
                    })),
                );
            }
        }
    }
    build_success_response(frames)
}

/// Build a successful gRPC HTTP/2 response with data frame(s) and trailers.
fn build_success_response(data_frames: Vec<Bytes>) -> http::Response<BoxBody> {
    let body = GrpcBody { data_frames, trailers: Some(success_trailers()), index: 0 };
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(boxed(body))
        .unwrap_or_else(|_error| {
            http::Response::new(boxed(http_body_util::Empty::new().map_err(|never| match never {})))
        })
}

/// Build a gRPC error response.  For HTTP/2 this is a "trailers-only" response:
/// response headers + trailers in a single HEADERS frame with `END_STREAM`.
fn grpc_error_response(
    status_code: i32,
    message: &str,
    details_json: Option<serde_json::Value>,
) -> http::Response<BoxBody> {
    let mut trailers = http::HeaderMap::new();
    if let Ok(value) = http::HeaderValue::from_str(&status_code.to_string()) {
        trailers.insert("grpc-status", value);
    }
    if let Ok(value) = http::HeaderValue::from_str(&sanitize_grpc_message(message)) {
        trailers.insert("grpc-message", value);
    }
    if let Some(value) = build_grpc_status_details_bin(status_code, message, details_json.as_ref())
        .and_then(|encoded| http::HeaderValue::from_str(&encoded).ok())
    {
        trailers.insert("grpc-status-details-bin", value);
    }
    if let Some(details) = details_json &&
        let Ok(value) = http::HeaderValue::from_str(&details.to_string())
    {
        trailers.insert("x-specmock-errors", value);
    }

    let body = GrpcBody { data_frames: vec![], trailers: Some(trailers), index: 0 };
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(boxed(body))
        .unwrap_or_else(|_error| {
            http::Response::new(boxed(http_body_util::Empty::new().map_err(|never| match never {})))
        })
}

// ---------------------------------------------------------------------------
// Spawn server
// ---------------------------------------------------------------------------

/// Spawn gRPC server over HTTP/2 using hyper.
///
/// The server accepts TCP connections, upgrades each to HTTP/2 via
/// `hyper_util::server::conn::auto::Builder`, and delegates every request to
/// [`DynamicGrpcService`] which routes by URI path.
pub async fn spawn_grpc_server(
    runtime: GrpcRuntime,
    bind_addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), RuntimeError> {
    let listener = TcpListener::bind(bind_addr).await?;
    let bound = listener.local_addr()?;

    let service = DynamicGrpcService { methods: runtime.methods.clone(), seed: runtime.seed };

    let task = tokio::spawn(async move {
        let builder =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());

        loop {
            tokio::select! {
                () = shutdown.notified() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { continue };
                    let svc = service.clone();
                    let conn_builder = builder.clone();
                    tokio::spawn(async move {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let hyper_svc = hyper_util::service::TowerToHyperService::new(svc);
                        let _result = conn_builder
                            .serve_connection(io, hyper_svc)
                            .await;
                    });
                }
            }
        }
    });

    Ok((bound, task))
}

// ---------------------------------------------------------------------------
// Custom HTTP body that yields data frames followed by a trailers frame
// ---------------------------------------------------------------------------

/// A body implementation that first yields zero or more data frames and then a
/// single trailers frame.  This produces proper HTTP/2 trailers which gRPC
/// clients expect for `grpc-status` / `grpc-message` / `grpc-status-details-bin`.
struct GrpcBody {
    data_frames: Vec<Bytes>,
    trailers: Option<http::HeaderMap>,
    index: usize,
}

impl http_body::Body for GrpcBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        if this.index < this.data_frames.len() {
            let frame = Frame::data(this.data_frames[this.index].clone());
            this.index += 1;
            return Poll::Ready(Some(Ok(frame)));
        }

        if let Some(trailers) = this.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }

        Poll::Ready(None)
    }
}

/// Wrap an `http_body::Body` into a `BoxBody`.
fn boxed<B>(body: B) -> BoxBody
where
    B: http_body::Body<Data = Bytes, Error = tonic::Status> + Send + 'static,
{
    BoxBody::new(body)
}

/// Trailers for a successful gRPC response.
fn success_trailers() -> http::HeaderMap {
    let mut map = http::HeaderMap::new();
    map.insert("grpc-status", http::HeaderValue::from_static("0"));
    map
}

// ---------------------------------------------------------------------------
// gRPC framing helpers
// ---------------------------------------------------------------------------

fn is_grpc_content_type(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/grpc"))
}

fn decode_grpc_unary_frame(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 5 {
        return Err("grpc frame too short".to_owned());
    }
    if bytes[0] != 0 {
        return Err("compressed grpc payload is not supported".to_owned());
    }
    let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if bytes.len() < length + 5 {
        return Err("grpc frame length mismatch".to_owned());
    }
    Ok(bytes[5..5 + length].to_vec())
}

fn encode_grpc_unary_frame(payload: &[u8]) -> Vec<u8> {
    let length = payload.len() as u32;
    let mut framed = Vec::with_capacity(payload.len() + 5);
    framed.push(0);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

// ---------------------------------------------------------------------------
// gRPC error encoding helpers
// ---------------------------------------------------------------------------

fn sanitize_grpc_message(message: &str) -> String {
    message
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_string()
            } else if character == ' ' {
                "%20".to_owned()
            } else {
                String::new()
            }
        })
        .collect::<String>()
}

fn build_grpc_status_details_bin(
    status_code: i32,
    message: &str,
    details_json: Option<&serde_json::Value>,
) -> Option<String> {
    let mut status =
        GoogleRpcStatus { code: status_code, message: message.to_owned(), details: Vec::new() };
    if let Some(details) = details_json {
        let field_violations = extract_field_violations(details);
        if field_violations.is_empty() {
            status.details.push(AnyMessage {
                type_url: "type.googleapis.com/specmock.ValidationErrors".to_owned(),
                value: details.to_string().into_bytes(),
            });
        } else {
            let bad_request = GoogleRpcBadRequest { field_violations };
            status.details.push(AnyMessage {
                type_url: "type.googleapis.com/google.rpc.BadRequest".to_owned(),
                value: bad_request.encode_to_vec(),
            });
        }
    }

    let encoded = status.encode_to_vec();
    Some(base64::engine::general_purpose::STANDARD.encode(encoded))
}

#[derive(Clone, PartialEq, prost::Message)]
struct GoogleRpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<AnyMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AnyMessage {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GoogleRpcBadRequest {
    #[prost(message, repeated, tag = "1")]
    field_violations: Vec<GoogleRpcBadRequestFieldViolation>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GoogleRpcBadRequestFieldViolation {
    #[prost(string, tag = "1")]
    field: String,
    #[prost(string, tag = "2")]
    description: String,
}

fn extract_field_violations(
    details_json: &serde_json::Value,
) -> Vec<GoogleRpcBadRequestFieldViolation> {
    let Some(errors) = details_json.get("errors").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in errors {
        let instance_pointer =
            item.get("instance_pointer").and_then(serde_json::Value::as_str).unwrap_or("/");
        let field = pointer_to_field_path(instance_pointer);
        let description = item
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "request validation failed".to_owned(), ToOwned::to_owned);
        out.push(GoogleRpcBadRequestFieldViolation { field, description });
    }
    out
}

fn pointer_to_field_path(pointer: &str) -> String {
    if pointer.is_empty() || pointer == "/" {
        return "body".to_owned();
    }

    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

// ---------------------------------------------------------------------------
// Mock data generation
// ---------------------------------------------------------------------------

fn generate_dynamic_message(
    descriptor: prost_reflect::MessageDescriptor,
    seed: u64,
) -> Result<DynamicMessage, String> {
    let mut message = DynamicMessage::new(descriptor.clone());
    let mut oneof_taken = std::collections::HashSet::new();

    for field in descriptor.fields() {
        if let Some(oneof) = field.containing_oneof() {
            let name = oneof.full_name().to_owned();
            if oneof_taken.contains(&name) {
                continue;
            }
            oneof_taken.insert(name);
        }

        if field.is_map() {
            message
                .try_set_field(&field, Value::Map(HashMap::new()))
                .map_err(|error| error.to_string())?;
            continue;
        }

        if field.is_list() {
            let item = scalar_value_for_field(&field.kind(), seed ^ u64::from(field.number()))?;
            message
                .try_set_field(&field, Value::List(vec![item]))
                .map_err(|error| error.to_string())?;
            continue;
        }

        if field.cardinality() == Cardinality::Optional ||
            field.cardinality() == Cardinality::Required ||
            field.supports_presence()
        {
            let value = scalar_value_for_field(&field.kind(), seed ^ u64::from(field.number()))?;
            message.try_set_field(&field, value).map_err(|error| error.to_string())?;
        }
    }

    Ok(message)
}

fn scalar_value_for_field(kind: &Kind, seed: u64) -> Result<Value, String> {
    match kind {
        Kind::Bool => Ok(Value::Bool((seed & 1) == 1)),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Ok(Value::I32((seed % 2048) as i32)),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Ok(Value::I64((seed % 1_000_000) as i64)),
        Kind::Uint32 | Kind::Fixed32 => Ok(Value::U32((seed % 2048) as u32)),
        Kind::Uint64 | Kind::Fixed64 => Ok(Value::U64(seed % 1_000_000)),
        Kind::Float => Ok(Value::F32(((seed % 10_000) as f32) / 100.0)),
        Kind::Double => Ok(Value::F64(((seed % 10_000) as f64) / 100.0)),
        Kind::String => Ok(Value::String(format!("mock-{seed}"))),
        Kind::Bytes => Ok(Value::Bytes(bytes::Bytes::from(format!("mock-{seed}")))),
        Kind::Enum(enum_descriptor) => {
            let first =
                enum_descriptor.values().next().ok_or_else(|| "enum has no values".to_owned())?;
            Ok(Value::EnumNumber(first.number()))
        }
        Kind::Message(message_descriptor) => {
            let nested = generate_dynamic_message(message_descriptor.clone(), seed + 1)?;
            Ok(Value::Message(nested))
        }
    }
}

fn hash_path(path: &str) -> u64 {
    path.bytes().fold(0_u64, |acc, byte| acc.wrapping_mul(131).wrapping_add(u64::from(byte)))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use base64::Engine;
    use prost::Message;
    use serde_json::json;

    use super::{GoogleRpcBadRequest, GoogleRpcStatus, build_grpc_status_details_bin};

    #[test]
    fn grpc_status_details_bin_encodes_status_and_details() {
        let details = json!({
            "errors": [{
                "instance_pointer": "/payload/id",
                "schema_pointer": "#/components/schemas/Pet/properties/id",
                "keyword": "type",
                "message": "invalid type"
            }]
        });

        let encoded = build_grpc_status_details_bin(3, "bad request", Some(&details));
        assert!(encoded.is_some(), "expected encoded details");
        let Some(encoded) = encoded else {
            return;
        };

        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded);
        assert!(decoded.is_ok(), "failed to decode base64");
        let Ok(decoded) = decoded else {
            return;
        };

        let status = GoogleRpcStatus::decode(decoded.as_slice());
        assert!(status.is_ok(), "failed to decode protobuf status");
        let Ok(status) = status else {
            return;
        };

        assert_eq!(status.code, 3);
        assert_eq!(status.message, "bad request");
        assert_eq!(status.details.len(), 1);
        assert_eq!(status.details[0].type_url, "type.googleapis.com/google.rpc.BadRequest");

        let bad_request = GoogleRpcBadRequest::decode(status.details[0].value.as_slice());
        assert!(bad_request.is_ok(), "failed to decode google.rpc.BadRequest");
        let Ok(bad_request) = bad_request else {
            return;
        };
        assert_eq!(bad_request.field_violations.len(), 1);
        assert_eq!(bad_request.field_violations[0].field, "payload.id");
        assert!(bad_request.field_violations[0].description.contains("invalid type"));
    }
}
