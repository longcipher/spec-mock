# Design: Stateful Mocking

Generated on 2026-10-04 from a design grilling session. Every decision below was
settled explicitly; the alternatives are recorded so they are not re-litigated.

## Context

`spec-mock` v0.1.0 is complete against its stated scope: OpenAPI/HTTP, AsyncAPI/WebSocket,
and Protobuf/gRPC, with a shared validation model and an embeddable SDK. Every request is
independent. The Rust source contains **zero** occurrences of `scenario`, `stateful`,
`StateStore`, `latency`, or `fault` — confirmed by search, not inference.

The one existing lever that reaches a non-2xx response is `Prefer: code=`. That is the entire
extent of the tool's ability to express behaviour that depends on what happened before.

Against Prism, whose own feature set this README tabulates line by line, the notable gaps are
statefulness, gRPC reflection, client-streaming, and URL `$ref`. Of those, statefulness is the
one that changes what the tool *is* rather than adding a row to the table: it is the difference
between a response generator and something a client can be tested against.

The repo has strong existing properties this design must not break: determinism from a `seed`,
one `ValidationIssue` shape shared across all three protocols, RFC 7807 errors, and an asserted
equivalence between embedded and process SDK modes.

## Goals

1. Let a client walk a named flow whose responses depend on what already happened.
2. Let a client create a resource and subsequently read, update, and delete **that same
   resource** back.
3. Keep every response schema-valid and every error structured and diagnosable.
4. Keep determinism, restated honestly for sequences.
5. Leave behaviour absent when no scenario file is supplied, byte-for-byte as today.

## Non-goals

- **WebSocket and gRPC adoption.** Core types stay protocol-agnostic so either can adopt them,
  but neither is implemented in this change. A long-lived connection is itself the natural
  state scope, which makes WS a genuinely different problem rather than a port.
- **A domain model with relationships and invariants.** Bounded work only: a keyed store of
  generated values. Inventing cross-request consistency is unbounded and fights determinism.
- **Scenarios in proxy mode.** Rejected at startup. The upstream holds the real state; a mock
  store layered in front of it is incoherent.
- **State surviving restart.** Process-lifetime only. `e2e_full_stack.rs` already pins
  cross-restart seed determinism, so this is stated rather than left implicit.
- **Cross-spec scenarios.** One sidecar per spec file.
- **Session expiry.** A TTL reintroduces wall-clock dependence into a reproducibility tool.
- **Query-parameter filtering or pagination** on collection reads.
- **Atomic read-modify-write.** Accepted and documented, not engineered around.

## Design

### Domain model

Full definitions in `GLOSSARY.md`. The nouns are new; none existed before this change.

- **Scenario** — a named flow: a starting state, a set of states, and transitions.
- **Scenario instance** — one isolated run, holding its own current state and its own store.
- **Session** — the client-supplied key naming an instance. Absent a session, requests resolve
  to one default instance.
- **State** — a named position; exactly one is current per instance.
- **Transition** — a move between states, triggered by a matched operation. Repeatable; firing
  it a second time requires a declared self-loop. "Only once" is expressed by the target state
  being terminal, not by a consumed edge.
- **Store** — the retained invented resources of one instance.
- **Resource** — an invented record, keyed by identifier, valid against the schema its
  operation declares.

`Scenario` is WireMock's term, chosen because this repo tabulates its Prism parity in the
README and matching competitor vocabulary aids discovery. It collides with "scenario" meaning
*test case* in `specs/` prose; `GLOSSARY.md` records the disambiguation.

### Sidecar file

Behaviour lives in a sidecar resolved next to the OpenAPI document, **not** in `x-specmock-*`
extensions (ADR 0002). Requiring extensions means forking the input spec, and forking breaks
every `$ref` into shared schema files — after which the mock is no longer exercising the real
contract. spec-mock's value is pointing at a spec written by someone else, so the sidecar must
be sufficient alone. `x-specmock-variant` remains and keeps its existing meaning: it is read
*from* the schema being generated, not authored alongside it.

Loaded through the **existing `RefResolver`**, so `$ref` works inside the sidecar and into the
spec, with the same depth limit and allowed-root sandboxing. One resolver, two documents.

```yaml
version: 1                      # required; unknown versions fail startup

operations:                     # named catalogue, shared across scenarios
  createUser:
    match: { method: POST, path: /users }
    store:
      resource: /users          # key namespace (see "Key namespacing")
      action: create
      keyFrom: { in: response, pointer: /id }
      populate: body
    responses:
      "201": { body: { example: { id: usr_1, name: Ada } } }

  getUser:
    match: { method: GET, path: /users/{id} }
    store: { resource: /users, action: read, key: "{id}" }
    responses: { "200": {} }

  listUsers:
    match: { method: GET, path: /users }
    store: { resource: /users, action: list }

  deleteUser:
    match: { method: DELETE, path: /users/{id} }
    store: { resource: /users, action: delete, key: "{id}" }

scenarios:
  - name: checkout
    initialState: anonymous
    seed:                                     # pre-existing store contents
      - resource: /users
        items:
          - { id: usr_1, name: Ada }
    states:
      anonymous:
        createUser: { to: authed }
      authed:
        getUser: {}                            # legal here, no transition
        deleteUser: { to: anonymous }
        checkout:                              # state-specific response: fault injection
          to: failed
          responses:
            "500": {}
```

