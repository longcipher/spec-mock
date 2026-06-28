# Tasks: Codebase Quality Improvements

Generated from design.md at commit `20a9e02` on 2026-06-27.

## Phase 1: Security (HIGH priority)

### Task 2.1: Reject non-HTTP upstream URL schemes

> **Context:** SSRF via `file://` and other non-HTTP upstream URLs (Finding 2).
> **Verification:** `cargo test --all-features` passes; server rejects `file://` upstream at config time.
> **Scenario Coverage:** `features/security.feature` — Proxy rejects non-HTTP upstream

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Server must reject non-http/https upstream schemes at config validation time.
- **Simplification Focus:** Add scheme check in existing `ServerConfig::validate()`. One match arm.
- **Status:** 🟢 DONE
- [x] Step 1: Add test in `crates/specmock-runtime/src/lib.rs::tests` that creates `ServerConfig` with `upstream: Some("file:///etc/passwd".into())`, calls `validate()`, asserts error.
- [x] Step 2: In `ServerConfig::validate()`, after proxy/upstream check, parse upstream URL and reject if scheme is not `http` or `https`.
- [x] Step 3: Run `cargo test --all-features` — new test passes.
- [x] BDD Verification: N/A — unit test covers the scenario.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: `cargo run -p spec-mock -- serve --openapi docs/specs/pets.openapi.yaml --mode proxy --upstream file:///etc/passwd` exits with error.

### Task 2.2: Block private/link-local upstream by default

> **Context:** SSRF via metadata endpoints like `http://169.254.169.254/` (Finding 2).
> **Verification:** `cargo test --all-features` passes; proxy rejects private upstream by default.
> **Scenario Coverage:** `features/security.feature` — Proxy rejects private upstream

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Proxy mode rejects RFC 1918, link-local, and loopback upstream URLs unless `--allow-private-upstream` is set.
- **Simplification Focus:** Check resolved IP after DNS lookup in `proxy_request`, or check URL host at config time. Use `std::net::IpAddr::is_private()` or similar.
- **Status:** 🟢 DONE
- [x] Step 1: Add test that configures upstream to `http://127.0.0.1:9999` without `--allow-private-upstream`, assert validation fails.
- [x] Step 2: Add `allow_private_upstream: bool` to `ServerConfig` (default `false`). In `validate()`, resolve upstream host and check if it's private/loopback/link-local.
- [x] Step 3: Add `--allow-private-upstream` CLI flag in `bin/spec-mock/src/main.rs`.
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: `cargo run -p spec-mock -- serve --openapi docs/specs/pets.openapi.yaml --mode proxy --upstream http://127.0.0.1:8080` exits with error; adding `--allow-private-upstream` succeeds.

### Task 3.1: Validate callback URLs before firing

> **Context:** SSRF via callback URL extracted from untrusted request body (Finding 3).
> **Verification:** `cargo test --all-features` passes; callback with non-HTTP URL is not fired.
> **Scenario Coverage:** `features/security.feature` — Callback URL validated

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Callback URLs must be `http` or `https` scheme. Private IP callbacks are blocked unless `allow_private_upstream` is set.
- **Simplification Focus:** Add URL validation in `fire_callback` or before the `tokio::spawn`. One check.
- **Status:** 🟢 DONE
- [x] Step 1: Add test in `crates/specmock-runtime/src/http/mod.rs::tests` that verifies `fire_callback` with a `file://` URL does not send a request.
- [x] Step 2: In `http_fallback_handler` before spawning callback task, parse the resolved URL with `url::Url`, check scheme is `http`/`https`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — behavior is fire-and-forget, verified by test.

### Task 9.1: Strip sensitive headers in proxy mode

> **Context:** Proxy forwards `Authorization`, `Cookie` to upstream (Finding 9).
> **Verification:** `cargo test --all-features` passes; auth headers are stripped.
> **Scenario Coverage:** `features/security.feature` — Proxy strips auth headers

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `authorization`, `cookie`, `proxy-authorization` headers are stripped from proxied requests by default.
- **Simplification Focus:** Add 3 more entries to the existing skip list in `proxy_request`. Three lines.
- **Status:** 🟢 DONE
- [x] Step 1: Add test in `crates/specmock-runtime/tests/http_openapi.rs` that sends a request with `Authorization: Bearer token` in proxy mode, asserts the upstream does not receive it.
- [x] Step 2: In `crates/specmock-runtime/src/http/proxy.rs:37`, add `authorization`, `cookie`, `proxy-authorization` to the skip list.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — integration test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — verified by test.

