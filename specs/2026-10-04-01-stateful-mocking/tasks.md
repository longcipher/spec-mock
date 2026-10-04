# Stateful Mocking — Tasks

| Metadata | Details |
| :--- | :--- |
| **Design Doc** | specs/2026-10-04-01-stateful-mocking/design.md |
| **Status** | Shipped across HTTP, WebSocket, and gRPC |
| **Glossary** | GLOSSARY.md |
| **ADRs** | `docs/adr/0001`, `0002`, `0003` |

> **On the checkboxes in this file.** `specs/README.md` records that three prior specs closed
> their task lists while claiming work that was never done. Boxes below are ticked where a
> named test covers the behaviour, and left open where it does not. Everything is now covered
> except `--max-store-entries`, which has no test of its own -- the session cap does, and both
> share one code path through `StoreError::CapacityExceeded`.

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

- **Status:** 🟢 DONE
- [x] Add `State`, `Transition`, `Scenario`, `ScenarioInstance` in `specmock-core/src/scenario.rs`.
- [x] `Transition` carries `target: Option<State>` and an optional `responses` override.
- [x] Absent `target` means "legal here, do not move" (a resting state, not an error).
- [x] `Scenario` holds `name`, `initial_state`, and a state → allowed-operations map.
- [x] Lookups returning a copy of `State` (cheap, string-backed) rather than a reference.
- [x] Unit tests: legal/illegal operation lookup, unknown state returns `None`.

### Task 1.2: Store and key namespacing

- **Status:** 🟢 DONE
- [x] Add `Resource`, `StoreAction` (Create/Read/Update/Delete/List), `Store` in
      `specmock-core/src/store.rs`.
- [x] `Store` backed by `scc::HashMap<(StoreKey, String), Resource>` — lock-free per `AGENTS.md`.
- [x] `StoreKey` is the collection path template with parameters bound; preserve insertion
      order per namespace for `list` (a second `Vec` per namespace, or an ordered index).
- [x] `create` returns `Err` on key collision; `read`/`update`/`delete`/`list` on absent resource.
- [x] `delete` on an absent resource errors rather than succeeding.
- [x] Document that read-modify-write on one key is not atomic, and that this is accepted.
- [x] Unit tests: CRUD matrix, key namespacing isolation, creation-order stability.

### Task 1.3: Seeded and pre-seeded resource construction

- **Status:** 🟢 DONE
- [x] Resources are generated through the existing faker so they are schema-valid by
      construction; no separate validation path.
- [x] Pre-seeded items are inserted through the same path as created ones.
- [x] A resource read back out of the store is byte-identical to the one written, across
      identifiers that stress the key handling (`a`, `b-2`, `3`, `with.dots`, `UPPER`,
      `%20escaped`). Resources originate either from the request body, already validated
      against the spec, or from the faker, so the invariant is really that the store does not
      mangle what it holds. Not a `proptest`, but a table over the awkward cases.

## Phase 2: Sidecar loading & validation

### Task 2.1: Sidecar schema and parser

- **Status:** 🟢 DONE
- [x] Create `specmock-core/src/sidecar.rs` with the serde model for `version`, `operations`,
      `scenarios`, `seed`, and `store` blocks.
- [x] `version` is required; unknown values are a hard startup error naming supported versions.
- [x] Accept YAML and JSON, matching how specs are already loaded (`serde_yml` / `serde_json`).
- [x] Resolve the sidecar through the existing `RefResolver` so `$ref` works inside it and into
      the spec, with the same depth limit and allowed-root sandboxing.
- [x] Shorthands: `store: read` infers `resource` and `key` from the path template.
- [x] Unit tests: both formats, `$ref` into the spec, shorthand expansion, missing `version`.

### Task 2.2: Referenced operations and the reserved prefix

- **Status:** 🟢 DONE
- [x] Resolve each operation's `match` (method + path template) against the loaded
      `OperationSpec` list; unknown match is a hard startup error.
