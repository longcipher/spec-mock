# Design: Prism Comparison Fuzz Testing

| Metadata | Details |
| :--- | :--- |
| **Status** | Draft |
| **Created** | 2026-03-02 |
| **Scope** | Full |
| **Depends On** | specs/2026-03-02-01-prism-parity-overhaul (complete) |

## 1. Executive Summary

spec-mock claims Prism feature parity for OpenAPI HTTP mocking but lacks automated comparison tests to prove it. This design adds a dedicated integration test harness that starts both spec-mock and Prism side-by-side, generates random HTTP requests via fuzz testing against OpenAPI specifications, sends identical requests to both servers, and compares the responses structurally. The harness covers success paths (2xx), error paths (4xx/5xx), `Prefer` header behavior, content negotiation, request body validation, and schema conformance. A new `just integration-test` command runs the full comparison suite.

## 2. Requirements & Goals

### 2.1 Functional Requirements

| ID | Requirement | Priority |
| --- | --- | --- |
| F01 | Start spec-mock and Prism from the same OpenAPI spec, both on random ports | Critical |
| F02 | Generate random valid HTTP requests (method, path, query params, headers, body) from OpenAPI spec | Critical |
| F03 | Generate random invalid HTTP requests (wrong types, missing required fields, invalid paths) | Critical |
| F04 | Send identical requests to both servers and collect responses | Critical |
| F05 | Compare response status codes between spec-mock and Prism | Critical |
| F06 | Compare response body structure (JSON keys, types, array lengths) — not exact values | Critical |
| F07 | Compare response Content-Type headers | High |
| F08 | Compare RFC 7807 error envelope structure for 4xx/5xx responses | High |
| F09 | Test `Prefer: code=xxx` header produces matching status codes on both servers | High |
| F10 | Test `Prefer: example=xxx` header produces matching example bodies on both servers | High |
| F11 | Test `Prefer: dynamic=true` produces schema-conforming responses on both servers | High |
| F12 | Test content negotiation via `Accept` header produces matching Content-Type on both servers | Medium |
| F13 | Test request body validation (valid → 2xx, invalid → 422/400) on both servers | High |
| F14 | Test multi-value query parameters produce matching behavior | Medium |
| F15 | Provide a rich OpenAPI spec covering all testable features (polymorphism, callbacks, examples, multiple content types, array parameters) | High |
| F16 | Add `just integration-test` command to run the comparison suite | Critical |
| F17 | Fuzz testing generates reproducible requests via deterministic seed | High |
| F18 | Report clear diffs when behavior diverges between spec-mock and Prism | High |

### 2.2 Non-Functional Requirements

| ID | Requirement |
| --- | --- |
| NF01 | Tests follow AGENTS.md rules: `thiserror` in library code, `eyre` not used in test code, `tracing` for any logging, `hpx` for HTTP client |
| NF02 | Prism is assumed to be installed globally via npm (`npx @stoplight/prism-cli`); test skips gracefully if not found |
| NF03 | All fuzz iterations complete within 60 seconds for the default seed and iteration count |
| NF04 | False positive rate < 5% — comparison logic must tolerate expected differences (e.g., faker values, timestamps) |
| NF05 | Test output clearly distinguishes structural mismatches from value differences |

### 2.3 Assumptions

- A01: Prism is installed via `npx @stoplight/prism-cli mock` or a global `prism` binary. Tests will attempt `npx @stoplight/prism-cli` first, then `prism` on PATH.
- A02: Prism and spec-mock produce identical status codes for identical requests when using `Prefer: code=xxx` and `Prefer: example=xxx`.
- A03: For `Prefer: dynamic=true` and default mock mode, response bodies will differ in values but should match in structure (same JSON keys, same JSON types, compatible array lengths).
- A04: Prism may return slightly different RFC 7807 error detail strings. Comparison focuses on `status`, `title`, and error array length — not on exact `detail` text.
- A05: Prism does not support OpenAPI callbacks/webhooks or features unique to spec-mock (AsyncAPI, gRPC). These are explicitly excluded from comparison tests.

### 2.4 Out of Scope

