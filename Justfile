# Default recipe to display help
default:
  @just --list

# Format all code
format:
  rumdl fmt .
  taplo fmt
  cargo +nightly fmt --all

# Auto-fix linting issues
fix:
  rumdl check --fix .

# Run all lints
lint:
  typos
  rumdl check .
  taplo fmt --check
  cargo +nightly fmt --all -- --check
  cargo +nightly clippy --all -- -D warnings
  cargo machete

# Run tests
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

# Run tests with coverage
#
# `cargo llvm-cov` is used rather than `cargo tarpaulin`: tarpaulin's bundled
# profdata parser rejects the profiles emitted by current nightly rustc
# ("consistency check for reading counts failed"), while llvm-cov shells out to
# the toolchain's own llvm-profdata and stays in step with it.
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

# Publish all crates to crates.io
publish:
  cargo publish --workspace

# Check for Chinese characters
check-cn:
  rg --line-number --column "\p{Han}"

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
  cargo install cargo-nextest
  cargo install cargo-fuzz
  cargo install cargo-llvm-cov

# Generate documentation for the workspace
docs:
  cargo doc --no-deps --open
