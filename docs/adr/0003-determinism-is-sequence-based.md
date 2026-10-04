# Determinism is a property of a request sequence, not of a request

The guarantee becomes "same seed + same request *sequence* produces the same response
sequence", replacing today's "same seed + same request produces the same response".
No new CLI flag or SDK option is introduced to opt out.

This is forced by the CRUD store, not chosen for convenience: DELETE-then-GET-must-
return-404 requires remembering that a deletion occurred, which is irreducibly
order-dependent. The alternative — deriving a value from `hash(seed, key)` for
unknown keys — was rejected because it can only synthesise keys it was told to expect
and so directly contradicts the delete semantics.

The consequence for readers of the runtime: response generation can no longer be
reasoned about from a single request in isolation. Any code that assumes `OperationSpec::mock_response`
is a pure function of `(operation, request, seed)` is wrong once a scenario is active.
