# Media-Type Parity and Runtime Completeness — Tasks

| Metadata | Details |
| :--- | :--- |
| **Design Doc** | specs/2026-10-02-01-media-type-parity/design.md |
| **Status** | Complete |

## Summary & Timeline

| Phase | Tasks | Estimated Effort | Depends On |
| --- | --- | --- | --- |
| 1 — Media types & negotiation | 1.1–1.6 | Medium-Large | — |
| 2 — Mock data fidelity | 2.1–2.2 | Medium | Phase 1 |
| 3 — Configuration & parity | 3.1–3.2 | Small | Phase 1 |
| 4 — Performance | 4.1 | Small | Phase 1 |
| 5 — Verification | 5.1–5.2 | Small | Phase 2, 3, 4 |

---

## Phase 1: Media types & negotiation

### Task 1.1: Add the `not_acceptable` problem type

- **Status:** 🟢 DONE
- [x] Add `ProblemDetails::not_acceptable` in `specmock-core/src/error.rs` (406, "Not Acceptable").
- [x] Add `RuntimeError::NotAcceptable` in `specmock-runtime/src/lib.rs`.
- [x] Unit test asserting status and title.

### Task 1.2: Classify and encode media types

- **Status:** 🟢 DONE
- [x] Create `crates/specmock-runtime/src/http/media.rs`.
- [x] Implement `normalize_media_type`, `is_json_media_type`, `is_xml_media_type`,
      `is_text_media_type`, `is_binary_media_type`.
- [x] Implement `media_type_is_accepted` with `type/*`, `*/*`, and `+json` rules.
- [x] Implement `encode_body` and `json_to_xml` (array wrapper elements, escaping, name
      sanitizing).
- [x] Unit tests for classification, accept matching, and every encoding branch.

### Task 1.3: Parse every declared response media type and header

- **Status:** 🟢 DONE
- [x] Replace `ResponseSpec::{schema, example, named_examples}` with `content: Vec<MediaTypeSpec>`
      and `headers: BTreeMap<String, ResponseHeaderSpec>`.
- [x] Sort content so the JSON family comes first, preserving declaration order otherwise.
- [x] Parse named examples that are bare values as well as Example Objects.
- [x] Add `RequestBodySpec` carrying `schema`, `required`, and `media_types`.
- [x] Unit tests for ordering, lookup, header parsing, and request media types.

### Task 1.4: Negotiate and build responses in the handler

- **Status:** 🟢 DONE
- [x] `mock_response` negotiates a media type, returning `NotAcceptable` on an unsatisfiable
      `Accept`, and keeping bodyless responses bodyless.
- [x] `MockHttpResponse` carries `media_type`, `schema`, and generated `headers`.
- [x] Generate response header values from `example` or the faker with a per-header seed.
- [x] `build_mock_response` sets `Content-Type` and encodes the body per media type.
- [x] Unit tests for negotiation, 406, headers, and unknown named examples.

### Task 1.5: Validate request media types

- **Status:** 🟢 DONE
- [x] Return `415` with the declared media types when the body `Content-Type` is undeclared.
- [x] Add `body_present` to `validate_request` so non-JSON payloads satisfy `required`.
- [x] Integration tests for accepted and rejected request media types.

### Task 1.6: Integration coverage and example spec

- **Status:** 🟢 DONE
- [x] Add `tests/specs/openapi-media-negotiation.yaml` and `docs/specs/reports.openapi.yaml`.
- [x] Integration tests for text, XML, binary, wildcard `Accept`, `406`, declared response
      headers, bodyless responses, request `415`, and unknown `Prefer` values.

## Phase 2: Mock data fidelity

### Task 2.1: Seed-driven JSON faker selection

- **Status:** 🟢 DONE
- [x] Select `enum` values with the RNG instead of the first entry.
- [x] Select `oneOf`/`anyOf` variants with the RNG when no discriminator is present.
- [x] Align discriminator variant selection with the mapping so the discriminator value
      matches the generated payload; support `x-specmock-variant` and `title` hints.
- [x] Make `Discriminator::mapping` a `BTreeMap` for deterministic ordering.
- [x] Tests for seed variation, determinism, discriminator/variant consistency, and
      generated data validating against its own schema.

### Task 2.2: Seed-driven gRPC enum selection

- **Status:** 🟢 DONE
- [x] Add `rand`/`rand_chacha` to `specmock-runtime` via `cargo add`.
- [x] Select protobuf enum values with `ChaCha8Rng` seeded from the field seed.
- [x] Add `tests/specs/enum-service.proto` plus integration tests for variation and
      determinism.

## Phase 3: Configuration & parity

### Task 3.1: Configurable WebSocket base path

- **Status:** 🟢 DONE
- [x] Add `--ws-path` to the CLI and `MockServerBuilder::ws_path`.
- [x] Forward `--ws-path` in SDK process mode.
- [x] Tests: `ws_url()` follows the configured path, per-channel routes follow the base
      path, the default path stops serving, invalid paths are rejected.

### Task 3.2: Unsatisfiable `example` preference

- **Status:** 🟢 DONE
- [x] `Prefer: example=<unknown>` returns `404` problem+json instead of falling back.
- [x] Integration tests for unknown `example` and unknown `code`.

## Phase 4: Performance

### Task 4.1: Hash schemas without serializing

- **Status:** 🟢 DONE
- [x] Replace `serde_json::to_string` with a length-prefixed recursive hash of the value tree.
- [x] Tests: ordering-independent keys, distinct keys for distinct schemas, nested and empty
      containers, and continued enforcement of schema constraints.

## Phase 5: Verification

### Task 5.1: Full gate

- **Status:** 🟢 DONE
- [x] `just format`, `just lint`, `just test` all pass with no warnings.

### Task 5.2: Runtime smoke test

- **Status:** 🟢 DONE
- [x] Serve `docs/specs/reports.openapi.yaml` and verify JSON, `text/plain`, `application/xml`,
      `406`, `415`, and declared response headers with `curl`.
- [x] Document the behavior, the new CLI flag, and the Prism divergences in `README.md`.
