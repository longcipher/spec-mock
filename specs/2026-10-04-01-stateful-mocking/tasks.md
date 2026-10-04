# Stateful Mocking — Tasks

| Metadata | Details |
| :--- | :--- |
| **Design Doc** | specs/2026-10-04-01-stateful-mocking/design.md |
| **Status** | Not started |
| **Glossary** | GLOSSARY.md |
| **ADRs** | `docs/adr/0001`, `0002`, `0003` |

> **On the checkboxes in this file.** `specs/README.md` records that three prior specs closed
> their task lists while claiming work that was never done. Do not tick a box to mark work
> complete; tick it when `cargo test` covers the behaviour, and treat any `Status: Complete`
> in this repository as unverified until someone has diffed it against the tree.

## Summary & Timeline

| Phase | Tasks | Estimated Effort | Depends On |
| --- | --- | --- | --- |
| 1 — Core domain types | 1.1–1.3 | Medium | — |
| 2 — Sidecar loading & validation | 2.1–2.3 | Medium-Large | Phase 1 |
| 3 — Runtime precedence & store | 3.1–3.4 | Large | Phase 2 |
| 4 — Errors & bounds | 4.1–4.2 | Medium | Phase 3 |
| 5 — Observability, control, CLI | 5.1–5.3 | Medium | Phase 3 |
| 6 — SDK | 6.1–6.2 | Small-Medium | Phases 3, 5 |
| 7 — Documentation & fixtures | 7.1–7.3 | Small | Phase 5 |
| 8 — Verification | 8.1–8.3 | Medium | All |

---

## Phase 1: Core domain types

All types live in `specmock-core` and must not reference `http`, `axum`, or any protocol type.
The core crate already has zero protocol dependencies and owns the shared validation model, so
this follows the existing layering rather than introducing a new boundary — which is also why it
did not warrant an ADR.

### Task 1.1: Scenario, state, and transition model

- **Status:** ⬜ NOT STARTED
- [ ] Add `State`, `Transition`, `Scenario`, `ScenarioInstance` in `specmock-core/src/scenario.rs`.
- [ ] `Transition` carries `target: Option<State>` and an optional `responses` override.
- [ ] Absent `target` means "legal here, do not move" (a resting state, not an error).
- [ ] `Scenario` holds `name`, `initial_state`, and a state → allowed-operations map.
- [ ] Lookups returning a copy of `State` (cheap, string-backed) rather than a reference.
- [ ] Unit tests: legal/illegal operation lookup, unknown state returns `None`.

### Task 1.2: Store and key namespacing

- **Status:** ⬜ NOT STARTED
- [ ] Add `Resource`, `StoreAction` (Create/Read/Update/Delete/List), `Store` in
      `specmock-core/src/store.rs`.
- [ ] `Store` backed by `scc::HashMap<(StoreKey, String), Resource>` — lock-free per `AGENTS.md`.
- [ ] `StoreKey` is the collection path template with parameters bound; preserve insertion
      order per namespace for `list` (a second `Vec` per namespace, or an ordered index).
- [ ] `create` returns `Err` on key collision; `read`/`update`/`delete`/`list` on absent resource.
- [ ] `delete` on an absent resource errors rather than succeeding.
- [ ] Document that read-modify-write on one key is not atomic, and that this is accepted.
- [ ] Unit tests: CRUD matrix, key namespacing isolation, creation-order stability.

### Task 1.3: Seeded and pre-seeded resource construction

- **Status:** ⬜ NOT STARTED
- [ ] Resources are generated through the existing faker so they are schema-valid by
      construction; no separate validation path.
- [ ] Pre-seeded items are inserted through the same path as created ones.
- [ ] Property test: every stored resource validates against its operation's schema.

## Phase 2: Sidecar loading & validation

### Task 2.1: Sidecar schema and parser

- **Status:** ⬜ NOT STARTED
- [ ] Create `specmock-core/src/sidecar.rs` with the serde model for `version`, `operations`,
      `scenarios`, `seed`, and `store` blocks.
- [ ] `version` is required; unknown values are a hard startup error naming supported versions.
- [ ] Accept YAML and JSON, matching how specs are already loaded (`serde_yml` / `serde_json`).
- [ ] Resolve the sidecar through the existing `RefResolver` so `$ref` works inside it and into
      the spec, with the same depth limit and allowed-root sandboxing.