### Task 10.1: Sanitize error messages to remove file paths

> **Context:** Error responses leak internal filesystem paths (Finding 10).
> **Verification:** `cargo test --all-features` passes; error responses contain no absolute paths.
> **Scenario Coverage:** `features/security.feature` — Error paths sanitized

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** HTTP error responses must not contain absolute filesystem paths.
- **Simplification Focus:** Add a sanitizer function that strips path prefixes from error messages at the HTTP boundary. One function.
- **Status:** 🟢 DONE
- [x] Step 1: Add test that triggers a spec load error and checks the response body does not contain `/` (absolute path indicator).
- [x] Step 2: In `http_fallback_handler` or `problem_response`, sanitize `detail` and `message` fields to redact paths.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — verified by test.

## Phase 2: Correctness bugs (HIGH/MED priority)

### Task 1.1: Add depth limit to gRPC message generator

> **Context:** Recursive protobuf messages cause stack overflow (Finding 1).
> **Verification:** `cargo test --all-features` passes; recursive proto schema doesn't crash.
> **Scenario Coverage:** `features/correctness.feature` — Recursive protobuf does not crash

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `generate_dynamic_message` bounds recursion at depth 32.
- **Simplification Focus:** Add `depth: usize` parameter, one check at top of function.
- **Status:** 🟢 DONE
- [x] Step 1: Add `depth: usize` parameter to `generate_dynamic_message` and `scalar_value_for_field`.
- [x] Step 2: At the top of `generate_dynamic_message`, if `depth > 32`, return `Err("maximum protobuf message recursion depth reached")`.
- [x] Step 3: Pass `depth + 1` on recursive call at line 586.
- [x] Step 4: Update all call sites to pass `depth: 0` initially.
- [x] Step 5: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — the fix prevents a panic, verified by compilation and existing tests.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — recursive protos are an edge case.

### Task 4.1: Fix faker enum to use seed-based selection

> **Context:** Faker always picks first enum value (Finding 4).
> **Verification:** `cargo test --all-features` passes; different seeds can produce different enum values.
> **Scenario Coverage:** `features/correctness.feature` — Faker enum varies by seed

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `generate_with_rng` selects enum values using the RNG.
- **Simplification Focus:** One line change: `rng.random_range(0..enum_values.len())`.
- **Status:** 🟢 DONE
- [x] Step 1: Add test in `crates/specmock-core/src/faker.rs::tests` that generates with two different seeds from `{"enum": ["A", "B", "C"]}` and asserts the results can differ.
- [x] Step 2: Change line 71 from `enum_values.first()` to `enum_values[rng.random_range(0..enum_values.len())]`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — deterministic behavior verified by test.

### Task 5.1: Remove faker caps on minItems and minLength

> **Context:** Faker caps break constrained schemas (Finding 5).
> **Verification:** `cargo test --all-features` passes; `minItems: 10` produces 10+ items.
> **Scenario Coverage:** `features/correctness.feature` — Faker respects minItems/minLength

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Faker respects schema constraints up to a reasonable DoS-prevention limit.
- **Simplification Focus:** Replace `.min(3)` with `.min(100)` (absolute max), `.min(64)` with `.min(10000)`.
- **Status:** 🟢 DONE
- [x] Step 1: Add test that generates from `{"type": "array", "minItems": 10, "items": {"type": "string"}}` and asserts length >= 10.
- [x] Step 2: Add test that generates from `{"type": "string", "minLength": 200}` and asserts length >= 200.
- [x] Step 3: Change `.min(3)` to `.min(100)` at `faker.rs:167`. Change `.min(64)` to `.min(10000)` at `faker.rs:239`.
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — faker behavior verified by test.

### Task 6.1: Fix integer faker overflow

