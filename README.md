# spec-mock

[![DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/longcipher/spec-mock)
[![Context7](https://img.shields.io/badge/Website-context7.com-blue)](https://context7.com/longcipher/spec-mock)
[![crates.io](https://img.shields.io/crates/v/spec-mock.svg)](https://crates.io/crates/spec-mock)
[![docs.rs](https://docs.rs/spec-mock/badge.svg)](https://docs.rs/spec-mock)

![spec-mock](https://socialify.git.ci/longcipher/spec-mock/image?font=Source+Code+Pro&language=1&name=1&owner=1&pattern=Circuit+Board&theme=Auto)

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
| Non-JSON response media types        |     ✅     |   ✅   |
| Declared response headers            |     ✅     |   ✅   |
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
| Content negotiation (406)            |     ✅     |   ❌   |
| Configurable WebSocket path          |     ✅     |   ❌   |
| Stateful mocking (scenarios, store)  |     ✅     |   ❌   |
| Stateful WebSocket channels          |     ✅     |   ❌   |
| Stateful gRPC methods                |     ✅     |   ❌   |
| Create-then-read-back resource CRUD  |     ✅     |   ❌   |
| Scenario failure injection           |     ✅     |   ❌   |

## Capability Summary

- Request validation against spec schema.
- RFC 7807 `application/problem+json` error responses with structured validation details.
- Response generation priority: `example` → `examples[0]` → `default` → deterministic schema faker.
- Content negotiation via the `Accept` header across every declared media type; an
  unsatisfiable `Accept` returns `406`. Without `Accept`, the JSON family is preferred.
- Response body encoding per media type: compact JSON, XML (with schema-derived root
  element), and — for `text/*` and binary types alike — a string value emitted verbatim,
  `null` as an empty body, and any other value as its JSON form.
- Declared response headers are emitted, using `example` when present and the faker otherwise.
- Faker supports: `pattern` (regex), all standard `format` values, seeded `enum` and
  `oneOf`/`anyOf` variant selection, `discriminator` + `mapping`, `additionalProperties`,
  `default` values.
- Proxy mode for HTTP: forwards upstream response and validates it against OpenAPI response schema.
- `Prefer` header: select response by status code, named example, or force dynamic generation.
- Multi-value query parameters: repeated keys are collected and validated against the `items`
  subschema. OpenAPI serialization (`style`/`explode`) is **not** implemented.
- Callback/webhook firing on matched operations (fire-and-forget).
- Configurable request body size limit (default 10 MiB, 413 on exceeded).
- Content-Type validation returns 415 for media types the operation does not declare.
- gRPC error metadata includes `grpc-status-details-bin` plus `grpc-message` and `grpc-status`.
- AsyncAPI v2 and v3 with multi-path WebSocket routing on a configurable base path.
- Stateful mocking via a scenario sidecar: named flows, resource CRUD backed by a
  store, pre-seeding, per-session isolation, and error injection as a state
  transition. See [Stateful Mocking](#stateful-mocking).

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

A preference that cannot be satisfied is reported instead of being silently ignored:
`Prefer: code=418` on a spec without an `418` response returns `404`, and
`Prefer: example=nope` on a response without that named example returns `404`.

#### Content Negotiation

The `Accept` header selects among all media types declared for the selected response. Using
the bundled `docs/specs/reports.openapi.yaml` example:

```bash
cargo run -p spec-mock -- serve \
  --openapi docs/specs/reports.openapi.yaml \
  --http-addr 127.0.0.1:4010

curl -H "Accept: application/json" http://127.0.0.1:4010/report
curl -H "Accept: text/plain" http://127.0.0.1:4010/report
curl -H "Accept: application/xml" http://127.0.0.1:4010/inventory
```

Response bodies are encoded per media type:

| Media type family                          | Body                                                    |
| ------------------------------------------ | ------------------------------------------------------- |
| `application/json`, `*+json`               | compact JSON                                             |
| `application/xml`, `text/xml`, `*+xml`     | XML document rooted at `xml.name` / `title` / `root`      |
| `text/*`, `application/yaml`, …            | the string value verbatim, otherwise JSON                |
| `image/*`, `application/octet-stream`, …   | the string value verbatim, otherwise JSON                |

An `Accept` header that matches none of the declared media types returns `406`:

```bash
curl -i -H "Accept: application/vnd.custom+report" http://127.0.0.1:4010/report
```

Declared response headers are emitted when their status is selected:

```bash
curl -i -H "Prefer: code=429" http://127.0.0.1:4010/limited
```

Requests are checked the same way: a non-empty body whose `Content-Type` is not declared by
the operation returns `415`, and only `application/json` bodies are schema-validated.
Non-JSON bodies satisfy `requestBody.required` but are not parsed.

### 2. AsyncAPI WebSocket mock server

Supports both AsyncAPI v2.x and v3.x specs.

```bash
cargo run -p spec-mock -- serve \
  --asyncapi docs/specs/chat.asyncapi.yaml \
  --http-addr 127.0.0.1:4011
```

WebSocket endpoint: `ws://127.0.0.1:4011/ws`

Use `--ws-path` to move the WebSocket endpoint:

```bash
cargo run -p spec-mock -- serve \
  --asyncapi docs/specs/chat.asyncapi.yaml \
  --ws-path /socket
```

Per-channel endpoints are derived from the base path, so the example above also serves
`ws://127.0.0.1:4011/socket/chat.send`.

Input envelope options:

- Explicit channel envelope: `{"channel":"chat.send","payload":{...}}`
- Alias envelope: `{"topic":"chat.send","data":{...}}`
- Auto-routing: send raw payload and runtime matches by publish schema.

### 3. Protobuf gRPC mock server

Serves HTTP/2 over `hyper-util` with hand-rolled gRPC framing and trailers; `tonic` is used
only for its `Status` type, not its transport. Supports unary and server-streaming RPCs.
Client-streaming, bidi-streaming, and server reflection are **not** supported — `grpcurl`
requires an explicit `-proto`.

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
  --ws-path <PATH>        WebSocket base path [default: /ws]
  --max-body-size <BYTES> Maximum request body size in bytes [default: 10485760]
  --allow-private-upstream  Allow private/loopback/link-local proxy upstreams
  --scenarios <PATH>      Scenario sidecar declaring stateful behaviour
  --max-sessions <N>      Cap on live scenario instances [default: 256]
  --max-store-entries <N> Cap on stored resources per instance [default: 1024]
  --log-level <LEVEL>     Log verbosity [default: info]
                           [possible values: error, warn, info, debug, trace]
  --no-control-endpoints  Disable the control endpoints under /_specmock/
```

## Stateful Mocking

By default every request is independent: the mock answers from the schema, and
nothing is remembered. A **scenario sidecar** changes that. It declares named
flows whose responses depend on what already happened, plus a store so a client
can create a resource and read that same resource back.

Behaviour lives in a sidecar resolved *beside* the OpenAPI document, not in
`x-specmock-*` extensions inside it. Requiring extensions would mean forking the
input spec, and forking breaks every `$ref` into shared schema files — after
which the mock is no longer exercising the real contract. See
[ADR 0002](docs/adr/0002-behaviour-in-sidecar-file-not-spec-extensions.md).

```bash
cargo run -p spec-mock -- serve \
  --openapi docs/specs/store.openapi.yaml \
  --scenarios docs/specs/store.scenarios.yaml \
  --log-level info
```

```yaml
# docs/specs/store.scenarios.yaml
version: 1

operations:
  createWidget:
    match: { method: POST, path: /widgets }
    store:
      resource: /widgets
      action: create
      keyFrom: { in: request, pointer: /id }
    responses:
      "201": {}

  getWidget:
    match: { method: GET, path: "/widgets/{id}" }
    store: { resource: /widgets, action: read, key: "{id}" }
    responses:
      "200": {}

  listWidgets:
    match: { method: GET, path: /widgets }
    store: { resource: /widgets, action: list }
    responses:
      "200": {}

scenarios:
  - name: catalogue
    initialState: browsing
    seed:
      - resource: /widgets
        items:
          - { id: seeded-1, name: Preloaded Widget }
    states:
      browsing:
        createWidget: { to: browsing }
        getWidget: {}
        listWidgets: {}
```

Create a resource, then read back exactly what was created:

```bash
curl -sX POST localhost:4010/widgets \
  -H 'Content-Type: application/json' \
  -d '{"id":"w1","name":"First"}'

curl -s localhost:4010/widgets/w1     # => {"id":"w1","name":"First"}
curl -s localhost:4010/widgets        # => [{"id":"seeded-1",...},{"id":"w1",...}]
```

> **Quote a path containing `{...}`.** In YAML flow style an unquoted
> `{id}` starts a nested mapping rather than a string. Use block style or quotes.

### Selecting a scenario and session

| Header | Effect |
| --- | --- |
| `Prefer: scenario=<name>` | Run that scenario. Absent, the first declared one. |
| `Prefer: session=<key>` | Address an isolated instance. Absent, one default instance. |

Session keying is opt-in over a singleton default: embedded mode already gives
each test its own server, while process mode shares one child across the whole
session. Tests that run in parallel ask for their own key.

### Store semantics

| Action | Behaviour when the resource is absent |
| --- | --- |
| `create` | Stored. A colliding identifier is `409`. |
| `read` | `404` |
| `update` (`PUT`/`PATCH`) | `404`. Not an upsert. |
| `delete` | `404`, and later reads also `404` |
| `list` | `[]`, in creation order |

Update is deliberately not an upsert. RFC 9110 permits create-on-`PUT`, but for a
mock the `404` is the product: a test that updates something it never created has
a bug, and upserting would let it pass green.

Keys are `(collection path template with parameters bound, identifier)`, so
`/users/5/orders` and `/orders` never collide on a shared `id`.

Query parameters are **not** applied to `list` — no filtering, no pagination. A
partially-honoured list is worse than a documented simple one.

### Fault injection

An error is a transition into a state that declares one, not a separate mechanism:

```yaml
    states:
      healthy:
        getWidget: { to: failing, responses: { "503": { body: { error: unavailable } } } }
      failing:
        getWidget: {}
```

`Prefer: code=` still works and short-circuits the machine entirely — see
[ADR 0001](docs/adr/0001-fold-fault-injection-into-scenario-model.md). So does
`Prefer: dynamic=true`, which bypasses the store. Both are read-only escape
hatches and neither advances state, so neither changes meaning depending on
whether a scenario is active.

### Errors

Stateful failures are RFC 7807 with structured extension members, not
validation issues. An operation the state machine refuses has no instance and no
schema pointer, so forcing it into a `ValidationIssue` would mean writing
pointers that point at nothing.

| Condition | Status |
| --- | --- |
| Unknown scenario name | `404`, `detail` lists the scenarios that exist |
| Operation not legal in the current state | `409` |
| Unknown resource | `404` |
| Create identifier already taken | `409` |
| Live session cap exceeded | `429`, `detail` names `--max-sessions` |
| Store entry cap exceeded | `500`, `detail` names `--max-store-entries` |

```json
{
  "type": "about:blank",
  "title": "Conflict",
  "status": 409,
  "detail": "operation 'getWidget' is not legal in state 'spent' of scenario 'once' (session '__default__')",
  "scenario": "once",
  "session": "__default__",
  "state": "spent",
  "operation": "getWidget"
}
```

### Determinism

With a sidecar the guarantee becomes **same seed + same request *sequence*
produces the same response sequence**. A store is irreducibly order-dependent:
delete-then-read-must-`404` requires remembering that a deletion happened. See
[ADR 0003](docs/adr/0003-determinism-is-sequence-based.md). State does not
survive a restart.

An invalid request changes nothing — not the state, not the store. The client is
the thing under test, so a client bug must not poison every later assertion. A
store `404` is different: it is a legitimate outcome of a *valid* request, so it
does advance.

### Control endpoints

Process mode has no handle into a running child, so state is reachable over HTTP
under the reserved `/_specmock/` prefix. It is the only mutating surface on the
server, so it is enabled only with `--scenarios` or `.control_endpoints()` and
every request must present the token printed at startup.

```bash
curl -H "Authorization: Bearer $TOKEN" localhost:4010/_specmock/state
curl -X POST -H "Authorization: Bearer $TOKEN" localhost:4010/_specmock/reset
```

Embedded mode uses the handle instead:

```rust
let server = MockServer::builder()
    .openapi("docs/specs/store.openapi.yaml")
    .scenarios("docs/specs/store.scenarios.yaml")
    .start()
    .await?;

server.reset_scenarios();               // state and store, together
let (scenario, state) = server.scenario_state("").unwrap();
```

### WebSocket and gRPC

The same sidecar drives all three protocols; only the addressing differs.

| Protocol | Operation key | Scenario and session | Identifier source |
| --- | --- | --- | --- |
| HTTP | `post /widgets` | `Prefer:` headers | path parameter (`key`) |
| WebSocket | `channel widgets.created` | `?scenario=&session=` on the handshake | message field (`keyFrom`) |
| gRPC | `unary /widgets.Widgets/Create` | lowercase metadata | message field (`keyFrom`) |

A WebSocket connection is its own session by default and is released when it
closes; `?session=` opts into sharing. Over gRPC, a `create` stores the
*generated reply* rather than the request, because a gRPC request and its
response are distinct protobuf types — storing the request would hand the client
bytes of the wrong message. The stored message's identifier is aligned with the
key, so reading back by the identifier the client was just given succeeds.

Over gRPC a `responses` key is a gRPC status code, not an HTTP status:

```yaml
    states:
      steady:
        unstableWidget: { to: broken }
      broken:
        unstableWidget: { responses: { "13": {} } }   # INTERNAL
```

### Not supported

- Scenarios in proxy mode. The upstream holds the real state; startup rejects the
  combination and names both settings.
- Filtering or pagination on `list`.
- State surviving a restart.
- Session expiry. A TTL would reintroduce wall-clock dependence into a tool whose
  pitch is reproducibility.
- Atomic read-modify-write. Two concurrent `PUT`s to one resource may interleave.

Absent a sidecar, every existing test behaves exactly as it did before scenarios
existed. That regression guard is enforced by
`an_absent_sidecar_leaves_plain_mocking_untouched`.

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

`MockServer::ws_url()` reflects the configured WebSocket path, so AsyncAPI specs can be
served on any route:

```rust
# use specmock_sdk::MockServer;
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let server = MockServer::builder()
    .asyncapi("docs/specs/chat.asyncapi.yaml")
    .ws_path("/socket")
    .start()
    .await?;

assert!(server.ws_url().ends_with("/socket"));
server.shutdown().await;
# Ok(())
# }
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

## Testing Strategy

The test suite is layered so that each layer catches a different class of defect.

| Layer | Where | Runs in |
| --- | --- | --- |
| Unit tests | `#[cfg(test)]` modules next to the code | `just test` |
| Property tests | `proptest` blocks in the same modules | `just test` |
| Integration / E2E | `crates/*/tests/*.rs` | `just test` |
| Fuzzing | `crates/*/fuzz/fuzz_targets/*.rs` | `just fuzz` |
| Coverage | instrumented re-run of the suite | `just test-coverage` |

Property tests reseed on every run, so a single green run is not evidence that a
property is sound. Run the suite a few times, and commit any
`proptest-regressions/` files that appear — each one is a minimal counterexample
that should stay covered from then on.

### Unit and property tests

`just test` runs everything, including the `proptest` properties — they are part
of the normal loop, not a separate command. Each module pairs example-based
tests for named cases with properties for invariants that must hold across many
generated inputs:

- `faker` — generation is deterministic per seed, integer/string/array bounds are
  always honoured, and hostile schemas never panic.
- `validate` — the validator cache key is insensitive to member ordering but
  sensitive to any leaf change, so two schemas never share a validator.
- `ref_resolver` — JSON-pointer navigation returns the addressed node or `None`,
  and `split_ref` always partitions the input on the first `#`.
- `schema` — extracted mappings are always sorted and stringly typed.
- `router` — declared templates always match their instantiation, while
  unrelated paths never match.
- `media` — the JSON/XML/text/binary families stay mutually exclusive, XML output
  is always a balanced single-rooted tree, and raw metacharacters never survive
  escaping.
- `negotiate` — content negotiation always yields a declared media type and
  preference parsing never panics.
- `openapi` — parsing an arbitrary document never panics.
- `protobuf` — gRPC framing round-trips exactly, and decoding arbitrary bytes
  either succeeds within the frame bounds or fails cleanly.
- `asyncapi` — every handled message returns a well-formed outcome.
- `runtime` — startup hashing is total and deterministic, and error rendering
  never leaks absolute paths.
- `sdk` — the configuration builder accepts any chain of settings without panicking.

### Fuzzing

Fuzz targets live in the crate that owns the code under test and are excluded
from the workspace, so they never slow down `cargo test`:

```bash
just fuzz-build          # compile the targets
just fuzz 60             # 60s per target
just fuzz-smoke 10       # short CI-friendly run
just ci-fuzz             # lint + test + build + smoke fuzz
```

| Target | Crate | Invariant |
| --- | --- | --- |
| `ref_resolver` | `specmock-core` | `$ref` resolution of hostile documents and ref strings never panics |
| `json_pointer` | `specmock-core` | pointer navigation never returns a value absent from the document |
| `faker` | `specmock-core` | generation is deterministic, bounded, and panic-free |
| `validate` | `specmock-core` | validation is total and its issues are always fully populated |
| `media_encode` | `specmock-runtime` | encoding never panics; JSON round-trips; XML stays balanced |
| `negotiate` | `specmock-runtime` | negotiation only ever returns a declared media type |

### Integration Tests (Prism Comparison)

The `integration-test` command runs a side-by-side comparison suite against [Prism](https://stoplight.io/open-source/prism) to verify that spec-mock and Prism return structurally equivalent responses.

**Prerequisites:**

```bash
npm install -g @stoplight/prism-cli
```

**Run:**

```bash
just integration-test
```

**Configuration via environment variables:**

| Variable | Default | Description |
| --- | --- | --- |
| `SPECMOCK_FUZZ_SEED` | `42` | Random seed for reproducible fuzz requests |
| `SPECMOCK_FUZZ_ITERATIONS` | `5` | Number of fuzz iterations per operation |
| `SPECMOCK_PRISM_CMD` | auto-detected | Path to Prism binary or `npx` invocation |

**Example with custom seed:**

```bash
SPECMOCK_FUZZ_SEED=123 SPECMOCK_FUZZ_ITERATIONS=20 just integration-test
```

**CI (GitHub Actions example):**

```yaml
- name: Install Prism
  run: npm install -g @stoplight/prism-cli
- name: Run integration tests
  run: just integration-test
```

**Known behavioral divergences:**

- Wrong Content-Type handling: spec-mock returns `415 Unsupported Media Type`; Prism returns `422` or `400`. Both are valid 4xx responses. This divergence is documented and expected.
- Unsatisfiable `Accept`: spec-mock returns `406 Not Acceptable`; Prism falls back to the first declared media type.

## Example Specs

- OpenAPI: `docs/specs/pets.openapi.yaml`
- OpenAPI (media types): `docs/specs/reports.openapi.yaml`
- AsyncAPI: `docs/specs/chat.asyncapi.yaml`
- Protobuf: `docs/specs/greeter.proto`

## License

Apache-2.0
