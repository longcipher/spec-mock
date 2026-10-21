# Default recipe to display help
default:
  @just --list

# Format markdown, TOML and Rust sources
format:
  rumdl fmt .
  taplo fmt
  cargo +nightly fmt --all

# Auto-fix what `format` cannot: markdown lints
fix:
  rumdl check --fix .

# TOML is formatted with `taplo`, not `cargo sort`: CI installs taplo-cli and
# `.taplo.toml` owns the dependency-key ordering for this workspace.
#
# Run all lints (every tool used here is installed by .github/workflows/ci.yml)
lint:
  typos
  rumdl check .
  taplo fmt --check
  cargo +nightly fmt --all -- --check
  cargo +nightly clippy --all -- -D warnings
  cargo machete

# Run the workspace test suite
test:
  cargo test --all-features

# Run tests with nextest (faster, per-test isolation)
test-nextest:
  cargo nextest run --workspace --all-features

# Run only unit tests (no integration suites)
test-unit:
  cargo test --workspace --lib --bins

# Run only integration/E2E suites
test-integration:
  cargo test --workspace --tests

# `cargo llvm-cov` is used rather than `cargo tarpaulin`: tarpaulin's bundled
# profdata parser rejects the profiles emitted by current nightly rustc
# ("consistency check for reading counts failed"), while llvm-cov shells out to
# the toolchain's own llvm-profdata and stays in step with it.
#
# Run tests with coverage
test-coverage:
  cargo llvm-cov --all-features --workspace --summary-only

# Emit an lcov coverage report for CI
coverage-report:
  cargo llvm-cov --all-features --workspace --lcov --output-path lcov.info

# ============================================================================
# Fuzzing
# ============================================================================
# `cargo fuzz` targets live in `crates/<crate>/fuzz` and are intentionally kept
# out of `cargo test`: they need nightly, a sanitizer build, and a corpus.

# Run every fuzz target for `seconds` seconds each
fuzz seconds="60":
  #!/usr/bin/env bash
  set -euo pipefail
  for target_dir in crates/*/fuzz; do
    [ -d "$target_dir" ] || continue
    crate="$(basename "$(dirname "$target_dir")")"
    for target in $(cargo fuzz list --fuzz-dir "$target_dir"); do
      echo "==> fuzzing $crate/$target for {{seconds}}s"
      cargo fuzz run --fuzz-dir "$target_dir" "$target" -- -max_total_time="{{seconds}}"
    done
  done

# Short fuzz run suitable for CI (catches obvious panics, not deep bugs)
fuzz-smoke seconds="10":
  #!/usr/bin/env bash
  set -euo pipefail
  for target_dir in crates/*/fuzz; do
    [ -d "$target_dir" ] || continue
    crate="$(basename "$(dirname "$target_dir")")"
    for target in $(cargo fuzz list --fuzz-dir "$target_dir"); do
      echo "==> smoke-fuzzing $crate/$target for {{seconds}}s"
      cargo fuzz run --fuzz-dir "$target_dir" "$target" -- -max_total_time="{{seconds}}" -runs=20000
    done
  done

# Rebuild the fuzz targets without executing them
fuzz-build:
  #!/usr/bin/env bash
  set -euo pipefail
  for target_dir in crates/*/fuzz; do
    [ -d "$target_dir" ] || continue
    echo "==> building fuzz targets in $target_dir"
    cargo fuzz build --fuzz-dir "$target_dir"
  done

# Run Prism comparison integration tests (requires Prism: npm i -g @stoplight/prism-cli)
integration-test:
  cargo test --test prism_comparison --features integration-test -- --nocapture --ignored

# Build entire workspace
build:
  cargo build --workspace

# Check all targets compile
check:
  cargo check --all-targets --all-features

# Publish all crates to crates.io (dry run)
publish-check:
  cargo publish --workspace --dry-run --allow-dirty

# Publish all crates to crates.io (irreversible: run only with a registry token)
publish:
  cargo publish --workspace

# `faker.rs`, `http/media.rs` and `grpc/protobuf.rs` carry CJK test data for
# IRI and Unicode handling, and `specs/` is a historical plan archive.
#
# Fail if Chinese characters are present in tracked files
check-cn:
  #!/usr/bin/env bash
  set -euo pipefail
  if rg --line-number --column "\p{Han}" \
      --glob '!specs/**' \
      --glob '!crates/specmock-core/src/faker.rs' \
      --glob '!crates/specmock-runtime/src/http/media.rs' \
      --glob '!crates/specmock-runtime/src/grpc/protobuf.rs' \
      . ; then
    echo "error: Chinese characters found" >&2
    exit 1
  fi
  echo "ok: no Chinese characters found"

# Full CI check
ci: lint test build

# Full CI check including a fuzz smoke run
ci-fuzz: ci fuzz-smoke

# ============================================================
# Maintenance & Tools
# ============================================================

# Clean build artifacts
clean:
  cargo clean

# Install all required development tools
setup:
  cargo install cargo-machete
  cargo install taplo-cli
  cargo install typos-cli
  cargo install rumdl
  cargo install cargo-nextest
  cargo install cargo-fuzz
  cargo install cargo-llvm-cov
  cargo install cargo-audit
  cargo install cargo-outdated

# Generate documentation for the workspace
docs:
  cargo doc --no-deps --open

# Run security audit
audit:
  cargo audit

# Check for outdated dependencies
outdated:
  cargo outdated

# Run clippy with all lints
clippy-all:
  cargo +nightly clippy --all-targets --all-features -- -W clippy::pedantic -W clippy::nursery -W clippy::cargo

# Check for unused dependencies
unused-deps:
  cargo machete

# Update all dependencies
update-deps:
  cargo update

# Full check (lint + test + build + audit)
full-check: lint test build audit
