# Rust Workspace Agent Instructions

## Scope

- This template targets Rust workspaces only.
- `bin/` contains CLI binary crates.
- `crates/` contains reusable library crates.
- No frontend/web-framework-specific assumptions.

## Cargo Workspace Rules (Critical)

1. Always take a dependency at the latest version released for it. Never pin an older release, never downgrade to match another manifest, and never hand-type a version number; use `cargo add` so the version is resolved from the registry.
2. Add workspace-level dependencies with:

   ```bash
   cargo add <crate> --workspace
   ```

3. Add sub-crate dependencies with:

   ```bash
   cargo add <crate> -p <crate-name> --workspace
   ```

4. Root `[workspace.dependencies]` must resolve external crates from the crates.io registry; never use `git`, `path`, or other non-registry sources for them.
5. Root `[workspace.dependencies]` must not carry features by default.
6. Sub-crates must use `workspace = true` for `version`, `edition`, and shared dependencies.
7. Before declaring a dependency change done, re-check every entry against the newest published release and confirm that `cargo build`, `just lint`, and `just test` all pass with no warnings and no errors.

## Dependency Version Policy

- Use the latest released version of every crate. Upgrading is the default, not an exception that needs justification.
- Never record version numbers in this file. Version lists here go stale and silently become wrong; resolve versions from the registry at the moment of the change instead.
- Never hold a dependency back for unrelated reasons. When a newer major or minor release lands, upgrade it in the same change and fix any fallout.
- Keep the manifest honest: if the workspace was last synchronized long after a release, refresh the whole dependency set rather than only the crate being touched.
- Prefer checking the actual latest release over inferring it. Useful ways to confirm:

  ```bash
  cargo search <crate> --limit 1
  ```

## Dependency Priority and Forbidden Choices

- HTTP client preference: `hpx` (with `rustls`) over `reqwest`.
- Concurrent map/set preference: `scc` over `dashmap` and `RwLock<HashMap<...>>`.
- Forbidden by default: `anyhow`, `log`, `reqwest`, `dashmap`.

## Engineering Principles

### Rust Implementation Guidelines

1. Error handling:
   - Application layer: `eyre`.
   - Library layer: `thiserror`.
2. Concurrency:
   - Prefer lock-free/container-first approaches (`scc`).
   - Avoid `Arc<Mutex<T>>` when better alternatives are available.
3. Observability:
   - Logging: `tracing` only.
   - Metrics/traces: OpenTelemetry OTLP gRPC.
   - Prometheus should not be the default instrumentation path.
4. Safety:
   - Use `unsafe` only when strictly necessary and document the safety invariants.

### Key Design Principles

- Modularity: Design each crate so it can be used as a standalone library with clear boundaries and minimal hidden coupling.
- Performance: Prefer architectures that support parallelism, memory-mapped I/O for large read-heavy workloads, optimized data structures, and lock-free data types.
- Extensibility: Use traits and generic types to support multiple implementations without invasive refactors.
- Type Safety: Maintain strong static typing across interfaces and internals, with minimal use of dynamic dispatch.

### Performance Considerations

- Avoid allocations in hot paths; prefer references and borrowing to reduce allocation and copy overhead.
- Use `rayon` for CPU-bound parallel processing.
- Use `tokio` async/await for I/O-bound concurrency.

### Concurrency and Async Execution

- Prefer atomic types (`AtomicUsize`, `AtomicBool`, etc.) with explicit `Ordering` for simple shared state.
- Use `scc` for highly concurrent maps/sets; avoid `Arc<RwLock<HashMap<...>>>` and `Arc<Mutex<HashMap<...>>>` on hot paths.
- Use `moka` for concurrent caches instead of custom LRU implementations.
- Prefer `parking_lot::{Mutex, RwLock}` over `std::sync` locks for synchronous locking.
- Release `std::sync::Mutex` and `parking_lot::Mutex` guards before hitting any `.await` point.
- Use `tokio::sync::Mutex` for locks that span across `.await` points.
- Use `tokio::task::spawn_blocking` for CPU-bound work and blocking I/O.
- Batch work or use bounded worker patterns instead of spawning massive volumes of tiny Tokio tasks.
- Channel selection:
  - Async-to-Async: `tokio::sync::mpsc` / `tokio::sync::broadcast`
  - Sync/MPMC: `crossbeam-channel` or `flume`
  - Avoid `std::sync::mpsc`

