# Design: Prism Parity Overhaul

| Metadata | Details |
| :--- | :--- |
| **Status** | Draft |
| **Created** | 2026-03-02 |
| **Scope** | Full |
| **Target** | Full Prism feature parity + multi-protocol advantage |

## 1. Executive Summary

spec-mock is a pure-Rust spec-driven mock server supporting OpenAPI HTTP, AsyncAPI WebSocket, and Protobuf gRPC. A technical review identified critical gaps that prevent it from replacing Stoplight Prism for production OpenAPI mocking: incomplete `$ref` resolution, non-standard gRPC transport, limited faker coverage, and missing HTTP protocol features (content negotiation, `Prefer` header, RFC 7807 errors, multi-value query parameters).

This design overhauls every layer of spec-mock to achieve full Prism feature parity for OpenAPI mocking while preserving spec-mock's unique multi-protocol and Rust SDK advantages. The gRPC runtime is rebuilt on `tonic` for standard HTTP/2 + trailers compliance. The core layer gains a complete `$ref` resolver, regex-based string generation, and full JSON Schema format coverage. The HTTP runtime adds content negotiation, `Prefer: code/example` header support, RFC 7807 error responses, and proper proxy header forwarding. Implementation bugs (memory leak, unbounded body reads, deprecated dependencies) are fixed throughout.

## 2. Requirements & Goals

### 2.1 Functional Requirements

| ID | Requirement | Priority |
| --- | --- | --- |
| F01 | Complete `$ref` resolution: local (`#/...`), file-relative (`./file.yaml#/...`), and URL-based | Critical |
| F02 | Nested `$ref` resolution inside `normalize_schema` and all schema processing paths | Critical |
| F03 | gRPC runtime rebuilt on `tonic` with proper HTTP/2, trailers, and streaming support | Critical |
| F04 | Server-streaming and client-streaming gRPC mock support | High |
| F05 | Faker: `pattern` (regex) string generation via `rand_regex` or equivalent | High |
| F06 | Faker: complete `format` coverage (`uri`, `hostname`, `ipv4`, `ipv6`, `byte`, `binary`, `password`, `time`) | High |
| F07 | Faker: `discriminator` + `mapping` support for polymorphic schemas | High |
| F08 | Faker: `additionalProperties` generation | Medium |
| F09 | Faker: `default` value in generation priority chain | Medium |
| F10 | AsyncAPI v3.0 support (`send`/`receive` semantics) alongside v2.x | High |
| F11 | Multi-path WebSocket routing per AsyncAPI channel | Medium |
| F12 | Content negotiation: respect `Accept` header, support multiple media types per response | High |
| F13 | `Prefer: code=xxx` and `Prefer: example=xxx` header for dynamic response/example selection | High |
| F14 | RFC 7807 Problem Details JSON error format | Medium |
| F15 | Multi-value query parameter support (`style`/`explode` per OpenAPI) | High |
| F16 | Request body Content-Type validation with 415 response | Medium |
| F17 | Proxy mode: correct `Host` header forwarding to upstream | Medium |
| F18 | HTTP request body size limit (configurable, default 10 MiB) | Medium |
| F19 | Efficient path matching using radix tree | Medium |
| F20 | Callback / Webhook endpoint support (outbound mock calls) | Low |
| F21 | Migrate from deprecated `serde_yaml` to `serde_yml` | Medium |
| F22 | Remove `common` crate template residue or repurpose | Low |
| F23 | Comprehensive test specs covering real-world complexity | High |

### 2.2 Non-Functional Requirements

| ID | Requirement |
| --- | --- |
| NF01 | All changes follow AGENTS.md rules: `thiserror` in libraries, `eyre` in applications, `tracing` for logging, no `anyhow`/`reqwest`/`dashmap`/`log` |
| NF02 | No `unsafe` unless strictly required and documented |
| NF03 | Performance: path matching O(log n), validation overhead < 5ms for typical schemas |
| NF04 | Memory: no leaks in server lifecycle (fix `Box::leak`) |
| NF05 | Dependency versions follow AGENTS.md preferred versions |
| NF06 | `clippy::pedantic` + `clippy::nursery` clean |
| NF07 | All public APIs documented |