The structure is **state-centric**. An illegal (operation, state) pair cannot be expressed,
which is the config-file equivalent of the typestate discipline `AGENTS.md` asks for. The
alternative — a flat operation-centric rule list, as WireMock uses — permits two rules to
compete for one state with load order deciding the winner.

Operations are referenced by method + path rather than `operationId`, because `operationId` is
optional in OpenAPI and this codebase already parses it and never reads it.

CRUD is declared, not inferred from the HTTP method. Inference has no defensible answer at
create time: `POST /users` has no identifier in the path, so the mock must invent one, return
it, and key the store by it, and there is no obvious default for where that key is read from.
It also misreads verbs — in `POST /users/{id}/orders` the created thing is a nested resource,
not a `User`. Shorthands are provided for the mechanical cases (`store: read` infers `resource`
and `key` from the path template).

Pre-seeding exists because "an authenticated user hits a protected endpoint" is the canonical
protected-endpoint test. Without it, that test is forced through a login flow it is not testing.
Seeded resources are seed-derived and schema-valid, so they are indistinguishable from created
ones.

### Key namespacing

The store key is `(collection path template with its parameters bound, identifier)`. So
`POST /users/5/orders` with id `123` is namespaced `/users/5/orders`, and is distinct from
`GET /orders/123`.

A flat namespace would let an order be retrieved as a user. Namespacing by resource *type*
would require inferring the type from the schema, which is guesswork when the spec uses `oneOf`
or omits a type name. Namespacing by path template is the only option derivable from the spec
alone.

### Response precedence

Resolved in order; the first match wins.

1. `Prefer: code=<status>` — bypass the machine entirely, **no transition** (ADR 0001).
2. `Prefer: dynamic=true` — bypass the store, generate fresh, **no transition**.
3. Scenario transition's `responses` override for the current state.
4. Operation catalogue `responses`.
5. Today's default selection: first `2xx`, then `default`, then first.

Within whichever response wins, body generation stays `example` → first `named_examples` →
faker — **except** that a store-backed read supplies the body directly, because otherwise CRUD
would be pointless.

Steps 1 and 2 are both read-only escape hatches and neither advances state. This keeps
`dynamic=true` meaning one thing: without that, it would return a stored resource when a
scenario is active and a fresh value otherwise, and no header should change meaning based on
hidden server state.

A scenario is an **additive overlay**, not an allow-list. An operation absent from the named
scenario falls through to steps 4–5, causes no transition, and leaves state unchanged. Under
strict allow-list semantics every test would have to enumerate every operation in the file
before the mock worked at all.

Because a request names exactly one scenario, there is no cross-scenario ambiguity at request
time.

### Store semantics

| Operation | Behaviour when the resource is absent |
| --- | --- |
| `create` | Store it. Collision on an existing key → `409`. |
| `read` | `404`. |
| `update` (`PUT`/`PATCH`) | `404`. Not an upsert. |
| `delete` | `404`. Subsequent reads also `404`. |
| `list` | `[]`. Returns the stored collection in creation order. |

`404` on update is deliberate. RFC 9110 permits create-on-`PUT`, but for a mock the `404` is the
product: a test that updates something it never created has a bug, and upsert would let it pass
green.

An empty `list` returns `[]` rather than invented items, because faker output for an empty store
makes the mock actively misrepresent state. Query parameters are not applied — no filtering, no
pagination — and a partially-honoured list is worse than a documented simple one.

### Transition ordering

State advances **only if the request passes validation**. A rejected request leaves state and
store untouched. The client is the thing under test, so a client bug must not poison subsequent
requests; under the alternative, one malformed request silently desynchronises every later
assertion and the failure surfaces nowhere near its cause. A real server does not change state
when it rejects a request.

Two corollaries: a `404` from the store is a legitimate outcome of a *valid* request, so it
**does** advance; and per ADR 0001, `Prefer: code=` bypasses the machine and does **not** advance.

### Determinism

The guarantee is restated (ADR 0003): **same seed + same request *sequence* ⇒ same response
sequence.** This is forced by the store, not chosen — delete-then-read-must-404 requires
remembering that a deletion happened, which is irreducibly order-dependent. Synthesising values
for unknown keys from `hash(seed, key)` was rejected precisely because it contradicts delete
semantics.

The consequence for readers: response generation is no longer a pure function of
`(operation, request, seed)`. Any code assuming `OperationSpec::mock_response` is pure is wrong
once a scenario is active.

`GET /users` returning creation order, and collection reads returning `[]`, are both part of
this guarantee.

### Errors

