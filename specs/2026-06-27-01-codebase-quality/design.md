# Design: Codebase Quality Improvements

| Metadata | Details |
| :--- | :--- |
| **Status** | Draft |
| **Created** | 2026-06-27 |
| **Mode** | Full |
| **Priority** | P1 |
| **Planned at** | commit `20a9e02`, 2026-06-27 |

## Summary

> 22 findings across correctness, security, performance, tech debt, and DX. The highest-leverage fixes are: (1) gRPC recursive message stack overflow, (2) SSRF via proxy upstream and callback URLs, (3) faker bugs (enum, caps, overflow), (4) content-type schema fallback, (5) proxy auth header leak. Security findings dominate the top of the list because they affect deployments beyond local testing.

## Why this matters

The SSRF findings (#2, #3) are exploitable in any deployment where the mock server is network-reachable. The gRPC recursion bug (#1) crashes the server on any recursive protobuf schema. The faker bugs (#4, #5, #6) produce invalid mock data that fails its own validation. The remaining findings are maintenance debt that compounds over time.

## Approach

Fix in priority order: security first (SSRF, header leak, path leak), then correctness (gRPC depth, faker, content-type, Prefer, callback, determinism, ws_url), then performance (validator cache, normalize_schema), then tech debt (deduplication, dead code), then DX (AGENTS.md, BDD). Each finding is a self-contained change with its own test.

## Findings

### Finding 1: gRPC recursive message stack overflow

- **Category:** bug
- **Impact:** HIGH — server panics on any recursive protobuf schema
- **Effort:** S

#### Requirements (EARS Notation)

- **[REQ-01]:** WHEN a protobuf message contains recursive fields (direct or indirect), THE runtime SHALL bound recursion depth to a configurable maximum (default 32).
- **[REQ-02]:** WHEN the depth limit is exceeded, THE runtime SHALL return an error instead of panicking.

#### Current state

- `crates/specmock-runtime/src/grpc/protobuf.rs:526-567` — `generate_dynamic_message` takes `(descriptor, seed)` with no depth parameter. Line 586 recurses unconditionally: `generate_dynamic_message(message_descriptor.clone(), seed + 1)`.

#### Approach

- Add `depth: usize` parameter to `generate_dynamic_message` and `scalar_value_for_field`.
- Pass `depth + 1` on recursive call at line 586.
- Return `Err` when `depth > 32`.

### Finding 2: SSRF via unvalidated upstream URL

- **Category:** security
- **Impact:** HIGH — proxy can reach internal services, metadata endpoints
- **Effort:** S

#### Requirements (EARS Notation)

- **[REQ-01]:** THE runtime SHALL reject upstream URLs with schemes other than `http` and `https`.
- **[REQ-02]:** THE runtime SHOULD provide a `--allow-private-upstream` flag to opt into RFC 1918/link-local targets.

#### Current state

- `crates/specmock-runtime/src/http/proxy.rs:26-31` — `target_url` built from `upstream.as_str()` with no scheme/host validation.
- `crates/specmock-runtime/src/http/mod.rs:86-94` — `url::Url::parse` accepts any scheme.

#### Approach

- Validate upstream URL scheme in `ServerConfig::validate()` or `HttpRuntime::from_config()`.
- Reject `file://`, `ftp://`, and other non-HTTP schemes.
- Optionally block private IP ranges (configurable).

### Finding 3: SSRF via callback URL from request body

- **Category:** security
- **Impact:** HIGH — attacker-controlled request body triggers outbound HTTP to arbitrary URLs
- **Effort:** S

#### Requirements (EARS Notation)

- **[REQ-01]:** THE runtime SHALL validate callback URLs before firing: scheme must be `http` or `https`.
- **[REQ-02]:** THE runtime SHOULD block callback URLs targeting private/link-local IP ranges.

#### Current state

- `crates/specmock-runtime/src/http/openapi.rs:620-639` — `resolve_callback_url` extracts URL from request body.
- `crates/specmock-runtime/src/http/mod.rs:244-258` — fires callback without URL validation.

#### Approach

- Add URL validation in `fire_callback` or before the `tokio::spawn` call.
- Check scheme (`http`/`https` only).
- Optionally resolve DNS and check IP ranges.

### Finding 4: Faker enum always picks first value

- **Category:** bug
- **Impact:** MED — no seed-based variation for enum schemas
- **Effort:** S

#### Current state

- `crates/specmock-core/src/faker.rs:70-73` — `enum_values.first()` ignores RNG.

#### Approach

- Use `rng.random_range(0..enum_values.len())` to select based on seed.

### Finding 5: Faker caps minItems/minLength violating schema

- **Category:** bug
- **Impact:** MED — generated data fails its own validation for constrained schemas
- **Effort:** S

#### Current state

- `crates/specmock-core/src/faker.rs:167` — `.min(3)` caps minItems.
- `crates/specmock-core/src/faker.rs:239` — `.min(64)` caps minLength.

#### Approach

- Remove the `.min(3)` cap on min_items (keep a reasonable absolute max like 100 to prevent DoS).
- Remove the `.min(64)` cap on min_length (keep a reasonable absolute max like 10000).

### Finding 6: Integer faker overflow near i64::MAX

- **Category:** bug
- **Impact:** LOW — panics in debug, wraps in release
- **Effort:** S

#### Current state

- `crates/specmock-core/src/faker.rs:192` — `min + 100` can overflow.

#### Approach

- Use `min.saturating_add(100)` and cap at `i64::MAX`.

### Finding 7: Content-type fallback picks wrong schema

- **Category:** bug
- **Impact:** MED — non-JSON schema used for validation and faker
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/openapi.rs:491` — `.or_else(|| content.values().find_map(Value::as_object).cloned())`.
- Same pattern at lines 526 and 597.

#### Approach

- Remove the `.or_else` fallback. Return `None` when `application/json` is not in the content map.

### Finding 8: select_response silently ignores missing Prefer:code

- **Category:** bug
- **Impact:** LOW — behavioral surprise, not data corruption
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/negotiate.rs:121-126` — falls through to 200/default/first when code not found.

#### Approach

- Return `None` when `prefer.code` is set and no matching response exists.

### Finding 9: Proxy forwards auth headers to upstream

- **Category:** security
- **Impact:** MED — credentials leak to upstream
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/proxy.rs:35-41` — forwards all headers except `host` and `content-length`.

#### Approach

- Strip `authorization`, `cookie`, `proxy-authorization` by default.

### Finding 10: Error responses leak internal file paths

- **Category:** security
- **Impact:** LOW — information disclosure
- **Effort:** S

#### Current state

- `crates/specmock-core/src/ref_resolver.rs:100-101,218-233` — error messages include `path.display()`.

#### Approach

- Sanitize error messages at the HTTP handler boundary in `crates/specmock-runtime/src/http/mod.rs`.
- Strip or redact filesystem paths from `ProblemDetails` before sending.

### Finding 11: resolve_callback_url fails on non-body tokens

- **Category:** bug
- **Impact:** MED — legitimate callbacks silently skipped
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/openapi.rs:631` — `token.strip_prefix("$request.body#")?` returns `None` for other tokens.

#### Approach

- Skip unrecognized tokens (emit as-is or log warning) instead of returning `None`.

### Finding 12: named_examples HashMap non-deterministic

- **Category:** bug
- **Impact:** LOW — non-reproducible mock output across restarts
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/openapi.rs:97` — `HashMap<String, Value>` for named_examples.
- Line 547 — `.values().next()` picks arbitrary first.

#### Approach

- Change `named_examples` from `HashMap` to `BTreeMap`.

### Finding 13: Triplicated hash function

- **Category:** tech debt
- **Impact:** LOW — maintenance debt
- **Effort:** S

#### Current state

- `crates/specmock-runtime/src/http/mod.rs:347-354` — `hash_path_and_method`.
- `crates/specmock-runtime/src/grpc/protobuf.rs:592-594` — `hash_path`.
- `crates/specmock-runtime/src/ws/asyncapi.rs:321-323` — `hash_channel`.

#### Approach

- Extract `fn deterministic_hash(seed: u64, input: &str) -> u64` into `specmock-core` or shared module.

### Finding 14: Duplicated JSON Pointer resolver

- **Category:** tech debt
- **Impact:** LOW — bug fixes must be applied twice
- **Effort:** S

#### Current state

- `crates/specmock-core/src/ref_resolver.rs:308-334` — `resolve_pointer` returns `Option<Value>` (cloning).
- `crates/specmock-runtime/src/http/openapi.rs:643-661` — `json_pointer` returns `Option<&Value>` (borrowing).

#### Approach

- Keep the borrowing version in `specmock-core::ref_resolver` as canonical.
- Have `openapi.rs` call it.

### Finding 15: Protocol/ValidationDirection dead code

- **Category:** tech debt
- **Impact:** LOW — false signal to contributors
- **Effort:** S

#### Current state

- `crates/specmock-core/src/contract.rs:6-25` — two enums, zero usage outside re-exports.

#### Approach

- Remove both enums and their re-exports from `lib.rs`.

### Finding 16: ResolvedDocument pointless newtype

- **Category:** tech debt
- **Impact:** LOW — unnecessary indirection
- **Effort:** S

#### Current state

- `crates/specmock-core/src/ref_resolver.rs:22-26` — `pub struct ResolvedDocument { pub root: Value }`.

#### Approach

- `resolve()` returns `Value` directly. Delete `ResolvedDocument`.

### Finding 17: MockServer::ws_url() hardcodes /ws

- **Category:** bug
- **Impact:** MED — wrong URL for non-default ws_path configs
- **Effort:** S

#### Current state

- `crates/specmock-sdk/src/server.rs:51` — `format!("ws://{}/ws", ...)`.
- Line 84 — same in `ProcessMockServer::ws_url()`.

#### Approach

- Store `ws_path` in `MockServer` / `ProcessMockServer` and use it in `ws_url()`.

### Finding 18: normalize_schema clones entire tree

- **Category:** performance
- **Impact:** LOW — startup-only cost
- **Effort:** M

#### Current state

- `crates/specmock-runtime/src/http/openapi.rs:663-713` — takes `Value` by value, clones children before recursing.

#### Approach

- Change signature to `fn normalize_schema(schema: &mut Value, ...)` and mutate in-place.

### Finding 19: No proxy mode integration tests

- **Category:** tests
- **Impact:** HIGH — untested production code path
- **Effort:** M

#### Current state

- `crates/specmock-runtime/src/http/proxy.rs` — zero tests.

#### Approach

- Add integration tests with a mock upstream (tokio TcpListener).
- Test: successful proxy, upstream returns invalid schema -> 502, upstream unreachable -> 502.

### Finding 20: AGENTS.md drift

- **Category:** dx
- **Impact:** MED — broken onboarding for agents
- **Effort:** M

#### Current state

- `AGENTS.md` — references `just bdd` and `just test-all` (don't exist), preferred versions diverge from Cargo.toml, lists unused dependencies.

#### Approach

- Remove phantom commands, update versions to match Cargo.toml, trim unused dependency guidance.

### Finding 21: Validator cache key serializes full schema JSON

- **Category:** performance
- **Impact:** MED — hot path performance
- **Effort:** S

#### Current state

- `crates/specmock-core/src/validate.rs:42` — `serde_json::to_string(schema)` per call.

#### Approach

- Hash the schema value and use the hash as cache key.

### Finding 22: No BDD feature files

- **Category:** tests
- **Impact:** MED — no acceptance-level behavioral specs
- **Effort:** L

#### Current state

- Zero `.feature` files in the repository.

#### Approach

- Create `features/` directory with Gherkin files for core behaviors.
- Implement `cucumber-rs` step definitions.
- Wire to existing server test harness.

## Code Simplification Constraints

**Ponytail Ladder (mandatory at every decision point):**

1. Does this need to exist at all? Speculative need = skip it. (YAGNI)
2. Stdlib does it? Use it.
3. Native platform feature covers it? Use it.
4. Already-installed dependency? Use it.
5. One line? One line.
6. Only then: minimum code that works.

**Mark deferrals:** Use `ponytail:` comments for deliberate simplifications with known ceilings.

**Never simplify away:** input validation, error handling, security, accessibility, anything explicitly requested.

**Additional constraints:**

- **Behavioral Contract:** Preserve existing behavior unless a listed scenario or requirement explicitly changes it.
- **Repo Standards:** Use only the coding standards established by `AGENTS.md` and the existing codebase.
- **Readability Priorities:** Prefer explicit control flow, clear names, reduced nesting.
- **Refactor Scope:** Limit cleanup to touched modules unless the design explicitly justifies broader refactor.

## BDD Scenario Inventory

- `features/correctness.feature` — Recursive protobuf does not crash: server stays alive → Task 1.1
- `features/correctness.feature` — Faker enum varies by seed: deterministic enum selection → Task 4.1
- `features/correctness.feature` — Faker respects minItems/minLength: generated data validates → Task 5.1
- `features/correctness.feature` — Integer faker no overflow: no panic near i64::MAX → Task 6.1
- `features/correctness.feature` — Non-JSON content type rejected: no wrong schema applied → Task 7.1
- `features/correctness.feature` — Prefer missing code returns error: no silent fallback → Task 8.1
- `features/correctness.feature` — Callback URL handles non-body tokens: graceful resolution → Task 11.1
- `features/correctness.feature` — Named examples deterministic: same seed same result → Task 12.1
- `features/correctness.feature` — SDK ws_url uses configured path: correct URL → Task 17.1
- `features/security.feature` — Proxy rejects non-HTTP upstream: no file:// SSRF → Task 2.1
- `features/security.feature` — Proxy rejects private upstream: no metadata SSRF → Task 2.2
- `features/security.feature` — Callback URL validated: no arbitrary outbound → Task 3.1
- `features/security.feature` — Proxy strips auth headers: no credential leak → Task 9.1
- `features/security.feature` — Error paths sanitized: no filesystem disclosure → Task 10.1
- `features/performance.feature` — Validator cache uses hashed keys: faster validation → Task 21.1
- `features/tech-debt.feature` — Hash function shared: single implementation → Task 13.1
- `features/tech-debt.feature` — JSON Pointer shared: single implementation → Task 14.1
- `features/tech-debt.feature` — Dead code removed: cleaner exports → Task 15.1
- `features/tech-debt.feature` — ResolvedDocument eliminated: simpler API → Task 16.1
- `features/dx.feature` — AGENTS.md matches project: correct guidance → Task 20.1
- `features/dx.feature` — BDD features exist: acceptance criteria → Task 22.1

## Verification

| Purpose   | Command                                      | Expected on success |
|-----------|----------------------------------------------|---------------------|
| Install   | `cargo build --workspace`                    | exit 0              |
| Lint      | `cargo +nightly clippy --all -- -D warnings` | exit 0, no errors   |
| Format    | `cargo +nightly fmt --all -- --check`        | exit 0              |
| Tests     | `cargo test --all-features`                  | all pass            |
| Full CI   | `just ci`                                    | all pass            |