### 2.3 Assumptions

- A01: `rand_regex` (or `regex_generate`) crate is suitable for deterministic regex-based string generation with seed support. If not, a custom regex subset generator will be built.
- A02: `tonic` 0.13+ is compatible with the current `prost` 0.14 / `prost-reflect` 0.16 versions. If not, versions will be aligned.
- A03: URL-based `$ref` (F01) will be fetched with `hpx` and cached in memory. No filesystem sandbox or auth headers are required for remote refs in MVP.
- A04: Callback/Webhook support (F20) is limited to outbound HTTP calls triggered by matched operations — full event subscription model is out of scope.

### 2.4 Out of Scope

- OpenAPI 2.0 (Swagger) support
- OpenAPI code generation
- GUI / dashboard
- Authentication enforcement (spec-mock only validates structure, not tokens)
- Load testing / performance benchmarking mode

## 3. Architecture Overview

### 3.1 Current Architecture

```text
┌──────────────┐     ┌──────────────────┐     ┌──────────────┐     ┌──────────┐
│  spec-mock   │────▶│  specmock-sdk    │────▶│  specmock-   │────▶│ specmock-│
│  CLI (bin)   │     │  (embed/process) │     │  runtime     │     │ core     │
└──────────────┘     └──────────────────┘     │  ┌─http/     │     │ contract │
                                              │  │ openapi   │     │ error    │
                                              │  ├─ws/       │     │ faker    │
                                              │  │ asyncapi  │     │ validate │
                                              │  ├─grpc/     │     └──────────┘
                                              │  │ protobuf  │
                                              │  └───────────│
                                              └──────────────┘
```

### 3.2 Target Architecture

```text
┌──────────────┐     ┌──────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│  spec-mock   │────▶│  specmock-sdk    │────▶│  specmock-       │────▶│ specmock-core    │
│  CLI (bin)   │     │  (embed/process) │     │  runtime         │     │                  │
└──────────────┘     └──────────────────┘     │  ┌─http/         │     │ contract         │
                                              │  │ openapi       │     │ error (RFC 7807) │
                                              │  │ negotiate     │     │ faker (regex/fmt) │
                                              │  │ prefer        │     │ validate          │
                                              │  │ router (trie) │     │ ref_resolver ★NEW │
                                              │  ├─ws/           │     │ schema ★NEW       │
                                              │  │ asyncapi_v2   │     └──────────────────┘
                                              │  │ asyncapi_v3   │
                                              │  ├─grpc/         │
                                              │  │ tonic_dynamic  │  ← tonic-based
                                              │  │ streaming      │
                                              │  └────────────── │
                                              └──────────────────┘
```

### 3.3 Key Design Principles

1. **`$ref` resolution is a foundational cross-cutting concern.** It must happen once during spec loading, producing a fully-inlined document that all downstream code (validation, faker, routing) can consume without re-resolving references.

2. **gRPC uses tonic's `Routes` / `NamedService` with `prost-reflect` `DynamicMessage`.** This ensures standard HTTP/2, trailers, and streaming compliance. The dynamic service handler implements `tonic::codegen::Service<Request<BoxBody>>` and dispatches based on the descriptor pool.

3. **Error model is RFC 7807.** All HTTP error responses use `application/problem+json` with `type`, `title`, `status`, `detail`, and an `errors` extension array of `ValidationIssue`. The gRPC error model remains `google.rpc.Status` + `BadRequest` details.

4. **Router uses a radix tree.** Path templates are compiled into a trie at spec load time. Matching is O(depth) where depth = number of path segments.

5. **Content negotiation and `Prefer` header are handled as middleware/extractors** before the mock response is generated, so operation handlers always know the target status code and media type.

### 3.4 Existing Components to Reuse