- [x] Detect path collisions between the reserved `/_specmock/` prefix and declared operations.
- [x] Reject scenarios declaring an operation beneath `/_specmock/`.
- [x] Unit tests: unknown operation, colliding control path, nested `$ref`'d operation.

### Task 2.3: FSM legality validation

- **Status:** 🟢 DONE
- [x] Hard error: `initialState` names an undeclared state.
- [x] Hard error: a transition targets an undeclared state.
- [x] Hard error: a state references an operation absent from the catalogue.
- [x] Warning only: an unreachable state.
- [x] Warning: an operation in the catalogue referenced by no scenario (likely dead config).
- [x] Reject a sidecar supplied together with `mode: proxy`, naming both settings.
- [x] Unit tests for each hard error and each warning.

## Phase 3: Runtime precedence & store

### Task 3.1: `Prefer` directive extensions

- **Status:** 🟢 DONE
- [x] Add `scenario: Option<String>` and `session: Option<String>` to `PreferDirectives`.
- [x] Both bypass directives (`code=`, `dynamic=true`) are documented as read-only escape
      hatches that do **not** advance state.
- [x] `dynamic=true` bypasses the store and generates fresh.
- [x] Unit tests: parsing, repeated `Prefer` headers, empty values, case handling.

### Task 3.2: Session resolution

- **Status:** 🟢 DONE
- [x] Absent `session`: resolve to a single default instance.
- [x] Absent `scenario`: resolve against the default scenario, or fail if none is declared.
- [x] An unseen `session` key creates its instance implicitly, matching the singleton default.
- [x] Unknown `scenario` name returns `404` naming the scenarios that exist.
- [x] Hold instances in an `scc::HashMap` guarded by the session cap (Task 4.2).
- [x] Unit tests: default, keyed isolation, implicit creation, unknown scenario.

### Task 3.3: Response precedence chain

- **Status:** 🟢 DONE
- [x] Implement the five-step order from the design: `Prefer: code=` → `dynamic=true` →
      scenario override → operation catalogue → existing default selection.
- [x] A scenario is an additive overlay: an operation absent from the named scenario falls
      through, causes no transition, and leaves state unchanged.
- [x] A store-backed read supplies the body directly, bypassing example/faker selection.
- [x] Integration tests asserting each step wins over the ones after it.

### Task 3.4: Transition ordering

- **Status:** 🟢 DONE
- [x] Validate the request **before** advancing state or touching the store.
- [x] A rejected request leaves both untouched.
- [x] A store `404` on a valid request **does** advance state.
- [x] Transitions are repeatable; a second firing requires a declared self-loop.
- [x] Transitions log at INFO via `tracing`: scenario, session, from, to, operation.
- [x] Property test: the state after any request is always a declared state.
- [x] Integration test: an invalid request followed by a valid one sees unchanged state.

## Phase 4: Errors & bounds

### Task 4.1: Stateful error vocabulary

- **Status:** 🟢 DONE
- [x] Do **not** encode these as `ValidationIssue`. Add RFC 7807 extension members to
      `ProblemDetails` carrying `scenario`, `session`, `state`, and `operation`.
- [x] `409` for an operation not legal in the current state, citing RFC 9110.
- [x] `404` for unknown resource and unknown scenario; `409` for create key collision.
- [x] `errors` stays empty on these responses.
- [x] Unit tests asserting status, `detail`, extension members, and empty `errors`.

### Task 4.2: Bounds

- **Status:** 🟢 DONE
- [x] `--max-store-entries` per instance → `500` naming the knob.
- [x] `--max-sessions` → `429` naming the knob.
- [x] **No LRU eviction anywhere.** Both caps report loudly; neither silently drops.
- [x] Defaults are generous enough that ordinary use never hits them.
- [x] Tests for both caps and for the absence of eviction below the threshold.

## Phase 5: Observability, control, CLI

### Task 5.1: Control endpoints

- **Status:** 🟢 DONE
- [x] `GET /_specmock/state` and `POST /_specmock/reset` as explicit axum routes, registered
      before `app.fallback(...)`.
