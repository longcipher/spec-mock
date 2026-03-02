# Prism Comparison Fuzz Testing — Tasks

| Metadata | Details |
| :--- | :--- |
| **Design Doc** | specs/2026-03-02-02-prism-comparison-fuzz/design.md |
| **Status** | Planning |

## Summary & Timeline

| Phase | Tasks | Estimated Effort | Depends On |
| --- | --- | --- | --- |
| 1 — Harness Foundation | 1.1–1.3 | Medium | — |
| 2 — Fuzzer | 2.1–2.4 | Large | Phase 1 |
| 3 — Comparator | 3.1 | Medium | Phase 1 |
| 4 — Comparison Spec & Tests | 4.1–4.3 | Large | Phase 2, 3 |
| 5 — Polish & CI | 5.1–5.2 | Small | Phase 4 |

---

## Phase 1: Harness Foundation

### Task 1.1: Add `integration-test` Feature Gate and Justfile Command

> **Context:** Comparison tests depend on Prism (external npm tool) and should not run during normal `cargo test`. Add a Cargo feature flag `integration-test` to `specmock-runtime` and a `just integration-test` command. The existing `just test` and `just ci` commands remain unchanged.
> **Verification:** `just integration-test` invokes `cargo test --test prism_comparison --features integration-test`. Feature compiles but test file can be a placeholder.

- [x] Step 1: Add `[features] integration-test = []` to `crates/specmock-runtime/Cargo.toml`
- [x] Step 2: Add `integration-test` recipe to `Justfile`:

  ```just
  # Run Prism comparison integration tests (requires Prism: npm i -g @stoplight/prism-cli)
  integration-test:
    cargo test --test prism_comparison --features integration-test -- --nocapture --ignored
  ```

- [x] Step 3: Create placeholder `crates/specmock-runtime/tests/prism_comparison.rs` with a single `#[test]` gated by `#[cfg(feature = "integration-test")]` that prints "placeholder"
- [x] Step 4: Run `just integration-test` — verify the placeholder test runs
- [x] Verification: `just integration-test` runs the placeholder test. `just test` does NOT run `prism_comparison` tests.

### Task 1.2: Implement Prism Process Manager (`harness/prism.rs`)

> **Context:** The harness needs to start Prism as a child process on a random port, wait for it to become ready, and kill it on drop. Prism does not support `--port 0`; we must pick an available port manually. Uses `std::process::Command` (not `tokio::process`) to keep it simple. Prism is started via `npx @stoplight/prism-cli mock <spec> --port <port> --host 127.0.0.1` or bare `prism mock <spec> --port <port> --host 127.0.0.1`. Readiness is checked by polling with `hpx`.
> **Verification:** Unit test starts Prism with the existing `openapi-pets.yaml` spec and confirms a GET request returns 200. Test skips if Prism is not installed.

- [x] Step 1: Create `crates/specmock-runtime/tests/harness/mod.rs` with `pub mod prism; pub mod request; pub mod fuzzer; pub mod comparator;` (fuzzer/comparator can be empty stubs)
- [x] Step 2: Create `crates/specmock-runtime/tests/harness/request.rs` with `FuzzRequest` and `CapturedResponse` structs:
  - `FuzzRequest { method, path, query, headers, body, content_type, description }`
  - `CapturedResponse { status: u16, content_type: Option<String>, body: Vec<u8>, headers: Vec<(String, String)> }`
- [x] Step 3: Create `crates/specmock-runtime/tests/harness/prism.rs` with `PrismServer` struct:
  - `find_prism_command()` — checks `SPECMOCK_PRISM_CMD` env, then `npx @stoplight/prism-cli`, then `prism` on PATH
  - `find_available_port()` — bind to `127.0.0.1:0`, extract port, close socket
  - `PrismServer::start(spec_path, port_retries: 3)` — spawn child process, poll readiness
  - `PrismServer::base_url()` — returns `http://127.0.0.1:{port}`
  - `impl Drop for PrismServer` — kills child process