| Component | Location | Reuse Strategy |
| --- | --- | --- |
| `ValidationIssue` struct | `specmock-core/src/error.rs` | Extend with RFC 7807 top-level fields; keep as inner `errors` array |
| `validate_instance()` | `specmock-core/src/validate.rs` | Keep as-is; wrap in ref-resolved schema |
| `generate_json_value()` | `specmock-core/src/faker.rs` | Extend with `pattern`/`format`/`discriminator`/`default`/`additionalProperties` |
| `OpenApiRuntime` parser | `specmock-runtime/src/http/openapi.rs` | Refactor to use `RefResolver` output; extract router into separate module |
| `AsyncApiRuntime` parser | `specmock-runtime/src/ws/asyncapi.rs` | Extend with v3 detection; refactor channel model |
| `MockServerBuilder` | `specmock-sdk/src/server.rs` | Keep API surface; update internals for new gRPC handle |
| `ServerConfig` | `specmock-runtime/src/lib.rs` | Add fields for body limit, callback specs |
| `hpx::Client` | `specmock-runtime/src/http/mod.rs` | Reuse for proxy mode and remote `$ref` fetching |

## 4. Detailed Design

### 4.1 Module: `specmock-core/src/ref_resolver.rs` (NEW)

Responsible for loading a spec document (OpenAPI or AsyncAPI YAML/JSON) and recursively resolving all `$ref` pointers into a single inlined `serde_json::Value`.

```rust
/// Resolved document with all $ref pointers inlined.
pub struct ResolvedDocument {
    /// Fully-inlined JSON value (no $ref nodes remain).
    pub root: Value,
}

/// Reference resolver supporting local, file, and URL references.
pub struct RefResolver {
    /// Base directory for relative file refs.
    base_dir: PathBuf,
    /// Cache of already-loaded external documents (path/URL -> Value).
    cache: HashMap<String, Value>,
    /// HTTP client for URL-based refs (lazy).
    http_client: Option<hpx::Client>,
    /// Maximum resolution depth to prevent cycles.
    max_depth: usize,
}

impl RefResolver {
    pub fn new(base_dir: PathBuf) -> Self;
    pub fn with_http_client(self, client: hpx::Client) -> Self;

    /// Load and fully resolve a spec file.
    pub async fn resolve(&mut self, path: &Path) -> Result<ResolvedDocument, SpecMockCoreError>;

    /// Resolve all $ref nodes in a Value tree.
    async fn resolve_value(
        &mut self,
        value: &mut Value,
        current_base: &Path,
        depth: usize,
    ) -> Result<(), SpecMockCoreError>;
}
```

**Resolution algorithm:**

1. Parse the root document as `serde_json::Value`.
2. Walk the tree recursively. For each node:
   - If the node is an object with a `"$ref"` key:
     - Parse the ref string: `[file_path]#[json_pointer]`
     - If `file_path` is empty → local ref: use JSON pointer on current document root.
     - If `file_path` starts with `http://` or `https://` → fetch with `hpx`, cache, then resolve pointer.
     - Otherwise → resolve as file path relative to `base_dir` of the referring document, load, cache, then resolve pointer.
   - Replace the `$ref` node with the resolved value.
   - Recurse into the resolved value (to handle nested refs).
3. Track visited refs to detect cycles; error on depth > `max_depth` (default 64).

### 4.2 Module: `specmock-core/src/schema.rs` (NEW)

A higher-level schema utility that wraps `normalize_schema` and operates on already-resolved documents.

```rust
/// Normalize an OpenAPI 3.0 schema (nullable → type array).
/// Operates on a fully $ref-resolved document.
pub fn normalize_openapi_schema(schema: Value, is_3_0: bool) -> Value;

/// Extract discriminator info from a schema.
pub fn extract_discriminator(schema: &Value) -> Option<Discriminator>;

pub struct Discriminator {
    pub property_name: String,
    pub mapping: HashMap<String, Value>, // discriminator value -> resolved schema
}
```

### 4.3 Module: `specmock-core/src/faker.rs` (EXTENDED)

**New generation priority chain:**

