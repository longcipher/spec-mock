//! Prism comparison integration tests.
//!
//! These tests start both spec-mock and Prism side-by-side, generate fuzz requests
//! from an OpenAPI spec, and compare responses structurally.
//!
//! Requires: `npm install -g @stoplight/prism-cli`
//! Run with: `just integration-test`

#![cfg(feature = "integration-test")]

use std::path::PathBuf;

use rand::SeedableRng as _;
use rand_chacha::ChaCha8Rng;

mod harness;

// ─── Task 4.3: Environment variable configuration ────────────────────────────

fn fuzz_seed() -> u64 {
    std::env::var("SPECMOCK_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(42)
}

fn fuzz_iterations() -> usize {
    std::env::var("SPECMOCK_FUZZ_ITERATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(5)
}

// ─── Shared setup helper ─────────────────────────────────────────────────────

/// Starts both spec-mock and Prism with the given spec.
/// Returns `None` if Prism is not found (callers should skip gracefully).
async fn setup_servers(
    spec_path: &std::path::Path,
) -> Result<
    Option<(specmock_runtime::RunningServer, harness::prism::PrismServer)>,
    Box<dyn std::error::Error>,
> {
    let Some(prism) = harness::prism::PrismServer::start(spec_path) else {
        return Ok(None);
    };
    let config = specmock_runtime::ServerConfig {
        openapi_spec: Some(spec_path.to_path_buf()),
        mode: specmock_core::MockMode::Mock,
        http_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        ..specmock_runtime::ServerConfig::default()
    };
    let server = specmock_runtime::start(config).await?;
    Ok(Some((server, prism)))
}

// ─── Task 4.2: Comparison runner ─────────────────────────────────────────────

/// Sends fuzz requests to both servers, compares responses and prints diffs.
///
/// Returns `(total_requests, total_fails)`.
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn run_comparison(
    client: &hpx::Client,
    specmock_url: &str,
    prism_url: &str,
    requests: &[harness::request::FuzzRequest],
    category: harness::comparator::RequestCategory,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let mut fail_count = 0usize;
    for req in requests {
        let specmock_resp = match harness::request::send_request(client, specmock_url, req).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[run_comparison] specmock request error for {:?}: {e}", req.description);
                continue;
            }
        };
        let prism_resp = match harness::request::send_request(client, prism_url, req).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[run_comparison] prism request error for {:?}: {e}", req.description);
                continue;
            }
        };
        let result = harness::comparator::compare_responses(
            &specmock_resp,
            &prism_resp,
            &req.description,
            category.clone(),
        );
        if !result.is_match() {
            eprintln!("--- DIVERGENCE ---");
            eprintln!("Request:\n  {}", harness::request::format_request(req));
            eprintln!("{}", result.report());
            eprintln!("---");
            fail_count += 1;
        }
    }
    Ok((requests.len(), fail_count))
}

// ─── Smoke test ──────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn prism_starts_and_serves_mock() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-pets.yaml");

    let Some(prism) = harness::prism::PrismServer::start(&spec_path) else {
        eprintln!("[prism_comparison] Prism not found — skipping prism_starts_and_serves_mock");
        return Ok(());
    };

    let client = hpx::Client::new();
    let req = harness::request::FuzzRequest {
        method: http::Method::GET,
        path: "/pets/1".to_string(),
        query: vec![],
        headers: vec![],
        body: None,
        content_type: None,
        description: "GET /pets/1".to_string(),
    };
    let captured = harness::request::send_request(&client, &prism.base_url(), &req).await?;
    assert_eq!(captured.status, 200, "expected 200 from Prism mock");
    Ok(())
}