- [x] Step 4: Add `hpx` to `[dev-dependencies]` in `specmock-runtime/Cargo.toml` if not already present
- [x] Step 5: Write a small integration test in `prism_comparison.rs` that starts Prism with `openapi-pets.yaml`, sends `GET /pets/1`, asserts status 200, and drops the server
- [x] Verification: `just integration-test` passes if Prism is installed; prints skip message and passes if not installed.

### Task 1.3: Implement Request Sender Utility

> **Context:** A shared function sends a `FuzzRequest` to a given base URL and returns a `CapturedResponse`. Both spec-mock and Prism are hit with the same function. Uses `hpx::Client` with a 5-second timeout.
> **Verification:** Function compiles and is used by the Prism startup test from Task 1.2.

- [x] Step 1: Add `send_request(client: &hpx::Client, base_url: &str, req: &FuzzRequest) -> Result<CapturedResponse>` to `harness/request.rs`
- [x] Step 2: Handle method dispatch, query string construction, header injection, body attachment
- [x] Step 3: Capture status, content-type header, full body bytes, and all response headers
- [x] Step 4: Refactor Task 1.2's Prism readiness check and test to use `send_request`
- [x] Verification: Existing Prism startup test still passes using the shared sender.

---

## Phase 2: Fuzzer

### Task 2.1: OpenAPI Spec Parser for Fuzzing

> **Context:** The fuzzer needs to parse an OpenAPI spec and extract: paths, operations, parameters (path/query/header), request body schemas, response codes, named examples, and content types. Reuse `serde_json::Value` and `serde_yml` for parsing (already in specmock-core dependencies). This parser is test-only code — it doesn't need to be as robust as the production OpenAPI parser in specmock-runtime, but must handle the features present in the comparison spec.
> **Verification:** Parser correctly extracts operations from `openapi-prism-comparison.yaml` (created in Task 4.1).

- [ ] Step 1: Create `crates/specmock-runtime/tests/harness/fuzzer.rs`
- [ ] Step 2: Define `OperationInfo` struct: `{ method, path_template, path_params, query_params, header_params, request_body_schema, response_codes, named_examples, content_types }`
- [ ] Step 3: Define `ParamInfo` struct: `{ name, location, required, schema }`
- [ ] Step 4: Implement `parse_spec(spec: &Value) -> Vec<OperationInfo>` — walks `paths` → HTTP methods → extracts parameters, requestBody, responses
- [ ] Step 5: Handle `$ref` within the spec (should already be resolved by the time tests use it, but handle inline schemas directly)
- [ ] Verification: Write a unit test that parses `openapi-pets.yaml` and asserts correct operation extraction (1 GET operation, 1 path param).

### Task 2.2: Valid Request Generator

> **Context:** For each operation, generate N requests with valid parameters. Path params are generated according to schema type/constraints. Query params are generated for required params always, optional params randomly. Request bodies are generated from the request body schema using simple random value generation (integers within min/max, strings of valid length, etc.). Uses `rand_chacha::ChaCha8Rng` with configurable seed.
> **Verification:** Generator produces valid requests for each operation in the comparison spec, and both spec-mock and Prism return 2xx.

- [ ] Step 1: Implement `generate_value(schema: &Value, rng: &mut ChaCha8Rng) -> Value` — simple JSON value generator from schema constraints (integer, string, boolean, array, object, enum)
- [ ] Step 2: Implement `generate_path(template: &str, params: &[ParamInfo], rng) -> String` — fills path template with valid generated values
- [ ] Step 3: Implement `generate_query_params(params: &[ParamInfo], rng) -> Vec<(String, String)>` — generates required + random optional query params
- [ ] Step 4: Implement `generate_body(schema: &Value, rng) -> Option<Vec<u8>>` — JSON body from request body schema
- [ ] Step 5: Implement `OpenApiFuzzer::generate_valid_requests() -> Vec<FuzzRequest>` — combines above for each operation, N iterations
- [ ] Verification: Generated requests are syntactically valid (valid JSON bodies, correct path param types).