1. `example` field
2. First value of `examples` array
3. `default` field (NEW)
4. Schema-based synthetic generation (existing + extensions below)

**New capabilities:**

```rust
/// Generate a string matching a regex pattern deterministically.
fn generate_pattern_string(pattern: &str, rng: &mut ChaCha8Rng) -> Result<Value, SpecMockCoreError>;

/// Generate a string for a known format.
fn generate_format_string(format: &str, rng: &mut ChaCha8Rng) -> Value;
// Extended formats: uri, hostname, ipv4, ipv6, byte (base64), binary, password, time,
// date-time, date, email, uuid (existing)

/// Generate value respecting discriminator.
fn generate_with_discriminator(
    schema: &Value,
    discriminator: &Discriminator,
    rng: &mut ChaCha8Rng,
    depth: usize,
) -> Result<Value, SpecMockCoreError>;

/// Generate additional properties for an object.
fn generate_additional_properties(
    schema: &Value,
    rng: &mut ChaCha8Rng,
    depth: usize,
) -> Result<Map<String, Value>, SpecMockCoreError>;
```

**`pattern` generation approach:** Use the `rand_regex` crate which generates random strings matching a regex from an `rand::Rng`. Seed it with our `ChaCha8Rng` for determinism. If `rand_regex` cannot parse the pattern (complex lookahead etc.), fall back to a generic alphanumeric string of `minLength`..`maxLength` and log a warning.

### 4.4 Module: `specmock-core/src/error.rs` (EXTENDED)

Add RFC 7807 Problem Details top-level structure:

```rust
/// RFC 7807 Problem Details response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation.
    pub detail: String,
    /// URI of the request that caused the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Detailed validation errors.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ValidationIssue>,
}
```

### 4.5 Module: `specmock-runtime/src/http/router.rs` (NEW)

Radix-tree based path router:

```rust
/// Compiled route index for O(log n) path matching.
pub struct PathRouter {
    root: RadixNode,
}

struct RadixNode {
    children: Vec<RadixEdge>,
    operations: Vec<(Method, usize)>, // method -> operation index
}

struct RadixEdge {
    segment: SegmentMatcher,
    child: RadixNode,
}

enum SegmentMatcher {
    Literal(String),
    Param(String),
}

impl PathRouter {
    pub fn build(operations: &[OperationSpec]) -> Self;
    pub fn match_route(&self, method: &Method, path: &str) -> Option<RouteMatch>;
}

pub struct RouteMatch {
    pub operation_index: usize,
    pub path_params: HashMap<String, String>,
}
```

### 4.6 Module: `specmock-runtime/src/http/negotiate.rs` (NEW)

Content negotiation and `Prefer` header parsing:

```rust
/// Parsed Prefer header directives relevant to mocking.
pub struct PreferDirectives {
    /// Preferred response status code.
    pub code: Option<u16>,
    /// Preferred example name.
    pub example: Option<String>,
    /// Preferred media type (from Accept header).
    pub media_type: Option<String>,
    /// Dynamic response enabled.
    pub dynamic: bool,
}

impl PreferDirectives {
    pub fn from_headers(headers: &HeaderMap) -> Self;
}

/// Select the best response spec given preferences.
pub fn select_response(
    responses: &[ResponseSpec],
    prefer: &PreferDirectives,
) -> Option<&ResponseSpec>;

/// Select the best media type from response content map.
pub fn negotiate_media_type(
    available: &[String],
    accept_header: Option<&str>,
) -> Option<String>;
```

### 4.7 Module: `specmock-runtime/src/grpc/tonic_dynamic.rs` (NEW, replaces `protobuf.rs`)

Rebuild gRPC runtime using `tonic`:

