# spec-mock Agent Instructions

## Non-negotiables

- Surgical edits only: every changed line must trace to the request. No drive-by refactors, no reformatting untouched code, no deleting pre-existing dead code (mention it instead).
- Fix root causes. Never silence a warning, weaken a check, or patch a symptom to make a gate pass.
- Never report completion while any `just` gate below fails. Report the failure verbatim and stop.
- Stop and ask when a request is ambiguous, conflicts with existing conventions, or requires credentials, publishing, or destructive operations.
- When a correction reveals a missing rule, add one specific rule here ("always X for Y", never "be careful with Y"); delete rules that never catch real errors.

## Scope

- spec-mock is a pure-Rust mock runtime: it derives HTTP (OpenAPI 3.0/3.1), WebSocket (AsyncAPI 2.x/3.x) and gRPC (Protobuf over HTTP/2) behaviour from a contract document, so a client under test can exercise a real API without a real server.
- `crates/specmock-core` owns the contract types, schema validation, deterministic generation and the scenario/store model; `crates/specmock-runtime` owns the HTTP (`http/`), WebSocket (`ws/`) and gRPC (`grpc/`) servers; `crates/specmock-sdk` embeds the runtime in `#[tokio::test]` and in process mode; `bin/spec-mock` is the standalone CLI.
- `GLOSSARY.md` defines the domain vocabulary (Operation, Route, Channel, Scenario file, and the terms to avoid). Use its words, not synonyms.
- `docs/adr/` holds the settled decisions, `docs/specs/` the sample contracts, `specs/` the dated implementation plans, and `context7.json` the Context7 documentation registration.
- English only in code, comments, docs, and commit messages.

## Execution

- Parallelize aggressively: dispatch subagents for independent research, exploration, and implementation.
- NEVER run two `cargo` commands concurrently, and never overlap file edits with a running `cargo` command (`target/` file lock): finish edits, wait, then run.
- NEVER use `git worktree`. Edit the current branch in place.
- TDD for behavior changes: failing test first, smallest change to pass, then run `just fuzz-smoke` to prove the parsers still hold.

## Verification loop

After each feature or bug fix, in order; any failure means the task is not done:

```bash
just format
just lint
just test
```

`just lint` is the CI gate: `typos`, `rumdl`, `taplo fmt --check`, `cargo +nightly fmt --check`, `cargo +nightly clippy -D warnings`, `cargo machete`. `just ci` adds `test` and `build`; the CI fuzz job additionally runs `just fuzz-build` and `just fuzz-smoke 15`.

## Rust workspace rules

- Edition 2024, resolver 3. Workspace lints live in the root `Cargo.toml`; members opt in with `[lints] workspace = true`, and `specmock-core`/`specmock-runtime` relax `expect_used` to `allow` on top of it.
- `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stdout`, `print_stderr` and `allow_attributes` are `deny`, and `rust_2018_idioms` is `deny` here. The only sanctioned escape hatch is `#[expect(lint, reason = "...")]`.
- The fuzz crates under `crates/*/fuzz` are detached workspaces and are excluded from the root workspace, so `cargo fmt --all` does not cover them; `cargo fuzz` builds them with sanitizer profiles.
- TOML is formatted with `taplo` and `.taplo.toml`; do not introduce `cargo sort`, which CI does not install.
- There is no `rust-toolchain.toml`: CI installs nightly, so use `cargo +nightly fmt` and `cargo +nightly clippy` exactly as the Justfile does. There is no `.cargo/config.toml`, so no `rustc-wrapper` has to be cleared.
- Coverage uses `cargo llvm-cov`, never `cargo tarpaulin`: tarpaulin's profdata parser rejects current nightly rustc profiles.
- Never hand-type dependency versions. Shared crates go into root `[workspace.dependencies]` and members reference them with `workspace = true`.

## Dependency choices

- HTTP client: `hpx` with rustls — never `reqwest`. `jsonschema` is pulled in with `default-features = false` on purpose, so no `reqwest`/`rustls`/`aws-lc-sys` C build is dragged in; `$ref` resolution is `specmock-core`'s own `ref_resolver`.
- gRPC: `tonic` with `prost`, `prost-reflect` and `protox`. WebSocket: `tokio-tungstenite` over `axum`/`hyper`/`hyper-util`/`tower`. AsyncAPI payloads are YAML, hence `serde_yml`.
- Concurrency: `scc::HashMap` for the scenario and CRUD stores; never `dashmap` or a `RwLock<HashMap<...>>` on a request path.
- Deterministic generation uses `rand` with `rand_chacha` and `rand_regex`; the seed is the only source of nondeterminism.
- Errors: `thiserror` in the crates, `eyre` in the binary. Concrete error types through `?`; never string errors.
- Forbidden here: `anyhow`, `log`, `reqwest`, `dashmap`.

## Code conventions

- Three settled decisions are load-bearing; re-read the ADR before touching them: error injection is folded into the scenario model (`docs/adr/0001`), behaviour lives in a sidecar file rather than `x-specmock-*` extensions (`docs/adr/0002`), and determinism is a property of a request *sequence*, not of a single request (`docs/adr/0003`).
- Because of ADR 0003, response generation is not a pure function of `(operation, request, seed)` once a scenario is active; do not refactor towards that assumption.
- The gRPC server is a passthrough: it never rewrites token representations, it only translates between the contract document and the wire format. Keep the `proto` shapes and the codec in sync.
- `proptest` properties live next to the crate logic they exercise and run in the ordinary `cargo test` loop; the fuzz targets (`faker`, `json_pointer`, `ref_resolver`, `validate`, `media_encode`, `negotiate`) cover the hostile-input parsers.
- The Prism comparison suite is `#[ignore]`d and needs `npm i -g @stoplight/prism-cli`; run it with `just integration-test`, never as part of `just test`.
- `crates/specmock-core/src/faker.rs`, `crates/specmock-runtime/src/http/media.rs` and `crates/specmock-runtime/src/grpc/protobuf.rs` legitimately contain CJK characters as Unicode test data; `just check-cn` excludes them.