### Task 2.3: Invalid Request Generator

> **Context:** Generate requests designed to trigger error responses. Categories: invalid path param type (string for integer), missing required query param, invalid request body (wrong types, missing required fields), wrong Content-Type, unknown path. Both servers should return consistent error status codes.
> **Verification:** Generator produces invalid requests and both spec-mock and Prism return 4xx status codes.

- [ ] Step 1: Implement `generate_invalid_path_param_requests(ops, rng) -> Vec<FuzzRequest>` — e.g., "abc" for integer path param
- [ ] Step 2: Implement `generate_missing_required_query_requests(ops, rng) -> Vec<FuzzRequest>` — omit required query params
- [ ] Step 3: Implement `generate_invalid_body_requests(ops, rng) -> Vec<FuzzRequest>` — wrong types, missing required fields
- [ ] Step 4: Implement `generate_wrong_content_type_requests(ops) -> Vec<FuzzRequest>` — `text/plain` for `application/json` endpoints
- [ ] Step 5: Implement `generate_unknown_path_requests(rng) -> Vec<FuzzRequest>` — paths not in spec
- [ ] Step 6: Wire all into `OpenApiFuzzer::generate_invalid_requests()`
- [ ] Verification: Each generated invalid request category triggers the expected error class.

### Task 2.4: Prefer and Accept Header Request Generator

> **Context:** Generate requests with `Prefer: code=xxx`, `Prefer: example=xxx`, `Prefer: dynamic=true`, and `Accept` headers for content negotiation testing. These are deterministic — one request per defined response code, one per named example, one dynamic, one per alternative content type.
> **Verification:** Prefer-code requests return matching status from both servers. Prefer-example requests return matching body from both servers.

- [ ] Step 1: Implement `generate_prefer_code_requests(ops) -> Vec<FuzzRequest>` — one per response status code per operation
- [ ] Step 2: Implement `generate_prefer_example_requests(ops) -> Vec<FuzzRequest>` — one per named example per operation
- [ ] Step 3: Implement `generate_prefer_dynamic_requests(ops) -> Vec<FuzzRequest>` — one per operation
- [ ] Step 4: Implement `generate_accept_requests(ops) -> Vec<FuzzRequest>` — one per alternative content type per operation
- [ ] Step 5: Wire into `OpenApiFuzzer::generate_prefer_requests()` and `generate_accept_requests()`
- [ ] Verification: Generated requests have correct `Prefer` and `Accept` headers.

---

## Phase 3: Comparator

### Task 3.1: Implement Structural Response Comparator

> **Context:** Compare two `CapturedResponse` structs and produce a `ComparisonResult` with detailed diffs. Status code comparison is exact. Body comparison is structural (JSON key presence, value types, array length ranges). Content-Type comparison ignores charset. RFC 7807 fields `type`, `title`, `status` are compared exactly; `detail` is ignored; `errors` array length is compared.
> **Verification:** Unit tests cover: identical responses → pass, status mismatch → fail, missing key → fail, type mismatch → fail, different values same type → pass (for dynamic mode).

- [ ] Step 1: Define `ComparisonResult`, `Diff`, `Server` enums/structs in `harness/comparator.rs`
- [ ] Step 2: Implement `compare_status(specmock: u16, prism: u16) -> Option<Diff>`
- [ ] Step 3: Implement `compare_content_type(specmock: &str, prism: &str) -> Option<Diff>` — strip charset
- [ ] Step 4: Implement `compare_json_structure(specmock: &Value, prism: &Value, path: &str) -> Vec<Diff>` — recursive key/type comparison
- [ ] Step 5: Implement `compare_rfc7807(specmock: &Value, prism: &Value) -> Vec<Diff>` — special handling for Problem Details
- [ ] Step 6: Implement `compare_responses(specmock: &CapturedResponse, prism: &CapturedResponse, category: &RequestCategory) -> ComparisonResult` — orchestrates above
- [ ] Step 7: Implement `ComparisonResult::summary() -> String` for human-readable diff output
- [ ] Step 8: Write unit tests for each comparison function: identical responses, status mismatch, missing key, type mismatch, array length mismatch, RFC 7807 partial match
- [ ] Verification: `cargo test --test prism_comparison --features integration-test` comparator unit tests pass.