- [ ] Shorthands: `store: read` infers `resource` and `key` from the path template.
- [ ] Unit tests: both formats, `$ref` into the spec, shorthand expansion, missing `version`.

### Task 2.2: Referenced operations and the reserved prefix

- **Status:** ⬜ NOT STARTED
- [ ] Resolve each operation's `match` (method + path template) against the loaded
      `OperationSpec` list; unknown match is a hard startup error.
- [ ] Detect path collisions between the reserved `/_specmock/` prefix and declared operations.
- [ ] Reject scenarios declaring an operation beneath `/_specmock/`.
- [ ] Unit tests: unknown operation, colliding control path, nested `$ref`'d operation.

### Task 2.3: FSM legality validation

- **Status:** ⬜ NOT STARTED
- [ ] Hard error: `initialState` names an undeclared state.
- [ ] Hard error: a transition targets an undeclared state.
- [ ] Hard error: a state references an operation absent from the catalogue.
- [ ] Warning only: an unreachable state.
- [ ] Warning: an operation in the catalogue referenced by no scenario (likely dead config).
- [ ] Reject a sidecar supplied together with `mode: proxy`, naming both settings.
- [ ] Unit tests for each hard error and each warning.

## Phase 3: Runtime precedence & store

### Task 3.1: `Prefer` directive extensions

- **Status:** ⬜ NOT STARTED
- [ ] Add `scenario: Option<String>` and `session: Option<String>` to `PreferDirectives`.
- [ ] Both bypass directives (`code=`, `dynamic=true`) are documented as read-only escape
      hatches that do **not** advance state.
- [ ] `dynamic=true` bypasses the store and generates fresh.
- [ ] Unit tests: parsing, repeated `Prefer` headers, empty values, case handling.

### Task 3.2: Session resolution

- **Status:** ⬜ NOT STARTED
- [ ] Absent `session`: resolve to a single default instance.
- [ ] Absent `scenario`: resolve against the default scenario, or fail if none is declared.
- [ ] An unseen `session` key creates its instance implicitly, matching the singleton default.
- [ ] Unknown `scenario` name returns `404` naming the scenarios that exist.
- [ ] Hold instances in an `scc::HashMap` guarded by the session cap (Task 4.2).
- [ ] Unit tests: default, keyed isolation, implicit creation, unknown scenario.

### Task 3.3: Response precedence chain

- **Status:** ⬜ NOT STARTED
- [ ] Implement the five-step order from the design: `Prefer: code=` → `dynamic=true` →
      scenario override → operation catalogue → existing default selection.
- [ ] A scenario is an additive overlay: an operation absent from the named scenario falls
      through, causes no transition, and leaves state unchanged.
- [ ] A store-backed read supplies the body directly, bypassing example/faker selection.
- [ ] Integration tests asserting each step wins over the ones after it.

### Task 3.4: Transition ordering

- **Status:** ⬜ NOT STARTED
- [ ] Validate the request **before** advancing state or touching the store.
- [ ] A rejected request leaves both untouched.
- [ ] A store `404` on a valid request **does** advance state.
- [ ] Transitions are repeatable; a second firing requires a declared self-loop.
- [ ] Transitions log at INFO via `tracing`: scenario, session, from, to, operation.
- [ ] Property test: the state after any request is always a declared state.
- [ ] Integration test: an invalid request followed by a valid one sees unchanged state.

## Phase 4: Errors & bounds

### Task 4.1: Stateful error vocabulary

- **Status:** ⬜ NOT STARTED
- [ ] Do **not** encode these as `ValidationIssue`. Add RFC 7807 extension members to
      `ProblemDetails` carrying `scenario`, `session`, `state`, and `operation`.
- [ ] `409` for an operation not legal in the current state, citing RFC 9110.
- [ ] `404` for unknown resource and unknown scenario; `409` for create key collision.
- [ ] `errors` stays empty on these responses.
- [ ] Unit tests asserting status, `detail`, extension members, and empty `errors`.

### Task 4.2: Bounds

- **Status:** ⬜ NOT STARTED
- [ ] `--max-store-entries` per instance → `500` naming the knob.
- [ ] `--max-sessions` → `429` naming the knob.
- [ ] **No LRU eviction anywhere.** Both caps report loudly; neither silently drops.
- [ ] Defaults are generous enough that ordinary use never hits them.
- [ ] Tests for both caps and for the absence of eviction below the threshold.

