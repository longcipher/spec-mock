# spec-mock

Pure Rust spec-driven mock runtime for:

- HTTP REST from OpenAPI 3.0.x and 3.1.x
- WebSocket from AsyncAPI 2.x and 3.x
- gRPC (unary + server-streaming) from Protobuf over HTTP/2

It supports standalone CLI usage and native Rust SDK embedding for `#[tokio::test]`.

## Feature Comparison vs Prism

| Feature                             | spec-mock | Prism |
| ----------------------------------- | :-------: | :---: |
| OpenAPI 3.0 / 3.1                   |     ✅     |   ✅   |
| Multi-file `$ref` resolution         |     ✅     |   ✅   |
| `Prefer: code=xxx` header            |     ✅     |   ✅   |
| `Prefer: example=xxx` header         |     ✅     |   ✅   |
| `Prefer: dynamic=true` header        |     ✅     |   ✅   |
| Content negotiation (`Accept`)       |     ✅     |   ✅   |
| RFC 7807 Problem Details errors      |     ✅     |   ✅   |
| Request body validation              |     ✅     |   ✅   |
| Query/path/header param validation   |     ✅     |   ✅   |
| Multi-value query params             |     ✅     |   ✅   |
| Proxy mode with validation           |     ✅     |   ✅   |
| Callbacks / Webhooks                 |     ✅     |   ❌   |
| AsyncAPI WebSocket mocking           |     ✅     |   ❌   |
| AsyncAPI v3 support                  |     ✅     |   ❌   |
| gRPC Protobuf mocking (HTTP/2)       |     ✅     |   ❌   |
| gRPC server-streaming                |     ✅     |   ❌   |
| Rust SDK (embed in tests)            |     ✅     |   ❌   |
| Configurable body size limit         |     ✅     |   ❌   |
| Content-Type validation (415)        |     ✅     |   ❌   |

## Capability Summary

- Request validation against spec schema.
- RFC 7807 `application/problem+json` error responses with structured validation details.
- Response generation priority: `example` → `examples[0]` → `default` → deterministic schema faker.
- Faker supports: `pattern` (regex), all standard `format` values, `discriminator` + `mapping`, `additionalProperties`, `default` values.
- Proxy mode for HTTP: forwards upstream response and validates it against OpenAPI response schema.
- `Prefer` header: select response by status code, named example, or force dynamic generation.
- Content negotiation via `Accept` header.
- Multi-value query parameters with `style`/`explode` support.
- Callback/webhook firing on matched operations (fire-and-forget).
- Configurable request body size limit (default 10 MiB, 413 on exceeded).
- Content-Type validation returns 415 for unsupported media types.
- gRPC error metadata includes `grpc-status-details-bin` plus `grpc-message` and `grpc-status`.
- AsyncAPI v2 and v3 with multi-path WebSocket routing.

## Quick Start (CLI)

The CLI subcommand is `spec-mock serve` (via `cargo run -p spec-mock -- serve` during development).

### 1. OpenAPI HTTP mock server

```bash
cargo run -p spec-mock -- serve \
  --openapi docs/specs/pets.openapi.yaml \
  --http-addr 127.0.0.1:4010

curl http://127.0.0.1:4010/pets/1
```

Invalid request example:

```bash
curl -i http://127.0.0.1:4010/pets/abc
```