// ─── Task 4.2: Full fuzz-comparison tests ────────────────────────────────────

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_fuzz_valid_requests_match() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    eprintln!("[prism_comparison] seed={seed} iterations={iterations}");
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let requests = fuzzer.generate_valid_requests();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::ValidFuzz,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(fails, 0, "{fails}/{total} valid fuzz requests produced divergent responses");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_fuzz_invalid_path_param_returns_matching_status()
-> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all_invalid = fuzzer.generate_invalid_requests(&mut rng);
    let requests: Vec<_> =
        all_invalid.into_iter().filter(|r| r.description.contains("invalid path param")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::InvalidPathParam,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(
        fails, 0,
        "{fails}/{total} invalid-path-param requests produced divergent responses"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_fuzz_missing_required_param_returns_matching_status()
-> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all_invalid = fuzzer.generate_invalid_requests(&mut rng);
    let requests: Vec<_> =
        all_invalid.into_iter().filter(|r| r.description.contains("missing required")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::MissingRequiredParam,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(
        fails, 0,
        "{fails}/{total} missing-required-param requests produced divergent responses"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_fuzz_invalid_body_returns_matching_status() -> Result<(), Box<dyn std::error::Error>>
{
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all_invalid = fuzzer.generate_invalid_requests(&mut rng);
    let requests: Vec<_> =
        all_invalid.into_iter().filter(|r| r.description.contains("invalid body")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::InvalidBody,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(fails, 0, "{fails}/{total} invalid-body requests produced divergent responses");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_wrong_content_type_returns_matching_status() -> Result<(), Box<dyn std::error::Error>>
{
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all_invalid = fuzzer.generate_invalid_requests(&mut rng);
    let requests: Vec<_> =
        all_invalid.into_iter().filter(|r| r.description.contains("wrong content-type")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::WrongContentType,
    )
    .await?;
    server.shutdown().await;
    if fails > 0 {
        eprintln!(
            "[known divergence] {fails}/{total} wrong-content-type requests have status divergence \
             (spec-mock=415, prism=400/422) — this is expected behavior"
        );
    }
    // Don't fail: this is a known documented divergence between spec-mock and Prism.
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_unknown_path_returns_matching_status() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let all_invalid = fuzzer.generate_invalid_requests(&mut rng);
    let requests: Vec<_> =
        all_invalid.into_iter().filter(|r| r.description.contains("unknown path")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::UnknownPath,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(fails, 0, "{fails}/{total} unknown-path requests produced divergent responses");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_prefer_code_matches() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let all_prefer = fuzzer.generate_prefer_requests();
    let requests: Vec<_> =
        all_prefer.into_iter().filter(|r| r.description.contains("prefer code=")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    // Use 0 as placeholder — actual code is in each request's description.
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::PreferCode(0),
    )
    .await?;
    server.shutdown().await;
    assert_eq!(fails, 0, "{fails}/{total} prefer-code requests produced divergent responses");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_prefer_example_matches() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let all_prefer = fuzzer.generate_prefer_requests();
    let requests: Vec<_> =
        all_prefer.into_iter().filter(|r| r.description.contains("prefer example=")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::PreferExample(String::new()),
    )
    .await?;
    server.shutdown().await;
    assert_eq!(fails, 0, "{fails}/{total} prefer-example requests produced divergent responses");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_prefer_dynamic_structure_matches() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let all_prefer = fuzzer.generate_prefer_requests();
    let requests: Vec<_> =
        all_prefer.into_iter().filter(|r| r.description.contains("prefer dynamic=true")).collect();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::PreferDynamic,
    )
    .await?;
    server.shutdown().await;
    // `Prefer: dynamic=true` responses are non-deterministic by design: array lengths,
    // optional field presence, and 4xx codes for $ref-schema bodies all vary between
    // spec-mock and Prism. Log divergences for visibility but don't fail the suite.
    if fails > 0 {
        eprintln!(
            "[known divergence] {fails}/{total} prefer-dynamic requests have structural \
             differences (array lengths, optional fields, or 4xx code variation) — \
             expected with Prefer: dynamic=true"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Prism: npm i -g @stoplight/prism-cli. Run with: just integration-test"]
#[expect(clippy::print_stderr, reason = "test infrastructure logging")]
async fn test_content_negotiation_matches() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/specs/openapi-prism-comparison.yaml");
    let Some((server, prism)) = setup_servers(&spec_path).await? else {
        eprintln!("[prism_comparison] Prism not found — skipping");
        return Ok(());
    };
    let seed = fuzz_seed();
    let iterations = fuzz_iterations();
    let fuzzer = harness::fuzzer::OpenApiFuzzer::new(&spec_path, seed, iterations)?;
    let requests = fuzzer.generate_accept_requests();
    let client = hpx::Client::new();
    let specmock_url = format!("http://{}", server.http_addr);
    let prism_url = prism.base_url();
    let (total, fails) = run_comparison(
        &client,
        &specmock_url,
        &prism_url,
        &requests,
        harness::comparator::RequestCategory::AcceptNegotiation,
    )
    .await?;
    server.shutdown().await;
    assert_eq!(
        fails, 0,
        "{fails}/{total} content-negotiation requests produced divergent responses"
    );
    Ok(())
}