> **Context:** `min + 100` wraps near i64::MAX (Finding 6).
> **Verification:** `cargo test --all-features` passes; no overflow with extreme minimum.
> **Scenario Coverage:** `features/correctness.feature` — Integer faker no overflow

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Integer generation never panics or wraps.
- **Simplification Focus:** `min.saturating_add(100)` — one line.
- **Status:** 🟢 DONE
- [x] Step 1: Add test with `{"type": "integer", "minimum": 9223372036854775707}`, assert it doesn't panic.
- [x] Step 2: Change `min + 100` to `min.saturating_add(100)` at `faker.rs:192`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — edge case verified by test.

### Task 7.1: Remove content-type fallback to non-JSON schema

> **Context:** Wrong schema applied when `application/json` is missing (Finding 7).
> **Verification:** `cargo test --all-features` passes; operations without JSON content type get no schema.
> **Scenario Coverage:** `features/correctness.feature` — Non-JSON content type rejected

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Only `application/json` content type is used for schema extraction.
- **Simplification Focus:** Remove `.or_else(...)` at 3 locations. Three deletions.
- **Status:** 🟢 DONE
- [x] Step 1: Add test with OpenAPI operation declaring only `application/xml`, assert `request_body_schema` is `None`.
- [x] Step 2: Remove `.or_else(|| content.values().find_map(Value::as_object).cloned())` at `openapi.rs:491`, `526`, `597`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — parsing behavior verified by test.

### Task 8.1: Return None when Prefer:code target is missing

> **Context:** Silent fallback to 200 when code not found (Finding 8).
> **Verification:** `cargo test --all-features` passes; missing code returns None.
> **Scenario Coverage:** `features/correctness.feature` — Prefer missing code returns error

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `select_response` returns `None` when `prefer.code` is set but no matching response exists.
- **Simplification Focus:** Return `None` instead of falling through. One `return None`.
- **Status:** 🟢 DONE
- [x] Step 1: Update existing test `select_response_falls_back_when_code_missing` to expect `None` instead of `Some("200")`.
- [x] Step 2: In `select_response`, after the `find` for code, add `return None` when not found.
- [x] Step 3: Handle `None` in caller (`mock_response`) — return 404 problem+json.
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: `curl -H "Prefer: code=418" http://127.0.0.1:4010/pets/1` returns 404 problem+json when 418 is not in spec.

### Task 11.1: Handle non-body tokens in callback URL resolver

> **Context:** Callbacks with `{$url}` or `{$method}` silently fail (Finding 11).
> **Verification:** `cargo test --all-features` passes; non-body tokens are handled gracefully.
> **Scenario Coverage:** `features/correctness.feature` — Callback URL handles non-body tokens

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `resolve_callback_url` skips unrecognized `{...}` tokens instead of returning `None`.
- **Simplification Focus:** Change `?` to a skip-or-emit pattern. Small match block.
- **Status:** 🟢 DONE
- [x] Step 1: Add test with expression `{$request.body#/url}/{$method}`, assert partial resolution.
- [x] Step 2: In `resolve_callback_url`, when `strip_prefix` returns `None`, emit the token as-is (literal `{token}`) instead of returning `None`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — callback behavior verified by test.

### Task 12.1: Make named_examples selection deterministic

> **Context:** HashMap iteration order is non-deterministic (Finding 12).
> **Verification:** `cargo test --all-features` passes; same example selected across runs.
> **Scenario Coverage:** `features/correctness.feature` — Named examples deterministic

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Named examples use sorted-key selection.
- **Simplification Focus:** Change `HashMap` to `BTreeMap` for `named_examples`. One type change.
- **Status:** 🟢 DONE
- [x] Step 1: In `openapi.rs`, change `named_examples: HashMap<String, Value>` to `BTreeMap<String, Value>` in `ResponseSpec`.
- [x] Step 2: Update imports and all construction sites.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — type change ensures determinism.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — determinism verified by type.

### Task 17.1: Fix SDK ws_url to use configured path