Error responses use [RFC 7807](https://www.rfc-editor.org/rfc/rfc7807) format (`application/problem+json`):

```json
{
  "type": "about:blank",
  "title": "Bad Request",
  "status": 400,
  "detail": "Request validation failed",
  "errors": [
    {
      "instance_pointer": "/id",
      "schema_pointer": "/minimum",
      "keyword": "minimum",
      "message": "..."
    }
  ]
}
```

#### `Prefer` Header Examples

Select a specific HTTP status code:

```bash
curl -H "Prefer: code=404" http://127.0.0.1:4010/pets/1
```

Select a named response example:

```bash
curl -H "Prefer: example=whiskers" http://127.0.0.1:4010/pets/1
```

Force dynamic (faker-generated) response:

```bash
curl -H "Prefer: dynamic=true" http://127.0.0.1:4010/pets/1
```

Combine preferences:

```bash
curl -H "Prefer: code=200, example=fluffy" http://127.0.0.1:4010/pets/1
```

### 2. AsyncAPI WebSocket mock server

Supports both AsyncAPI v2.x and v3.x specs.

```bash
cargo run -p spec-mock -- serve \
  --asyncapi docs/specs/chat.asyncapi.yaml \
  --http-addr 127.0.0.1:4011
```

WebSocket endpoint: `ws://127.0.0.1:4011/ws`

Input envelope options:

- Explicit channel envelope: `{"channel":"chat.send","payload":{...}}`
- Alias envelope: `{"topic":"chat.send","data":{...}}`
- Auto-routing: send raw payload and runtime matches by publish schema.

### 3. Protobuf gRPC mock server

Uses tonic for standard HTTP/2 transport with proper trailers. Supports unary and server-streaming RPCs.

```bash
cargo run -p spec-mock -- serve \
  --proto docs/specs/greeter.proto \
  --grpc-addr 127.0.0.1:5010 \
  --http-addr 127.0.0.1:4012

grpcurl -plaintext \
  -import-path docs/specs \
  -proto greeter.proto \
  -d '{"name":"alice"}' \
  127.0.0.1:5010 mock.Greeter/SayHello
```

## Mock and Proxy Modes

`spec-mock` supports `--mode mock` (default) and `--mode proxy`.

Proxy example:

```bash
cargo run -p spec-mock -- serve \
  --openapi docs/specs/pets.openapi.yaml \
  --mode proxy \
  --upstream http://127.0.0.1:8080 \
  --http-addr 127.0.0.1:4010
```

In proxy mode, if upstream JSON response violates OpenAPI schema, runtime returns `502` with aggregated schema errors.

## CLI Options

```text
spec-mock serve [OPTIONS]

Options:
  --openapi <PATH>        OpenAPI spec file path
  --asyncapi <PATH>       AsyncAPI spec file path
  --proto <PATH>          Protobuf root .proto file path
  --mode <MODE>           Runtime mode [default: mock] [possible values: mock, proxy]
  --upstream <URL>        Proxy upstream base URL
  --seed <SEED>           Deterministic data seed [default: 42]
  --http-addr <ADDR>      HTTP bind address [default: 127.0.0.1:4010]
  --grpc-addr <ADDR>      gRPC bind address [default: 127.0.0.1:5010]
  --max-body-size <BYTES> Maximum request body size in bytes [default: 10485760]
```

## Rust SDK

### Embedded in `#[tokio::test]`

```rust
use specmock_sdk::MockServer;

#[tokio::test]
async fn mock_server_for_test() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::builder()
        .openapi("docs/specs/pets.openapi.yaml")
        .seed(42)
        .start()
        .await?;

    let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
    assert_eq!(response.status().as_u16(), 200);

    server.shutdown().await;
    Ok(())
}
```

### Start as an external process

```rust
use std::path::Path;

use specmock_sdk::MockServer;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let mut server = MockServer::builder()
    .openapi("docs/specs/pets.openapi.yaml")
    .start_process_with_bin(Path::new("target/debug/spec-mock"))
    .await?;

let response = hpx::get(format!("{}/pets/1", server.http_base_url())).send().await?;
assert_eq!(response.status().as_u16(), 200);

server.shutdown()?;
# Ok(())
# }
```

## Workspace Commands

```bash
just format
just lint
just test
```

## Example Specs

- OpenAPI: `docs/specs/pets.openapi.yaml`
- AsyncAPI: `docs/specs/chat.asyncapi.yaml`
- Protobuf: `docs/specs/greeter.proto`

## License

Apache-2.0
