# Spec-Mock Rust Workspace Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a pure-Rust spec-driven mock server that supports OpenAPI (3.0.x/3.1.x) HTTP REST, AsyncAPI WebSocket, and Protobuf gRPC, with strict request/response validation and embeddable Rust SDK for `#[tokio::test]`.

**Architecture:** Use a layered design: `specmock-core` for contract model, schema validation, and data generation; `specmock-runtime` for protocol adapters (HTTP/WS/gRPC); `specmock-sdk` for in-process and process-mode lifecycle management; `spec-mock` CLI for standalone runtime. All protocols share one validation and error aggregation model.

**Tech Stack:** Rust workspace, `tokio`, `axum`, `hyper`, `tonic`, `serde`, `serde_json`, `serde_yaml`, `jsonschema`, `protox`, `prost-reflect`, `scc`, `hpx`, `tracing`, `clap`, `thiserror`, `eyre`.

---

## Task 1: Workspace Scaffolding and Dependency Baseline

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/bin/spec-mock/Cargo.toml`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/bin/spec-mock/src/main.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/Cargo.toml`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/lib.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/Cargo.toml`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/lib.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/Cargo.toml`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/src/lib.rs`
- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/Cargo.toml`

**Step 1: Write the failing test**

```rust
#[test]
fn workspace_contains_specmock_crates() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    assert!(root.join("crates/specmock-core").exists());
    assert!(root.join("crates/specmock-runtime").exists());
    assert!(root.join("crates/specmock-sdk").exists());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p common workspace_contains_specmock_crates -q`
Expected: FAIL because new crates are not created yet.

**Step 3: Write minimal implementation**

- Create new crates and binary.
- Add baseline dependencies via `cargo add` following workspace rules.
- Keep code compiling with minimal public exports.

**Step 4: Run test to verify it passes**

Run: `cargo test --workspace --all-targets`
Expected: PASS for scaffolding tests and compilation.

**Step 5: Commit**

```bash
git add Cargo.toml bin/spec-mock crates/specmock-core crates/specmock-runtime crates/specmock-sdk
git commit -m "feat: scaffold spec-mock workspace crates"
```

### Task 2: Core Contract Model, Error Aggregation, and Schema Faker

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/contract.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/error.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/faker.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/validate.rs`
- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/lib.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-core/src/lib.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn faker_generates_value_that_validates() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["id", "name"],
        "properties": {
            "id": {"type": "integer", "minimum": 1},
            "name": {"type": "string", "minLength": 1}
        }
    });
    let value = specmock_core::faker::generate_json_value(&schema, 7).unwrap();
    let errors = specmock_core::validate::validate_instance(&schema, &value).unwrap();
    assert!(errors.is_empty(), "expected no validation errors: {errors:?}");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p specmock-core faker_generates_value_that_validates -q`
Expected: FAIL because faker/validate modules do not exist.

**Step 3: Write minimal implementation**

- Add `ValidationIssue` model with `instance_pointer`, `schema_pointer`, `keyword`, `message`.
- Implement `validate_instance` using `jsonschema::validator_for` and `iter_errors`.
- Implement deterministic faker with seed fallback order: `example` -> `examples` -> schema walker.

**Step 4: Run test to verify it passes**