## Phase 5: Observability, control, CLI

### Task 5.1: Control endpoints

- **Status:** ⬜ NOT STARTED
- [ ] `GET /_specmock/state` and `POST /_specmock/reset` as explicit axum routes, registered
      before `app.fallback(...)`.
- [ ] Optional bearer token, minted per run, defaulting to on, logged at startup.
- [ ] Reset returns the instance to its initial state **and** empties the store; the two are
      not separable operations.
- [ ] Tests: token required when enabled, absent when disabled, reset clears state and store.

### Task 5.2: CLI flags

- **Status:** ⬜ NOT STARTED
- [ ] Add `--log-level`. **This does not exist today** and is required for the INFO transition
      log to be reachable at all.
- [ ] Add `--scenarios`, `--max-store-entries`, `--max-sessions`.
- [ ] Forward all of them in SDK process mode, or process parity breaks.

### Task 5.3: `Location` override on create

- **Status:** ⬜ NOT STARTED
- [ ] When a store-backed create succeeds, populate the declared `Location` response header from
      the created resource, overriding the faker value.
- [ ] Record separately that the un-overridden faker `Location` is a pre-existing defect for
      any spec declaring it, independent of scenarios.

## Phase 6: SDK

### Task 6.1: Embedded surface

- **Status:** ⬜ NOT STARTED
- [ ] `MockServer::reset_scenarios()` and `MockServer::scenario_state(session)`.
- [ ] `MockServerBuilder` gains `scenarios`, `max_store_entries`, `max_sessions`.
- [ ] Ensure `Drop` still shuts down cleanly with live sessions.

### Task 6.2: Process-mode parity

- **Status:** ⬜ NOT STARTED
- [ ] Process mode reaches the control endpoints over HTTP, using the minted token.
- [ ] Extend `tests/sdk_process.rs` to assert identical responses **and identical reset
      behaviour** between embedded and process modes. That file already asserts response
      parity; reset must not be the first thing to break it.

## Phase 7: Documentation & fixtures

### Task 7.1: Example specs

- **Status:** ⬜ NOT STARTED
- [ ] Add `docs/specs/users.openapi.yaml` exercising CRUD + pre-seed + a state-specific error.
- [ ] Add `docs/specs/users.scenarios.yaml` as the worked example referenced by the README.

### Task 7.2: README

- **Status:** ⬜ NOT STARTED
- [ ] Scenario file reference, the five-step precedence chain, and the store semantics table.
- [ ] Restate determinism as sequence-based (ADR 0003), including that state does not survive
      restart.
- [ ] Document the documented limits: no collection filtering or pagination, no atomic
      read-modify-write, no WS/gRPC scenarios.

### Task 7.3: Corrections owed from the audit

- **Status:** ⬜ NOT STARTED
- [ ] Remove the `style`/`explode` claim at `README.md:61` — never implemented.
- [ ] Correct the "Uses tonic for standard HTTP/2 transport" claim at `README.md:208` — the
      transport is `hyper_util::server::conn::auto`; `tonic` is only the `Status` error type.
- [ ] Add a `stateful` row to the Prism comparison table.
- [ ] Update `specs/context.md:55`'s test count, which is stale by roughly 3x.

## Phase 8: Verification

### Task 8.1: Regression gate

- **Status:** ⬜ NOT STARTED
- [ ] `just format`, `just lint`, `just test` pass with no warnings.
- [ ] All 730 existing tests still pass. **Absent a sidecar, behaviour is byte-identical** —
      this is the single most important check in the phase.

### Task 8.2: Test coverage

- **Status:** ⬜ NOT STARTED
- [ ] Integration: full CRUD flow, pre-seeded read, session isolation under the existing
      100-way concurrency test, every error status, both caps, control endpoint, and each
      `Prefer` bypass not advancing state.

### Task 8.3: Manual smoke test

- **Status:** ⬜ NOT STARTED
- [ ] Serve `docs/specs/users.openapi.yaml` with its sidecar and walk the flow with `curl`,
      confirming transitions in the log at `--log-level info`.
- [ ] Confirm the `429`/`500` cap responses name their knob.