- AsyncAPI/WebSocket comparison testing (Prism doesn't support it)
- gRPC/Protobuf comparison testing (Prism doesn't support it)
- Performance benchmarking (this is correctness testing only)
- Exact value matching for dynamically generated faker data
- Testing Prism's proxy mode
- Load/stress testing

## 3. Architecture Overview

### 3.1 System Context

```text
┌────────────────────────────┐
│  Integration Test Harness  │
│  (Rust, cargo test)        │
│                            │
│  ┌──────────────────────┐  │
│  │ OpenAPI Spec Fuzzer  │  │
│  │ (request generator)  │  │
│  └──────────┬───────────┘  │
│             │               │
│    ┌────────┴────────┐     │
│    ▼                 ▼     │
│ ┌──────────┐  ┌──────────┐│
│ │spec-mock │  │  Prism   ││
│ │ (in-proc)│  │ (process)││
│ │:random   │  │ :random  ││
│ └────┬─────┘  └────┬─────┘│
│      │              │      │
│      ▼              ▼      │
│  ┌─────────────────────┐   │
│  │ Response Comparator │   │
│  │ (structural diff)   │   │
│  └─────────────────────┘   │
└────────────────────────────┘
```

### 3.2 Key Design Principles

1. **spec-mock runs in-process; Prism runs as a child process.** This ensures spec-mock uses the same code path as production while Prism is exercised as a black-box external tool.

2. **Request generation is spec-driven.** The fuzzer reads the OpenAPI spec and generates requests that exercise all defined operations, parameter combinations, and edge cases. It does not generate arbitrary HTTP traffic.

3. **Comparison is structural, not exact.** Status codes must match exactly. Response bodies are compared by JSON key presence, value types, and array length ranges — not by exact values (since both servers use different faker engines).

4. **Deterministic reproducibility.** The fuzzer uses a configurable seed (default 42) so that failing test cases can be reproduced exactly.

5. **Graceful degradation.** If Prism is not installed, comparison tests are skipped with a clear message rather than failing the build.

### 3.3 Existing Components to Reuse

| Component | Location | Reuse Strategy |
| --- | --- | --- |
| `ServerConfig` + `start()` | `specmock-runtime/src/lib.rs` | Start spec-mock in-process with random port |
| `MockServer::builder()` | `specmock-sdk/src/server.rs` | Alternative in-process startup via SDK |
| `hpx::Client` | External dependency | HTTP client for sending requests to both servers |
| `openapi-negotiate.yaml` | `specmock-runtime/tests/specs/` | Reference spec for Prefer header tests |
| `openapi-content-types.yaml` | `specmock-runtime/tests/specs/` | Reference spec for content negotiation tests |
| `openapi-array-params.yaml` | `specmock-runtime/tests/specs/` | Reference spec for multi-value query param tests |
| `openapi-polymorphic.yaml` | `specmock-runtime/tests/specs/` | Reference spec for polymorphic schema tests |
| `openapi-body-limit.yaml` | `specmock-runtime/tests/specs/` | Reference spec for request body validation tests |
| Feature comparison table | `README.md` lines 11-30 | Reference for which features to test |

## 4. Detailed Design

### 4.1 New Crate: Test Harness Location

The comparison tests live as integration tests in **`crates/specmock-runtime/tests/`** alongside existing integration tests. A new file `prism_comparison.rs` contains the comparison harness. Shared utilities (fuzzer, comparator, Prism process management) are placed in a `tests/harness/` module directory.

```text
crates/specmock-runtime/tests/
├── harness/
│   ├── mod.rs           # re-exports
│   ├── prism.rs         # Prism process lifecycle
│   ├── fuzzer.rs        # OpenAPI-driven request generator
│   ├── comparator.rs    # Response structural comparison
│   └── request.rs       # Request/response types
├── prism_comparison.rs  # #[test] entry point
├── specs/
│   ├── openapi-prism-comparison.yaml  # Rich comparison spec
│   └── ... (existing specs)
└── ... (existing test files)
```

### 4.2 Module: `harness/prism.rs` — Prism Process Manager

Manages Prism as a child process.

```rust
/// Handle to a running Prism mock server process.
pub struct PrismServer {
    child: std::process::Child,
    port: u16,
}

impl PrismServer {
    /// Start Prism mocking the given OpenAPI spec on a random port.
    /// Returns None if Prism is not installed.
    pub async fn start(spec_path: &Path) -> Option<Self> {
        // Try npx @stoplight/prism-cli first, then bare `prism`
        // Use --port 0 is not supported; pick a random high port
        // Poll /pets or similar until Prism is ready (max 10s)
    }

    /// Base URL of the running Prism server.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for PrismServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
```

**Port selection:** Prism doesn't support `:0` port binding. The harness picks a random port from the ephemeral range (49152–65535), checks availability with a `TcpListener::bind` probe, then passes it to Prism via `--port`.

**Readiness check:** After spawning Prism, poll `GET /` every 200ms up to 10 seconds. Prism returns a response once loaded.

### 4.3 Module: `harness/fuzzer.rs` — OpenAPI Request Fuzzer

Generates random HTTP requests from an OpenAPI spec.

```rust
/// A generated test request.
pub struct FuzzRequest {
    pub method: http::Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
    /// Description for test output.
    pub description: String,
}

/// Request generator categories.
pub enum RequestCategory {
    /// Valid request matching schema constraints.
    Valid,
    /// Invalid path parameter type (e.g., string for integer).
    InvalidPathParam,
    /// Missing required query parameter.
    MissingRequiredQuery,
    /// Invalid request body (wrong types, missing required fields).
    InvalidBody,
    /// Wrong Content-Type header.
    WrongContentType,
    /// Oversized request body.
    OversizedBody,
    /// Unknown path (404).
    UnknownPath,
    /// With Prefer header (code/example/dynamic).
    WithPreferHeader { prefer: String },
    /// With Accept header for content negotiation.
    WithAcceptHeader { accept: String },
}

/// Generate fuzz requests from an OpenAPI spec.
pub struct OpenApiFuzzer {
    spec: serde_json::Value,
    rng: rand_chacha::ChaCha8Rng,
    iterations: usize,
}

impl OpenApiFuzzer {
    pub fn new(spec_path: &Path, seed: u64, iterations: usize) -> Self;

    /// Generate all fuzz requests.
    pub fn generate(&mut self) -> Vec<(RequestCategory, FuzzRequest)>;

    /// Generate valid requests for all operations.
    fn generate_valid_requests(&mut self) -> Vec<FuzzRequest>;

    /// Generate invalid requests for error path testing.
    fn generate_invalid_requests(&mut self) -> Vec<FuzzRequest>;

    /// Generate Prefer header variants.
    fn generate_prefer_requests(&mut self) -> Vec<FuzzRequest>;

    /// Generate Accept header variants.
    fn generate_accept_requests(&mut self) -> Vec<FuzzRequest>;
}
```

**Fuzzing strategy:**

1. **Parse the OpenAPI spec** to extract all paths, operations, parameters, request bodies, and response codes.
2. **For each operation, generate:**
   - N valid requests with randomized parameter values within schema constraints.
   - N invalid requests with type-mismatched parameters.
   - 1 request per defined `Prefer: code=xxx` (for each response status code).
   - 1 request per defined named example (`Prefer: example=xxx`).
   - 1 request with `Prefer: dynamic=true`.
   - 1 request per alternative `Accept` content type.
3. **For error paths, generate:**
   - Request to undefined path → expect 404 from both.
   - Request with missing required parameters → expect 400 from both.
   - Request with invalid body → expect 422 or 400 from both.
   - Request with wrong Content-Type → expect 415 from both.

### 4.4 Module: `harness/comparator.rs` — Response Comparator

Compares two HTTP responses structurally.

```rust
/// Result of comparing two responses.
pub struct ComparisonResult {
    pub passed: bool,
    pub status_match: bool,
    pub content_type_match: bool,
    pub body_structure_match: bool,
    pub diffs: Vec<Diff>,
}

pub enum Diff {
    StatusCode { specmock: u16, prism: u16 },
    ContentType { specmock: String, prism: String },
    MissingKey { path: String, present_in: Server },
    TypeMismatch { path: String, specmock_type: String, prism_type: String },
    ArrayLengthMismatch { path: String, specmock_len: usize, prism_len: usize },
}

pub enum Server {
    SpecMock,
    Prism,
}

/// Compare two responses.
pub fn compare_responses(
    specmock_status: u16,
    specmock_headers: &[(String, String)],
    specmock_body: &[u8],
    prism_status: u16,
    prism_headers: &[(String, String)],
    prism_body: &[u8],
    category: &RequestCategory,
) -> ComparisonResult;
```

**Comparison rules:**

| Aspect | Comparison Mode |
| --- | --- |
| Status code | Exact match |
| Content-Type | Exact match (ignoring charset parameter) |
| JSON key presence | Exact match (recursive) |
| JSON value types | Exact match (number/string/bool/null/array/object) |
| JSON string values | Ignored (different faker engines) |
| JSON number values | Ignored for dynamic mode; exact for examples |
| JSON array lengths | Range match (both > 0 or both = 0) |
| RFC 7807 `type` field | Exact match |
| RFC 7807 `title` field | Exact match |
| RFC 7807 `status` field | Exact match |
| RFC 7807 `detail` text | Ignored (implementation-specific) |
| RFC 7807 `errors` array | Length match (both have errors or both don't) |

### 4.5 OpenAPI Comparison Spec

A comprehensive OpenAPI spec designed to exercise all features Prism supports. Placed at `tests/specs/openapi-prism-comparison.yaml`.

The spec includes:

- `GET /pets` — list endpoint with query params (`limit`, `offset`, `tags[]`)
- `GET /pets/{id}` — single resource with integer path param, multiple response codes (200, 404, 500), named examples
- `POST /pets` — create with required request body, 201 response
- `PUT /pets/{id}` — update with request body
- `DELETE /pets/{id}` — delete, 204 no content
- Multiple response Content-Types (application/json, text/plain)
- `Prefer` header examples defined on key endpoints
- Polymorphic schema with `oneOf` + `discriminator`
- Array-typed query parameters
- Required and optional parameters
- `format` constraints (date-time, email, uri, uuid)
- `pattern` regex constraints
- `minLength`/`maxLength`/`minimum`/`maximum` constraints
- Nested object schemas with `$ref`

### 4.6 Test Organization

```rust
// prism_comparison.rs

#[cfg(test)]
mod harness;

/// Skip all tests if Prism is not installed.
/// Use a shared static to start servers once.
static SERVERS: OnceLock<Option<(specmock_runtime::RunningServer, PrismServer)>> = ...;

#[tokio::test]
async fn fuzz_valid_requests_match() { ... }

#[tokio::test]
async fn fuzz_invalid_path_returns_matching_status() { ... }

#[tokio::test]
async fn fuzz_invalid_body_returns_matching_status() { ... }

#[tokio::test]
async fn prefer_code_matches() { ... }

#[tokio::test]
async fn prefer_example_matches() { ... }

#[tokio::test]
async fn prefer_dynamic_structure_matches() { ... }

#[tokio::test]
async fn content_negotiation_matches() { ... }

#[tokio::test]
async fn missing_required_param_returns_matching_status() { ... }

#[tokio::test]
async fn wrong_content_type_returns_415_on_both() { ... }

#[tokio::test]
async fn unknown_path_returns_404_or_similar_on_both() { ... }
```

### 4.7 Justfile Integration

```just
# Run Prism comparison integration tests
integration-test:
  cargo test --test prism_comparison --features integration-test -- --nocapture
```

The `integration-test` feature gate prevents these tests from running in normal `cargo test` (since they require Prism as an external dependency). The feature is defined in `specmock-runtime/Cargo.toml`.

### 4.8 Error Handling

- **Prism not installed:** Tests print a skip message and return `Ok(())`. No test failure.
- **Prism fails to start:** Test fails with a clear error including Prism's stderr output.
- **Port conflict:** Retry with a different random port up to 3 times.
- **Comparison mismatch:** Test collects all diffs and reports them together (doesn't fail on first diff).
- **Timeout:** Each individual request pair times out at 5 seconds. Overall test suite timeout is 120 seconds.

### 4.9 Configuration

| Setting | Default | Source |
| --- | --- | --- |
| Fuzz seed | 42 | `SPECMOCK_FUZZ_SEED` env var |
| Fuzz iterations per operation | 10 | `SPECMOCK_FUZZ_ITERATIONS` env var |
| Prism command | `npx @stoplight/prism-cli` | `SPECMOCK_PRISM_CMD` env var |
| Request timeout | 5s | Hardcoded |
| Server startup timeout | 10s | Hardcoded |

## 5. Verification & Testing Strategy

### 5.1 The Tests Are the Feature

This spec is about creating tests, so verification is inherently embedded:

| Test Category | What It Verifies | Expected Outcome |
| --- | --- | --- |
| Valid request fuzz | spec-mock matches Prism for success responses | Status codes match, body structures match |
| Invalid path param | Both return 400-class error | Status codes match |
| Missing required param | Both return 400 | Status codes match |
| Invalid request body | Both return 400/422 | Status codes match |
| Wrong Content-Type | Both return 415 | Status codes match |
| Unknown path | Both return 404 | Status codes match |
| `Prefer: code=xxx` | Both return requested status code | Exact status match |
| `Prefer: example=xxx` | Both return the same example body | Exact value match |
| `Prefer: dynamic=true` | Both return schema-conforming different body | Structure match |
| Content negotiation | Both respond with requested Content-Type | Content-Type match |

### 5.2 CI Integration

The `just integration-test` command is **not** part of the default `just ci` recipe because it requires Prism (external npm dependency). CI pipelines that want comparison testing must:

1. Install Node.js and `npm install -g @stoplight/prism-cli`
2. Run `just integration-test`

## 6. Implementation Plan

| Phase | Description | Tasks |
| --- | --- | --- |
| 1 — Harness Foundation | Prism process manager, request/response types, Justfile command | Tasks 1.1–1.3 |
| 2 — Fuzzer | OpenAPI spec parser, request generator for all categories | Tasks 2.1–2.4 |
| 3 — Comparator | Structural response comparison | Task 3.1 |
| 4 — Comparison Spec | Rich OpenAPI spec + integration tests | Tasks 4.1–4.3 |
| 5 — Polish | Error reporting, CI docs, env var config | Tasks 5.1–5.2 |