> **Context:** `ws_url()` hardcodes `/ws` (Finding 17).
> **Verification:** `cargo test --all-features` passes; ws_url reflects configured path.
> **Scenario Coverage:** `features/correctness.feature` — SDK ws_url uses configured path

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `ws_url()` uses the configured `ws_path` from `ServerConfig`.
- **Simplification Focus:** Store `ws_path` in `MockServer`/`ProcessMockServer`. Two field additions.
- **Status:** 🟢 DONE
- [x] Step 1: Store `ws_path` from `RunningServer` in `MockServer`. Store from config in `ProcessMockServer`.
- [x] Step 2: Update `ws_url()` to use stored path instead of hardcoded `/ws`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — unit test covers.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — URL format verified by test.

## Phase 3: Performance

### Task 21.1: Use hashed keys for validator cache

> **Context:** Full JSON serialization per validate call (Finding 21).
> **Verification:** `cargo test --all-features` passes; cache uses hash keys.
> **Scenario Coverage:** `features/performance.feature` — Validator cache uses hashed keys

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Validator cache keys are hashes, not full JSON strings.
- **Simplification Focus:** Replace `serde_json::to_string` with a hash. Use `std::hash::DefaultHasher`.
- **Status:** 🟢 DONE
- [x] Step 1: In `validate.rs`, change `get_or_compile_validator` to hash the schema value instead of serializing it.
- [x] Step 2: Use `std::hash::{Hash, Hasher, DefaultHasher}` and hash the `serde_json::Value` tree.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — performance improvement, existing tests verify correctness.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — performance characteristic.

### Task 18.1: Make normalize_schema mutate in-place

> **Context:** Clones entire schema tree on every recursive call (Finding 18).
> **Verification:** `cargo test --all-features` passes; no `.clone()` in normalize_schema.
> **Scenario Coverage:** `features/performance.feature` — Schema normalization in-place

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `normalize_schema` takes `&mut Value` and mutates in-place.
- **Simplification Focus:** Change signature, replace clone-and-assign with direct mutation.
- **Status:** 🟢 DONE
- [x] Step 1: Change `normalize_schema` signature to `fn normalize_schema(schema: &mut Value, use_nullable_transform: bool)`.
- [x] Step 2: Replace all `let normalized = normalize_schema(value.clone(), ...); *value = normalized;` with `normalize_schema(value, ...)`.
- [x] Step 3: Update all call sites to pass `&mut`.
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — refactoring, existing tests verify.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — startup-only change.

## Phase 4: Tech Debt

### Task 13.1: Extract shared hash function

> **Context:** Three copies of the same fold-hash (Finding 13).
> **Verification:** `cargo test --all-features` passes; single hash function used everywhere.
> **Scenario Coverage:** `features/tech-debt.feature` — Hash function shared

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** All three modules call the same function.
- **Simplification Focus:** One function in a shared module, three call-site updates.
- **Status:** 🟢 DONE
- [x] Step 1: Add `pub fn deterministic_hash(seed: u64, input: &str) -> u64` to `specmock-runtime/src/lib.rs` or a shared module.
- [x] Step 2: Replace `hash_path_and_method` in `http/mod.rs`, `hash_path` in `grpc/protobuf.rs`, `hash_channel` in `ws/asyncapi.rs`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — refactoring.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — internal refactoring.

### Task 14.1: Unify JSON Pointer resolver

> **Context:** Two implementations of RFC 6901 pointer resolution (Finding 14).
> **Verification:** `cargo test --all-features` passes; single implementation used.
> **Scenario Coverage:** `features/tech-debt.feature` — JSON Pointer shared

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `openapi.rs` calls `specmock_core::ref_resolver::resolve_pointer`.
- **Simplification Focus:** Export existing function, delete duplicate.
- **Status:** 🟢 DONE
- [x] Step 1: Make `resolve_pointer` in `ref_resolver.rs` public (change return to `Option<&Value>` for borrow semantics).
- [x] Step 2: In `openapi.rs`, remove `json_pointer` function and import `resolve_pointer` from `specmock_core`.
- [x] Step 3: Update callers in `resolve_callback_url`.
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — refactoring.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — internal refactoring.

### Task 15.1: Remove dead code enums