Stateful failures are **not** `ValidationIssue`. Every one of its four fields describes a JSON
Schema violation; a missing transition has no instance and no schema pointer, so encoding one
would mean writing pointers that point at nothing and diluting the shared vocabulary the
architecture is built on.

Instead: `ProblemDetails` with a descriptive `detail`, an **empty** `errors` list, and RFC 7807
extension members carrying `scenario`, `session`, `state`, and `operation` as structured fields.

| Condition | Status |
| --- | --- |
| Unknown scenario name | `404`, `detail` lists the scenarios that exist |
| Operation not legal in the current state | `409` — RFC 9110's "conflict with the current state" |
| Unknown resource on read/update/delete | `404` |
| Create key collides with an existing resource | `409` |
| New session key | Creates an instance implicitly, matching the singleton default |
| Live session cap exceeded | `429`, `detail` names the knob |
| Store entry cap exceeded | `500`, `detail` names the knob |

The session/store asymmetry is deliberate: exceeding the session cap is client-driven, whereas
exceeding the store entry cap usually means the scenario itself is wrong.

### Sidecar validation

Strict at startup, matching how `RefResolver` already treats an unresolvable `$ref`:

- **Hard errors** — unknown operation reference, missing `initialState`, transition targeting an
  undeclared state, sidecar schema violation, unknown `version`, an operation whose `match` does
  not exist in the spec.
- **Warning** — an unreachable state is dead configuration, not a failure.

### Bounds

Neither cap is LRU. Silent eviction was rejected for the store because it surfaces as an
unexplained `404` rather than an error; that reasoning applies identically to sessions, and
reusing eviction for one and not the other would be incoherent. Both are generously defaulted,
both report loudly when hit.

- `--max-store-entries` — per scenario instance.
- `--max-sessions` — live instances. Needed because a client sending a fresh `session=` per
  request creates a fresh empty store each time: low entries, unbounded sessions.

### Observability and control

A state machine you cannot observe drives users back to WireMock.

- Transitions log at **INFO** via `tracing`: scenario, session, from-state, to-state, operation.
  INFO, not DEBUG — this is the primary diagnostic.
- **`--log-level`** CLI flag. This does not exist today; the CLI currently has no verbosity
  control at all, so a user needing transitions has no way to enable logging.
- `MockServer::reset_scenarios()` and `MockServer::scenario_state(session)`. Reset returns the
  instance to its initial state **and** empties the store; the two are not separable.
- `GET /_specmock/state` and `POST /_specmock/reset` for process mode, where there is no handle.

`/_specmock/` is a **reserved prefix**: startup fails if the sidecar declares an operation
beneath it. `crates/specmock-runtime/src/http/mod.rs:130` serves OpenAPI paths via
`app.fallback(...)`, so explicit routes always win — without the reservation, a declared
operation beneath the prefix becomes silently unreachable and the symptom is a mock returning
control-endpoint behaviour for a real endpoint.

The control endpoints are the only mutating surface on the server, so they take an **optional
bearer token minted per run and logged at startup**, defaulting to on. `SEC-005` (localhost
binding) is already recorded as an accepted, deferred risk, and that reasoning is sound for the
read path, which clients must call with no ceremony. It is much weaker for `reset`.

Embedded and process mode parity is already asserted by `tests/sdk_process.rs`; reset must not
be the first thing to break it.

### Pre-existing defects found while designing

Neither is caused by this change, and both are logged rather than folded into scope.

1. **Declared `Location` headers are faker-generated.** `ResponseHeaderSpec` emits declared
   response headers from `example` or the faker, so a spec declaring `Location` on a `201`
   receives a random string today. This design requires the store to override it (populated from
   the created resource), but the underlying bug predates scenarios and affects any spec declaring
   `Location`. A wrong `Location` is worse than an absent one: it looks like a working URL.
2. **No route conflict detection between WS paths and OpenAPI paths.** WS paths are explicit
   axum routes and OpenAPI paths are the fallback, so a spec declaring `GET /ws` already loses
   silently. The parity spec's Task 4.3 step 5 claimed this was implemented; it was not.
   The control prefix adds a third route family to the same unprotected gap.

## Verification

- `just format`, `just lint`, `just test` pass with no warnings; existing 730 tests unaffected.
- **Absent a sidecar, behaviour is unchanged** — the single most important regression check.
- Core unit tests: FSM legality, transition ordering, store CRUD semantics, key namespacing,
  bounds, caps.
- Property tests: transitions never leave an undefined state; store operations preserve
  schema-validity; a read after a create always succeeds within one instance and never across two.
- Integration: full CRUD flow, pre-seeded reads, session isolation under concurrency, each error
  status, cap exhaustion, control endpoint, `Prefer` bypass not advancing state.
- Embedded/process parity: identical responses and identical reset behaviour.
- `README.md`: scenario file reference, the restated determinism guarantee, and the removal of
  the `style`/`explode` and "uses tonic" claims recorded in `specs/README.md`.