### Memory and Allocation

- For binary server applications, configure `tikv-jemallocator` or `mimalloc`.
- For trusted internal hash keys, prefer `ahash` or `rustc-hash` over default SipHash-based maps.
- Use `compact_str` or `smol_str` for small-string-heavy paths.
- Prefer `beef::Cow` over `std::borrow::Cow` when minimizing footprint.
- Use `bytes::Bytes` / `bytes::BytesMut` for network buffers; pass `Bytes` instead of cloning `Vec<u8>`.
- For critical serialization hot paths, prefer `rkyv` or `zerocopy`; reserve `serde_json` for config and non-critical APIs.

### Type and Layout

- Order struct fields from largest to smallest unless stronger semantic grouping is required.
- Use `#[repr(C)]` / `#[repr(packed)]` only for FFI or fixed protocol layout requirements.
- Keep error types compact on hot paths; box large error payloads when needed to reduce `Result<T, E>` size.
- Prefer typestate-style APIs for compile-time state transitions instead of runtime state checks.

### Tooling and Hot Paths

- Keep code clean under `clippy::pedantic`, `clippy::nursery`, and `clippy::cargo` (allow `missing_errors_doc` for non-public APIs when needed).
- Use `#[inline]` for tiny methods called in hot loops or on every request, especially across crate boundaries.
- Mark cold error paths with `#[cold]` and `#[inline(never)]` when it improves hot-path instruction locality.

### Common Pitfalls

- Keep async tasks non-blocking; offload CPU-bound work to `spawn_blocking` or `rayon`.
- Handle errors explicitly and consistently with the `?` operator and concrete error types.

### What to Avoid

- Incomplete implementations: finish features before submitting.
- Large, sweeping changes: keep changes focused and reviewable.
- Mixing unrelated changes: keep one logical change per commit.

## Foundry Rules (If Solidity Exists)

- Use `soldeer` for dependencies; do not use git submodules.
- Required commands:
  - `forge soldeer install`
  - `forge soldeer update`
  - `forge build`
  - `forge test`

## Development Workflow

When fixing failures, identify root cause first, then apply idiomatic fixes instead of suppressing warnings or patching symptoms.

Use outside-in development for behavior changes:

- **Git Restrictions:** NEVER use `git worktree`. All code modifications MUST be made directly on the current branch in the existing working directory.
- start with a failing crate-local unit or integration test that names the expected behavior,
- drive implementation with failing crate-local unit tests and `proptest` properties in the affected crate,
- keep `proptest` in the normal `cargo test` loop instead of creating a separate property-test command,
- treat `cargo-fuzz` as conditional planning work rather than baseline template setup.

After each feature or bug fix, run:

```bash
just format
just lint
just test
```

If any command fails, report the failure and do not claim completion.

## Testing Requirements

- Unit tests: colocate with implementation (`#[cfg(test)]`).
- Prefer example-based unit tests for named business cases and edge cases, and reserve `proptest` for invariants that should hold across many generated inputs.
- Property tests: colocate `proptest` coverage with the crate logic it exercises so it runs through the ordinary `cargo test` path.
- Benchmarks: only plan or add Criterion when the scope includes an explicit latency SLA, throughput target, or known hot path in a specific crate.
- Benchmark workflow: when benchmarking is justified, add Criterion only in the affected crate instead of pre-seeding benchmark scaffolding across the workspace.
- Fuzz tests: only plan or add `cargo-fuzz` when a crate parses hostile input, implements protocols, decodes binary formats, or contains meaningful `unsafe` code.
- Fuzz workflow: when fuzzing is justified, generate the standard layout in the affected crate with `cargo fuzz init`, then run targets with `cargo fuzz run <target>` instead of pre-seeding a `fuzz/` directory in every starter.
- For `/pb-plan` work, mark benchmarking as `conditional` or `N/A` unless the scope explicitly includes a performance requirement or hot path, and mark fuzzing as `conditional` or `N/A` unless the scope explicitly includes parser-like, protocol, binary-decoding, or `unsafe`-heavy code.
- Integration tests: place in crate-level `tests/`.
- Add tests for behavioral changes and public API changes.

## Language Requirement

- Documentation, comments, and commit messages must be English only.