> **Context:** `Protocol` and `ValidationDirection` are unused (Finding 15).
> **Verification:** `cargo test --all-features` passes; no compilation errors.
> **Scenario Coverage:** `features/tech-debt.feature` — Dead code removed

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Removing these types breaks no code.
- **Simplification Focus:** Delete the enums and their re-exports. Two deletions.
- **Status:** 🟢 DONE
- [x] Step 1: Remove `Protocol` and `ValidationDirection` from `crates/specmock-core/src/contract.rs`.
- [x] Step 2: Remove their re-exports from `crates/specmock-core/src/lib.rs`.
- [x] Step 3: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — deletion.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — dead code removal.

### Task 16.1: Eliminate ResolvedDocument wrapper

> **Context:** Pointless newtype with no methods (Finding 16).
> **Verification:** `cargo test --all-features` passes; `resolve()` returns `Value`.
> **Scenario Coverage:** `features/tech-debt.feature` — ResolvedDocument eliminated

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** `RefResolver::resolve()` returns `Value` directly.
- **Simplification Focus:** Delete struct, change return type, update callers.
- **Status:** 🟢 DONE
- [x] Step 1: Change `resolve()` return type from `ResolvedDocument` to `Value`.
- [x] Step 2: Delete `ResolvedDocument` struct.
- [x] Step 3: Update callers (`openapi.rs:119`, `asyncapi.rs:54`, `protobuf.rs`).
- [x] Step 4: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — refactoring.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — internal refactoring.

## Phase 5: DX & Tests

### Task 20.1: Fix AGENTS.md to match actual project

> **Context:** AGENTS.md has phantom commands, wrong versions, unused deps (Finding 20).
> **Verification:** All referenced just commands exist; versions match Cargo.toml.
> **Scenario Coverage:** `features/dx.feature` — AGENTS.md matches project

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** AGENTS.md is accurate and actionable.
- **Simplification Focus:** Edit existing file. Remove phantom commands, update versions.
- **Status:** 🟢 DONE
- [x] Step 1: Remove `just bdd` and `just test-all` references from AGENTS.md.
- [x] Step 2: Update preferred dependency versions to match workspace `Cargo.toml`.
- [x] Step 3: Remove references to unused dependencies (`sqlx`, `utoipa`, `arc-swap`, etc.).
- [x] Step 4: Run `just lint` to verify markdown is clean.
- [x] BDD Verification: N/A — documentation.
- [x] Advanced Test Verification: `rumdl check .`
- [x] Runtime Verification: N/A — documentation.

### Task 19.1: Add proxy mode integration tests

> **Context:** Zero tests for proxy mode (Finding 19).
> **Verification:** New proxy integration tests pass.
> **Scenario Coverage:** N/A — test coverage gap, not a behavioral change

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Proxy mode is tested for: success, upstream error, schema validation failure.
- **Simplification Focus:** Add tests in existing `http_openapi.rs` using a mock upstream.
- **Status:** 🟢 DONE
- [x] Step 1: Add test helper that starts a tokio TcpListener as mock upstream.
- [x] Step 2: Add test: proxy forwards request and returns upstream response.
- [x] Step 3: Add test: upstream returns invalid schema, proxy returns 502.
- [x] Step 4: Add test: upstream unreachable, proxy returns 502.
- [x] Step 5: Run `cargo test --all-features`.
- [x] BDD Verification: N/A — test file.
- [x] Advanced Test Verification: `cargo +nightly clippy --all -- -D warnings`
- [x] Runtime Verification: N/A — tests.

### Task 22.1: Create BDD feature files for core behaviors

> **Context:** No `.feature` files exist (Finding 22).
> **Verification:** Feature files exist in `features/` covering core behaviors.
> **Scenario Coverage:** `features/dx.feature` — BDD features exist

- **Loop Type:** `TDD-only`
- **Behavioral Contract:** Gherkin files cover: mock response generation, request validation, content negotiation.
- **Simplification Focus:** The feature files written as part of this spec ARE the deliverable. Copy from `specs/.../features/` to `features/`.
- **Status:** 🟢 DONE
- [x] Step 1: Copy feature files from `specs/2026-06-27-01-codebase-quality/features/` to `features/` at repo root.
- [x] Step 2: Verify `features/` directory contains `.feature` files.
- [x] Step 3: Run `just lint` to verify markdown is clean.
- [x] BDD Verification: N/A — this creates the BDD files.
- [x] Advanced Test Verification: N/A
- [x] Runtime Verification: N/A