```rust
/// Dynamic gRPC service that dispatches based on protobuf descriptors.
pub struct DynamicGrpcService {
    pool: DescriptorPool,
    methods: Arc<HashMap<String, MethodDescriptor>>,
    seed: u64,
}

impl DynamicGrpcService {
    pub fn from_config(config: &ServerConfig) -> Result<Self, RuntimeError>;
}

// Implement tonic service traits:
// - For unary: decode DynamicMessage, validate, generate response, encode
// - For server-streaming: generate N mock messages as a stream
// - For client-streaming: consume stream, validate each message, respond once
// - For bidi-streaming: combine above

/// Spawn gRPC server using tonic's Server builder.
pub async fn spawn_grpc_server(
    service: DynamicGrpcService,
    bind_addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), RuntimeError>;
```

**Key implementation notes:**

- Use `tonic::server::NamedService` or `tonic::codegen::Service` trait to register the dynamic handler.
- The handler receives raw `tonic::Request<Streaming<Bytes>>` and uses `prost-reflect` to decode/encode `DynamicMessage`.
- gRPC status and `google.rpc.Status` details remain in trailers as per the gRPC specification.
- HTTP/2 is handled natively by tonic's hyper-based server.

### 4.8 Module: `specmock-runtime/src/ws/asyncapi_v3.rs` (NEW)

AsyncAPI v3.0 parser alongside the existing v2 parser:

```rust
/// Detect AsyncAPI version and dispatch to appropriate parser.
pub fn parse_asyncapi(root: Value) -> Result<AsyncApiChannels, RuntimeError> {
    let version = root.get("asyncapi").and_then(Value::as_str).unwrap_or("2.0.0");
    if version.starts_with("3.") {
        parse_v3(root)
    } else {
        parse_v2(root) // existing logic
    }
}

/// AsyncAPI v3 uses operations with send/receive actions
/// and channels are separate from operations.
fn parse_v3(root: Value) -> Result<AsyncApiChannels, RuntimeError>;
```

### 4.9 HTTP Runtime Changes (`specmock-runtime/src/http/mod.rs`)

1. **Remove `Box::leak`**: Use `Arc<str>` for ws_path or pass via State.
2. **Body size limit**: `to_bytes(body, config.max_body_size)` with configurable limit (default 10 MiB).
3. **Content-Type validation**: Return 415 when body Content-Type doesn't match any declared request body media type.
4. **Multi-value query params**: Change `HashMap<String, String>` to `HashMap<String, Vec<String>>`.
5. **`Prefer` header extraction**: Parse before dispatching to mock/proxy.
6. **RFC 7807 error format**: Replace `ErrorEnvelope` with `ProblemDetails`.
7. **Proxy `Host` header**: Set `Host` to upstream host when forwarding.

### 4.10 OpenAPI Parser Changes (`specmock-runtime/src/http/openapi.rs`)

1. **Accept `ResolvedDocument`** instead of raw file path. Remove internal `$ref` resolution.
2. **Build `PathRouter`** instead of `Vec<OperationSpec>`.
3. **Parse `style`/`explode`** for parameters to support multi-value query strings.
4. **Parse `discriminator`** from schema nodes and pass to faker.
5. **Parse multiple media types** in request body and responses for content negotiation.
6. **Support named examples** in response for `Prefer: example=xxx`.

### 4.11 Dependency Changes

| Action | Crate | Rationale |
| --- | --- | --- |
| Add | `tonic = "0.13"` | Standard gRPC server |
| Add | `tonic-reflection = "0.13"` | gRPC reflection service (optional) |
| Add | `hyper = "1"` + `hyper-util` | Required by tonic |
| Add | `rand_regex = "0.18"` | Regex-based string generation for faker |
| Add | `serde_yml = "0.0.12"` | Replace deprecated `serde_yaml` |
| Remove | `serde_yaml = "0.9.34"` | Deprecated |
| Keep | `protox`, `prost`, `prost-reflect` | Still needed for descriptor compilation |
| Keep | `axum` (for HTTP/WS) | Only gRPC moves to tonic |
| Keep | `hpx` | Proxy mode + remote `$ref` fetching |

### 4.12 Configuration Changes

Extend `ServerConfig`:

```rust
pub struct ServerConfig {
    // ... existing fields ...
    /// Maximum HTTP request body size in bytes (default 10 MiB).
    pub max_body_size: usize,
    /// Callback spec paths (OpenAPI callbacks).
    pub callback_specs: Vec<PathBuf>,
    /// Enable RFC 7807 error format (default true).
    pub rfc7807_errors: bool,
}
```