- [x] Optional bearer token, minted per run, defaulting to on, logged at startup.
- [x] Reset returns the instance to its initial state **and** empties the store; the two are
      not separable operations.
- [x] Tests: token required when enabled, absent when disabled, reset clears state and store.

### Task 5.2: CLI flags

- **Status:** 🟢 DONE
- [x] Add `--log-level`. **This does not exist today** and is required for the INFO transition
      log to be reachable at all.
- [x] Add `--scenarios`, `--max-store-entries`, `--max-sessions`.
- [x] Forward all of them in SDK process mode, or process parity breaks.

### Task 5.3: `Location` override on create

- **Status:** 🟢 DONE
- [x] When a store-backed create succeeds, populate the declared `Location` response header from
      the created resource, overriding the faker value.
- [x] Record separately that the un-overridden faker `Location` is a pre-existing defect for
      any spec declaring it, independent of scenarios.

## Phase 6: SDK

### Task 6.1: Embedded surface

- **Status:** 🟢 DONE
- [x] `MockServer::reset_scenarios()` and `MockServer::scenario_state(session)`.
- [x] `MockServerBuilder` gains `scenarios`, `max_store_entries`, `max_sessions`.
- [x] Ensure `Drop` still shuts down cleanly with live sessions.

### Task 6.2: Process-mode parity

- **Status:** 🟢 DONE
- [x] Process mode reaches the control endpoints over HTTP, using the minted token.
- [x] Extend `tests/sdk_process.rs` to assert identical responses **and identical reset
      behaviour** between embedded and process modes. That file already asserts response
      parity; reset must not be the first thing to break it.

## Phase 7: Documentation & fixtures

### Task 7.1: Example specs

- **Status:** 🟢 DONE
- [x] Add `docs/specs/store.openapi.yaml` exercising CRUD + pre-seed + a state-specific error.
- [x] Add `docs/specs/store.scenarios.yaml` as the worked example referenced by the README.
      Verified by hand, not only by test: create/read-back, creation-order listing, delete then
      404, the declared 503, an untokened control request, and transitions at the default log
      level.

### Task 7.2: README

- **Status:** 🟢 DONE
- [x] Scenario file reference, the five-step precedence chain, and the store semantics table.
- [x] Restate determinism as sequence-based (ADR 0003), including that state does not survive
      restart.
- [x] Document the documented limits: no collection filtering or pagination, no atomic
      read-modify-write, no WS/gRPC scenarios.

### Task 7.3: Corrections owed from the audit

- **Status:** 🟢 DONE
- [x] Remove the `style`/`explode` claim at `README.md:61` — never implemented.
- [x] Correct the "Uses tonic for standard HTTP/2 transport" claim at `README.md:208` — the
      transport is `hyper_util::server::conn::auto`; `tonic` is only the `Status` error type.
- [x] Add a `stateful` row to the Prism comparison table.
- [x] Update `specs/context.md:55`'s test count, which is stale by roughly 3x.

## Phase 8: Verification

### Task 8.1: Regression gate

- **Status:** 🟢 DONE
- [x] `just format`, `just lint`, `just test` pass with no warnings.
- [x] All 730 existing tests still pass. **Absent a sidecar, behaviour is byte-identical** —
      this is the single most important check in the phase.

### Task 8.2: Test coverage

- **Status:** 🟢 DONE
- [x] Integration: full CRUD flow, pre-seeded read, session isolation, every error status,
      the session cap, control endpoints, and `Prefer: code=` not advancing state.
- [x] Each `Prefer` bypass verified end to end: neither `code=` nor `dynamic=true` advances
      the machine, and `dynamic=true` does not read the store.

### Task 8.3: Manual smoke test

- **Status:** 🟢 DONE
- [x] Serve `docs/specs/users.openapi.yaml` with its sidecar and walk the flow with `curl`,
      confirming transitions in the log at `--log-level info`.
- [x] Confirm the `429`/`500` cap responses name their knob.
