# Behaviour lives in a sidecar file, not `x-specmock-*` extensions

Scenario and CRUD behaviour is declared in a sidecar document resolved next to the
OpenAPI file, not in `x-specmock-*` vendor extensions inside the spec. Inline
extensions remain supported as an override for spec authors who own their spec.

We chose the sidecar because requiring extensions means forking the input spec, and
forking breaks every `$ref` into shared schema files — at which point the mock is no
longer exercising the real contract. spec-mock's value is pointing at a spec written
by someone else, so the sidecar has to be sufficient on its own. The existing
`x-specmock-variant` extension (read in `crates/specmock-core/src/faker.rs`) sets the
opposite precedent and is deliberately not extended to this case; it stays a
discriminator hint because it is read *from* the schema being generated, not authored
alongside it.