Extend CLI `ServeArgs`:

```rust
struct ServeArgs {
    // ... existing fields ...
    /// Maximum request body size (e.g., "10MiB").
    #[arg(long, default_value = "10485760")]
    max_body_size: usize,
}
```

## 5. Verification & Testing Strategy

### 5.1 Unit Tests

| Module | Test Focus |
| --- | --- |
| `ref_resolver` | Local ref, file ref, URL ref, cycle detection, missing ref error |
| `faker` (pattern) | Regex patterns generate valid strings; deterministic with seed |
| `faker` (format) | All format strings pass validation |
| `faker` (discriminator) | Polymorphic schemas generate valid discriminator values |
| `faker` (default) | Default values used when no example exists |
| `router` | Radix tree matches literal, param, and ambiguous paths correctly |
| `negotiate` | `Prefer` header parsing; `Accept` content negotiation |
| `error` (RFC 7807) | Serialization matches RFC 7807 structure |

### 5.2 Integration Tests

| Test Spec | Scenarios |
| --- | --- |
| `openapi-petstore-full.yaml` (multi-file with `$ref`) | Load multi-file spec, validate cross-file refs resolve, mock response matches schema |
| `openapi-polymorphic.yaml` (`discriminator` + `oneOf`) | Mocked response includes correct discriminator value |
| `openapi-complex-params.yaml` (multi-value query, all param styles) | Multi-value query params validated correctly |
| `openapi-prefer.yaml` (multiple response codes + named examples) | `Prefer: code=404` returns 404 response; `Prefer: example=notFound` uses named example |
| `asyncapi-v3-chat.yaml` | v3 spec loads and routes correctly |
| `greeter-streaming.proto` | Server-streaming gRPC returns multiple messages; client-streaming consumes stream |
| `content-negotiation.yaml` (JSON + XML responses) | `Accept: application/json` returns JSON; `Accept: application/xml` returns XML in text |

### 5.3 Validation Rules Table

| Rule | Input | Expected |
| --- | --- | --- |
| Missing required path param | `GET /pets/abc` (integer expected) | 400 + RFC 7807 with `instance_pointer=/id` |
| Missing required body | `POST /pets` without body | 400 + RFC 7807 with `keyword=required` |
| Wrong Content-Type | `POST /pets` with `text/plain` body | 415 Unsupported Media Type |
| Body too large | 11 MiB POST body | 413 Payload Too Large |
| Prefer code selection | `Prefer: code=404` on operation with 404 response | 404 + mocked 404 body |
| Proxy upstream schema violation | Upstream returns `{id: "string"}` for integer schema | 502 + RFC 7807 with validation errors |
| gRPC invalid protobuf | Garbage bytes as gRPC body | `INVALID_ARGUMENT` (3) with `google.rpc.BadRequest` details |
| gRPC method not found | `/unknown.Service/Method` | `UNIMPLEMENTED` (12) |
| WS invalid channel | `{"channel": "nonexistent", "payload": {}}` | Error event with routing error |

## 6. Implementation Plan

Implementation is broken into 5 phases, executed sequentially. See `tasks.md` for the detailed task breakdown.

| Phase | Focus | Key Deliverable |
| --- | --- | --- |
| 1 — Foundation | `$ref` resolver, dependency changes, deprecation fixes | All specs load with full ref resolution |
| 2 — Core Enhancements | Faker extensions, RFC 7807, schema utilities | Core layer handles all schema complexity |
| 3 — HTTP Runtime | Router, content negotiation, Prefer, multi-value params, body limits | Full Prism-parity HTTP mocking |
| 4 — Protocol Runtimes | Tonic gRPC rebuild, AsyncAPI v3, WS multi-path | All three protocols production-ready |
| 5 — Integration & Polish | Comprehensive test specs, cleanup, documentation | Verified end-to-end Prism replacement |