Run: `cargo test -p specmock-core`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/specmock-core
git commit -m "feat(core): add schema validation and deterministic faker"
```

### Task 3: OpenAPI HTTP Runtime (Mock + Proxy + Validation)

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/http/mod.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/http/openapi.rs`
- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/lib.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/tests/http_openapi.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn invalid_request_returns_400_with_pointer_details() {
    // start runtime with OpenAPI spec requiring integer id
    // send id="abc"
    // expect 400 and JSON body contains instance_pointer/schema_pointer
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p specmock-runtime --test http_openapi invalid_request_returns_400_with_pointer_details -q`
Expected: FAIL because HTTP runtime does not exist.

**Step 3: Write minimal implementation**

- Parse OpenAPI YAML/JSON as `serde_json::Value`.
- Build route index for methods + path templates.
- Validate request (path/query/header/body) against JSON schemas.
- Response generation uses core faker.
- Add proxy mode using `hpx` and response validation before return.

**Step 4: Run test to verify it passes**

Run: `cargo test -p specmock-runtime --test http_openapi`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/specmock-runtime
git commit -m "feat(http): add openapi runtime validation and mock/proxy modes"
```

### Task 4: AsyncAPI WebSocket Runtime

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/ws/mod.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/ws/asyncapi.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/tests/ws_asyncapi.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn ws_invalid_message_returns_structured_error_event() {
    // connect websocket
    // send malformed payload that violates asyncapi message payload schema
    // expect error event with pointer details
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p specmock-runtime --test ws_asyncapi ws_invalid_message_returns_structured_error_event -q`
Expected: FAIL.

**Step 3: Write minimal implementation**

- Parse AsyncAPI as JSON value.
- Build channel action map (`publish`/`subscribe`) with payload schemas.
- Validate incoming WS payload and emit error envelope on failure.
- Emit mock response payload from `example/examples` or faker.

**Step 4: Run test to verify it passes**

Run: `cargo test -p specmock-runtime --test ws_asyncapi`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/specmock-runtime
git commit -m "feat(ws): add asyncapi websocket mock runtime"
```

### Task 5: Protobuf gRPC Dynamic Runtime

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/grpc/mod.rs`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/src/grpc/protobuf.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-runtime/tests/grpc_protobuf.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn grpc_invalid_request_returns_invalid_argument_with_details() {
    // start grpc runtime from proto
    // send invalid wire message or missing required oneof branch
    // expect INVALID_ARGUMENT and validation detail pointers
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p specmock-runtime --test grpc_protobuf grpc_invalid_request_returns_invalid_argument_with_details -q`
Expected: FAIL.

**Step 3: Write minimal implementation**

- Compile `.proto` descriptors via `protox`.
- Build dynamic method registry via `prost-reflect`.
- Implement unary call handling with decode/validate/generate/encode.
- Return gRPC status with structured detail payload on validation failure.

**Step 4: Run test to verify it passes**

Run: `cargo test -p specmock-runtime --test grpc_protobuf`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/specmock-runtime
git commit -m "feat(grpc): add protobuf dynamic grpc mock runtime"
```

### Task 6: SDK and CLI Integration

**Files:**

- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/src/server.rs`
- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/src/lib.rs`
- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/bin/spec-mock/src/main.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/tests/sdk_embed.rs`
- Test: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/tests/sdk_process.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn sdk_embedded_server_can_be_used_in_tokio_test() {
    let server = specmock_sdk::MockServer::builder()
        .openapi("tests/specs/petstore.yaml")
        .seed(42)
        .start()
        .await
        .unwrap();
    let status = reqwest_like_client_get(server.http_base_url()).await;
    assert_eq!(status, 200);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p specmock-sdk --test sdk_embed sdk_embedded_server_can_be_used_in_tokio_test -q`
Expected: FAIL.

**Step 3: Write minimal implementation**

- Implement `MockServer::builder()` with in-process mode for `#[tokio::test]`.
- Implement `spawn_process()` mode by launching `spec-mock serve`.
- Add CLI arguments for spec paths, seed, ports, and `mock|proxy` mode.

**Step 4: Run test to verify it passes**

Run: `cargo test -p specmock-sdk`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/specmock-sdk bin/spec-mock
git commit -m "feat: add sdk embedded/process runtime and cli serve command"
```

### Task 7: Workspace Verification and Documentation

**Files:**

- Modify: `/Volumes/akext/src/github.com/longcipher/spec-mock/README.md`
- Create: `/Volumes/akext/src/github.com/longcipher/spec-mock/crates/specmock-sdk/examples/tokio_test_like.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn readme_contains_sdk_example_and_cli_usage() {
    let readme = std::fs::read_to_string("README.md").unwrap();
    assert!(readme.contains("spec-mock serve"));
    assert!(readme.contains("#[tokio::test]"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p common readme_contains_sdk_example_and_cli_usage -q`
Expected: FAIL.

**Step 3: Write minimal implementation**

- Update README with protocol support matrix and quickstart.
- Document error response shape and JSON Pointer fields.
- Add SDK embed example and process mode example.

**Step 4: Run test to verify it passes**

Run: `just format && just lint && just test`
Expected: all PASS.

**Step 5: Commit**

```bash
git add README.md crates/specmock-sdk/examples
git commit -m "docs: add usage and validation error model"
```
