# Design: Media-Type Parity and Runtime Completeness

Generated on 2026-10-02 from the `2026-06-27-01-codebase-quality` review, which closed its
task list while several of its claims were not actually implemented.

## Context

`spec-mock` advertises Prism parity plus capabilities Prism lacks (AsyncAPI, gRPC,
callbacks, Rust SDK). A gap analysis against the shipped code found that the advertised
HTTP surface was only partially wired:

- `http::negotiate::negotiate_media_type` existed with unit tests but had **no call sites**.
  `PreferDirectives::media_type` was parsed from `Accept` and never read.
- `parse_responses` only ever read `application/json` from a response `content` map.
  Responses declared as `text/plain`, `application/xml`, or binary produced an **empty body
  with no `Content-Type` header**.
- `faker.rs` still selected `enum` and `oneOf`/`anyOf` variants with `.first()`, despite
  task 4.1 being checked off. The discriminator value was taken from
  `mapping.keys().next()`, which can contradict the generated payload.
- The gRPC faker always emitted the first protobuf enum value; this was never covered by
  the quality spec at all.
- `ServerConfig::ws_path` was unreachable: no CLI flag, no SDK builder method, and process
  mode never forwarded it.
- `Prefer: example=<unknown>` silently fell back to schema generation, contradicting the
  `Prefer: code=<unknown>` → 404 rule from task 8.1.
- `validate.rs` still serialized every schema to a JSON string before hashing, despite
  task 21.1 claiming the serialization was removed.

## Goals

1. Make every declared response media type produce a real body with a correct `Content-Type`.
2. Honor `Accept` end-to-end, including `406` when nothing acceptable is declared.
3. Emit declared response headers.
4. Make mocked enum/oneOf/anyOf data seed-varied and internally consistent.
5. Make the WebSocket base path configurable from the CLI and the SDK.
6. Report unsatisfiable `example` preferences instead of silently falling back.
7. Remove the per-request JSON serialization from the validator cache lookup.

## Non-goals

- Parsing or schema-validating non-JSON request bodies (only JSON bodies are validated).
- Full XML/XSD fidelity; XML is generated from the JSON value with a schema-derived root name.
- gRPC client-streaming, which stays unsupported.
- Callback payload templating beyond `{$request.body#/pointer}`.

## Design

### Response content model

`ResponseSpec` replaces its JSON-only `schema`/`example`/`named_examples` fields with:

```rust
pub struct ResponseSpec {
    pub status: String,
    pub content: Vec<MediaTypeSpec>,       // JSON family first, then declaration order
    pub headers: BTreeMap<String, ResponseHeaderSpec>,
}
```

Ordering matters: a request without `Accept` must keep receiving JSON even when a spec
declares `text/plain` first, so entries are stable-sorted with the JSON family in front.

`MockHttpResponse` gains `media_type`, `schema` (the schema the payload came from, used to
derive XML root names) and `headers`. The body stays a `serde_json::Value` so SDK consumers
and tests keep a structured view; encoding happens at the HTTP boundary.

### Media-type classification and encoding

`http::media` owns four concerns:

- `normalize_media_type` — strip parameters, lowercase.
- `is_json_media_type` / `is_xml_media_type` / `is_text_media_type` / `is_binary_media_type`.
- `encode_body` — JSON → compact JSON; XML → `json_to_xml`; text/binary → the string value
  verbatim, otherwise JSON.
- `media_type_is_accepted` — request-side matching with `type/*`, `*/*`, and the rule that
  `application/json` satisfies a declared `application/<sub>+json`.

`json_to_xml` wraps arrays in the property element containing repeated `<item>` children,
escapes text, and sanitizes element names.

### Negotiation flow

`OperationSpec::mock_response` resolves the response, then negotiates:

1. No declared content (e.g. `204`, headers-only errors) → bodyless response.
2. `Accept` unsatisfiable → `RuntimeError::NotAcceptable` → `406` problem+json.
3. `Prefer: example=` present but missing → `RuntimeError::NotFound` → `404` problem+json.
4. Otherwise resolve the payload per media type: named example → `example` → first named
   example → faker over `schema`.

### Request-side content type

The handler checks a non-empty body against `request_body_media_types` and returns `415`
with the declared list in `detail`. `OperationSpec::validate_request` gained a
`body_present` argument: `requestBody.required` is satisfied by any payload, while schema
validation still only applies to parsed JSON bodies.

### Faker seeding

- `enum` selects with `rng.random_range`.
- `oneOf`/`anyOf` selects the variant with `rng` when there is no discriminator.
- With a discriminator, the variant is chosen from the mapping so the generated
  discriminator value matches the generated payload. Variants are aligned to mapping keys by
  the optional `x-specmock-variant` hint or by `title`, because `RefResolver` inlines `$ref`
  nodes before generation. Without alignment the original random choice is kept.
- gRPC selects protobuf enum values with a `ChaCha8Rng` seeded from the field seed.

### Validator cache key

`hash_value` hashes the `serde_json::Value` tree directly, length-prefixing every container,
instead of serializing to a string first. Member ordering must not change the key because
`serde_json::Map` ordering is an implementation detail.

## Verification

- 223 tests pass (`just test`), clippy pedantic+nursery clean (`just lint`).
- New coverage: media classification/encoding, negotiation, response headers, request 415,
  406, unknown example, bodyless responses, seeded enum/oneOf/discriminator, gRPC enum
  variation and determinism, custom WebSocket path, validator cache key properties.
- Manual smoke test against `docs/specs/reports.openapi.yaml` for each media type.