---

## Phase 4: Comparison Spec & Tests

### Task 4.1: Create Comprehensive Comparison OpenAPI Spec

> **Context:** A single OpenAPI 3.1 spec that exercises all features supported by both Prism and spec-mock. This spec must be valid and produce meaningful mock data from both servers. It should ONLY include features that Prism supports (no callbacks/webhooks, which Prism doesn't handle). Reference existing test specs for patterns (`openapi-negotiate.yaml`, `openapi-polymorphic.yaml`, `openapi-content-types.yaml`, `openapi-array-params.yaml`).
> **Verification:** Both `spec-mock` and `prism` start successfully with this spec and serve all defined endpoints.

- [ ] Step 1: Create `crates/specmock-runtime/tests/specs/openapi-prism-comparison.yaml` with:
  - `GET /pets` — list with `limit` (integer, optional), `offset` (integer, optional), `tags` (array of strings, required)
  - `GET /pets/{petId}` — single resource, `petId` is integer, responses: 200 (named examples: "fluffy", "whiskers"), 404, 500
  - `POST /pets` — create pet, required JSON body with `name` (string, minLength 1), `tag` (string, optional), response 201
  - `PUT /pets/{petId}` — update pet, required JSON body, response 200
  - `DELETE /pets/{petId}` — delete, response 204 (no body)
  - Multiple content types on GET /pets/{petId}: application/json, text/plain
  - Schema constraints: `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`, `format` (date-time, email, uuid)
  - Enum field (`status: [available, pending, sold]`)
  - Nested objects (`owner: { name, email }`)
  - `oneOf` with `discriminator` on `/shapes/{shapeId}` endpoint
- [ ] Step 2: Validate spec with `npx @stoplight/spectral-cli lint` or manual review
- [ ] Step 3: Start spec-mock with this spec and manually verify all endpoints return valid responses
- [ ] Step 4: Start Prism with this spec and manually verify all endpoints return valid responses
- [ ] Verification: Both servers start and serve all endpoints without errors.

### Task 4.2: Wire Up Full Comparison Test Suite

> **Context:** The main test file `prism_comparison.rs` orchestrates: (1) start both servers, (2) generate fuzz requests, (3) send to both, (4) compare responses. Uses a shared `OnceLock` or `tokio::sync::OnceCell` to start servers once for all tests. Each test function covers one request category.
> **Verification:** All comparison tests pass when run with `just integration-test` (assuming Prism is installed and both servers behave identically for the tested features).

- [ ] Step 1: Replace placeholder in `prism_comparison.rs` with full test infrastructure:
  - Shared server startup using `tokio::sync::OnceCell<(RunningServer, PrismServer)>`
  - Helper `setup_servers()` that starts spec-mock in-process and Prism as child process
  - Skip logic if Prism is not found
- [ ] Step 2: Implement `test_fuzz_valid_requests_match()`:
  - Generate valid fuzz requests
  - Send each to both servers
  - Compare all responses
  - Collect and report all diffs at the end
- [ ] Step 3: Implement `test_fuzz_invalid_path_returns_matching_status()`
- [ ] Step 4: Implement `test_fuzz_invalid_body_returns_matching_status()`
- [ ] Step 5: Implement `test_missing_required_param_returns_matching_status()`
- [ ] Step 6: Implement `test_wrong_content_type_returns_415_on_both()`
- [ ] Step 7: Implement `test_unknown_path_returns_matching_status()`
- [ ] Step 8: Implement `test_prefer_code_matches()`
- [ ] Step 9: Implement `test_prefer_example_matches()`
- [ ] Step 10: Implement `test_prefer_dynamic_structure_matches()`
- [ ] Step 11: Implement `test_content_negotiation_matches()`
- [ ] Verification: `just integration-test` runs all tests; report shows pass/fail per category with diff details.

### Task 4.3: Env Var Configuration and Seed Reproducibility

> **Context:** Fuzz test seed, iteration count, and Prism command are configurable via env vars. This enables CI to run with specific seeds for reproducibility and developers to override the Prism binary path.
> **Verification:** Running `SPECMOCK_FUZZ_SEED=123 just integration-test` produces different but reproducible fuzz requests. `SPECMOCK_PRISM_CMD=/usr/local/bin/prism just integration-test` uses the specified Prism binary.

- [ ] Step 1: Read `SPECMOCK_FUZZ_SEED` (default 42), `SPECMOCK_FUZZ_ITERATIONS` (default 10), `SPECMOCK_PRISM_CMD` env vars in test setup
- [ ] Step 2: Pass seed and iterations to `OpenApiFuzzer::new()`
- [ ] Step 3: Log seed and iteration count at test start for reproducibility
- [ ] Step 4: Verify same seed produces identical request sequence across runs
- [ ] Verification: Two runs with `SPECMOCK_FUZZ_SEED=42` produce identical test output.

---

## Phase 5: Polish & CI

### Task 5.1: Test Output and Error Reporting

> **Context:** When a comparison test fails, the output must clearly show: the request that was sent, the spec-mock response, the Prism response, and the specific structural diffs. This makes debugging divergences fast.
> **Verification:** Intentionally introduce a divergence (e.g., wrong status code) and verify the error output is actionable.

- [ ] Step 1: Implement `format_request(req: &FuzzRequest) -> String` for human-readable request dumps
- [ ] Step 2: Implement `format_response(resp: &CapturedResponse) -> String` for human-readable response dumps
- [ ] Step 3: Implement diff summary formatting in `ComparisonResult::report()` that combines request + both responses + diffs
- [ ] Step 4: Use `tracing` or `eprintln!` (allowed in test code) for clear test output with `--nocapture`
- [ ] Step 5: Verify output clarity by running a test that intentionally mismatches
- [ ] Verification: Failed test output clearly shows the request, both responses, and structured diffs.

### Task 5.2: Documentation and CI Notes

> **Context:** Document how to install Prism, run comparison tests, configure seeds, and interpret output. Update README.md with a section on comparison testing.
> **Verification:** A developer can follow the docs to set up and run comparison tests from scratch.

- [ ] Step 1: Add section to README.md under "Workspace Commands" explaining `just integration-test`
- [ ] Step 2: Document env vars (`SPECMOCK_FUZZ_SEED`, `SPECMOCK_FUZZ_ITERATIONS`, `SPECMOCK_PRISM_CMD`) in README
- [ ] Step 3: Add note about Prism installation: `npm install -g @stoplight/prism-cli`
- [ ] Step 4: Add CI pipeline snippet example (GitHub Actions) showing Prism installation + `just integration-test`
- [ ] Verification: README section renders correctly and instructions are complete.

---

## Definition of Done

- [ ] `just integration-test` runs the full comparison suite
- [ ] Tests cover: valid requests, invalid path params, invalid bodies, missing required params, wrong Content-Type, unknown paths, `Prefer: code`, `Prefer: example`, `Prefer: dynamic`, content negotiation
- [ ] Fuzz requests are deterministic and reproducible with seed
- [ ] Tests skip gracefully when Prism is not installed (no build failure)
- [ ] Test output clearly reports diffs when behavior diverges
- [ ] `just test` does NOT run comparison tests (feature gated)
- [ ] `just lint` passes with no new warnings
- [ ] README documents `just integration-test` usage
