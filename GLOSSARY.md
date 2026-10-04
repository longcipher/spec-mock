# spec-mock

A pure-Rust mock runtime that derives HTTP, WebSocket, and gRPC behaviour from OpenAPI,
AsyncAPI, and Protobuf documents, so clients under test can exercise a real contract
without a real server.

## Specification

**Operation**:
A (method, path template) pair declared by an OpenAPI document, together with its
parameters, request body, and declared responses. The unit behaviour attaches to.
_Avoid_: Endpoint, route, handler, action

**Route**:
The result of matching an incoming method and path against the router, naming the
matched Operation and the values bound to its path parameters.
_Avoid_: Match, lookup, endpoint

**Channel**:
A named AsyncAPI stream. spec-mock validates what _arrives_ against the channel's
`publish` schema and replies using its `subscribe` schema. That is the specification's
view of the application, and it is the reverse of the client's — read these two field
names from the application's side, never the caller's.
_Avoid_: Topic, subject, stream

**Scenario file**:
The sidecar document declaring scenarios and their stores, resolved alongside the
OpenAPI document.
_Avoid_: Behaviour file, config, overlay, companion

## Scenarios

**Scenario**:
A named flow through a mock's behaviour, comprising a starting state, a set of states,
and the transitions between them. The unit of statefulness: a client opts into one by
name and remains inside it for the life of a scenario instance.
_Avoid_: Flow, test case, test scenario, session
_Note_: "Scenario" also appears in this repository's specifications to mean a test
case. Those are unrelated; see this file for the product sense.

**Scenario instance**:
One concurrent, isolated run of a scenario, holding its own current state and its own
store. A mock serves a single default instance until a client supplies a session.
_Avoid_: Run, execution, world

**Session**:
The client-supplied key naming a scenario instance. Requests carrying no session
resolve to the mock's one default instance.
_Avoid_: Instance (the session names an instance; it is not the instance)

**State**:
A named position within a scenario. Exactly one state is current per scenario instance
at any moment, and it determines which declared response an operation returns.
_Avoid_: Status, phase, mode, step

**Transition**:
The move from one state to another, triggered by a matched operation. A state reached
with no transition declared for the next operation is terminal for that operation.
_Avoid_: Edge, event, action, flow

**Store**:
The retained invented resources belonging to one scenario instance, keyed by
identifier. Operations write to it and are answered from it.
_Avoid_: Database, cache, repository, state

**Resource**:
An invented record held in a store, keyed by an identifier drawn from the request
path. Every resource is schema-valid because it is generated from the schema its
operation declares.
_Avoid_: Entity, object, record, item

## Contracts

**Validation issue**:
One structured failure, locating the offending value in the instance and the violated
rule in the schema. HTTP, WebSocket, and gRPC all report failures in this one shape,
without translation.
_Avoid_: Error, violation, problem

**Problem details**:
The RFC 7807 envelope carrying zero or more validation issues.
_Avoid_: Error response, error body

**Seed**:
The integer making generated data reproducible for a given request sequence.
Determinism is a property of a sequence, not of a single request.
_Avoid_: Salt, random seed

**Mock mode**:
Whether the runtime invents responses or forwards to an upstream and validates what
comes back.
_Avoid_: Mode (ambiguous with state)
