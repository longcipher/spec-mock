//! End-to-end tests for scenario-driven stateful mocking.
//!
//! These drive a real server over HTTP, because the behaviour worth testing is
//! exactly what a client sees: a resource created in one request coming back in
//! the next, state surviving across requests, and failures arriving in a shape
//! the client can act on.

use std::{net::SocketAddr, path::PathBuf};

use serde_json::{Value, json};
use specmock_core::MockMode;
use specmock_runtime::{RunningServer, RuntimeError, ServerConfig, start};

fn spec(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("specs").join(name)
}

fn store_config() -> ServerConfig {
    ServerConfig {
        openapi_spec: Some(spec("openapi-store.yaml")),
        scenarios_spec: Some(spec("openapi-store.scenarios.yaml")),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    }
}

async fn boot(config: ServerConfig) -> Result<Option<RunningServer>, Box<dyn std::error::Error>> {
    match start(config).await {
        Ok(server) => Ok(Some(server)),
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(None)
        }
        Err(error) => Err(error.to_string().into()),
    }
}

/// Read a response into `(status, headers, parsed body)`.
async fn read(
    response: hpx::Response,
) -> Result<(u16, hpx::header::HeaderMap, Value), Box<dyn std::error::Error>> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.bytes().await?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    Ok((status, headers, body))
}

#[tokio::test]
async fn a_created_resource_comes_back_on_the_next_request()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let created = read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "First"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(created.0, 201);
    assert_eq!(created.2["id"], "w1");

    let fetched = read(hpx::Client::new().get(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(fetched.0, 200);
    assert_eq!(
        fetched.2,
        json!({"id": "w1", "name": "First"}),
        "the resource created a moment ago must be returned, not regenerated"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_update_replaces_the_stored_resource_and_a_delete_removes_it()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "Original"}).to_string())
            .send()
            .await?,
    )
    .await?;

    let updated = read(
        hpx::Client::new()
            .put(format!("{base}/widgets/w1"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "Renamed"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(updated.0, 200);

    let after = read(hpx::Client::new().get(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(after.2["name"], "Renamed");

    let deleted =
        read(hpx::Client::new().delete(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(deleted.0, 204);

    let gone = read(hpx::Client::new().get(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(gone.0, 404, "a deleted resource stays deleted");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn reading_an_absent_resource_reports_404_and_updating_one_is_not_an_upsert()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let missing =
        read(hpx::Client::new().get(format!("{base}/widgets/ghost")).send().await?).await?;
    assert_eq!(missing.0, 404);

    let (status, _, body) = read(
        hpx::Client::new()
            .put(format!("{base}/widgets/ghost"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "ghost", "name": "Ghost"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(
        status, 404,
        "update must not create: a test that updates what it never created has a bug, and \
         upserting would let it pass green"
    );
    assert_eq!(body["status"], 404);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn listing_returns_the_store_in_creation_order_and_an_empty_array_when_empty()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let (status, _, body) =
        read(hpx::Client::new().get(format!("{base}/widgets")).send().await?).await?;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!([{"id": "seeded-1", "name": "Preloaded Widget"}]),
        "the catalogue scenario pre-seeds exactly one widget"
    );

    read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "First"}).to_string())
            .send()
            .await?,
    )
    .await?;

    let (_, _, listed) =
        read(hpx::Client::new().get(format!("{base}/widgets")).send().await?).await?;
    let ids: Vec<&str> =
        listed.as_array().expect("array").iter().filter_map(|w| w["id"].as_str()).collect();
    assert_eq!(
        ids,
        ["seeded-1", "w1"],
        "a created resource lands after the pre-seeded one, in creation order"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_pre_seeded_resource_is_readable_without_creating_it_first()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let (status, _, body) =
        read(hpx::Client::new().get(format!("{base}/widgets/seeded-1")).send().await?).await?;
    assert_eq!(status, 200);
    assert_eq!(body["name"], "Preloaded Widget");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_created_resource_reports_a_working_location_header()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let (status, headers, _) = read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w9", "name": "Ninth"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(status, 201);

    let location = headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or("a declared Location header must be emitted")?;

    let followed = read(hpx::Client::new().get(format!("{base}{location}")).send().await?).await?;
    assert_eq!(
        followed.0, 200,
        "Location must name a resource that resolves, not a plausible-looking string: {location}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn sessions_isolate_their_stores_from_each_other() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let create = |session: &'static str, id: &'static str| {
        let base = base.clone();
        async move {
            let response = hpx::Client::new()
                .post(format!("{base}/widgets"))
                .header("Prefer", format!("session={session}"))
                .header("Content-Type", "application/json")
                .body(json!({"id": id, "name": id}).to_string())
                .send()
                .await?;
            read(response).await
        }
    };
    create("alpha", "shared-id").await?;
    create("beta", "shared-id").await?;

    let list = |session: &'static str| {
        let base = base.clone();
        async move {
            let response = hpx::Client::new()
                .get(format!("{base}/widgets"))
                .header("Prefer", format!("session={session}"))
                .send()
                .await?;
            read(response).await
        }
    };
    let (_, _, alpha) = list("alpha").await?;
    let (_, _, beta) = list("beta").await?;

    assert_eq!(alpha.as_array().map(Vec::len), Some(2), "alpha sees only what alpha created");
    assert_eq!(beta.as_array().map(Vec::len), Some(2), "beta sees only what beta created");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_state_specific_response_reports_its_declared_status()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let first = read(
        hpx::Client::new()
            .get(format!("{base}/gadgets/fragile-1"))
            .header("Prefer", "scenario=fragile")
            .send()
            .await?,
    )
    .await?;
    assert_eq!(first.0, 503, "the `failing` state declares a 503");
    assert_eq!(first.2["error"], "unavailable");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_operation_illegal_in_the_current_state_reports_409_with_context()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig {
        openapi_spec: Some(spec("openapi-store.yaml")),
        scenarios_spec: Some({
            let mut path = spec("openapi-store.scenarios.yaml");
            path.set_file_name("scenarios-one-shot.yaml");
            path
        }),
        mode: MockMode::Mock,
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..ServerConfig::default()
    };
    // A scenario where `getWidget` may fire exactly once, so a second read is
    // refused by the machine.
    let one_shot = config.scenarios_spec.clone().expect("path");
    std::fs::write(
        &one_shot,
        r#"version: 1
operations:
  getWidget:
    match: { method: GET, path: "/widgets/{id}" }
    store: { resource: /widgets, action: read, key: "{id}" }
    responses: { "200": {} }
scenarios:
  - name: once
    initialState: first
    states:
      first:
        getWidget: { to: spent }
      spent: {}
"#,
    )
    .map_err(|e| e.to_string())?;

    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let (first_status, _, _) =
        read(hpx::Client::new().get(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(first_status, 404, "the store is empty, but the machine advanced");

    let (second_status, _, body) =
        read(hpx::Client::new().get(format!("{base}/widgets/w1")).send().await?).await?;
    assert_eq!(second_status, 409, "a second read is not legal in `spent`");
    assert_eq!(body["state"], "spent");
    assert_eq!(body["operation"], "getWidget");
    assert_eq!(body["scenario"], "once");
    assert!(
        body.get("errors").is_none(),
        "a state-machine refusal is not a schema violation, so it carries no validation issues"
    );

    let _ = std::fs::remove_file(&one_shot);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_unknown_scenario_name_reports_404_and_lists_the_real_ones()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let (status, _, body) = read(
        hpx::Client::new()
            .get(format!("{base}/widgets/seeded-1"))
            .header("Prefer", "scenario=ghost")
            .send()
            .await?,
    )
    .await?;
    assert_eq!(status, 404);
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("ghost"), "got {detail}");
    assert!(detail.contains("catalogue"), "the error must list what exists: {detail}");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn prefer_code_beats_the_status_the_current_state_declares()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    // Without the header the `healthy` state answers 503.
    let (status, _, _) = read(
        hpx::Client::new()
            .get(format!("{base}/gadgets/fragile-1"))
            .header("Prefer", "scenario=fragile")
            .send()
            .await?,
    )
    .await?;
    assert_eq!(status, 503);

    // A fresh session, with an explicit status: the header wins (ADR 0001).
    let (status, _, _) = read(
        hpx::Client::new()
            .get(format!("{base}/gadgets/fragile-1"))
            .header("Prefer", "scenario=fragile, code=200, session=explicit")
            .send()
            .await?,
    )
    .await?;
    assert_eq!(
        status, 200,
        "`Prefer: code=` must short-circuit the state machine rather than defer to it"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_invalid_request_leaves_the_store_untouched() -> Result<(), Box<dyn std::error::Error>> {
    let Some(server) = boot(store_config()).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "Valid"}).to_string())
            .send()
            .await?,
    )
    .await?;

    // Missing the required `name`, so the body must not reach the store.
    let invalid = read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w2"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(invalid.0, 400);

    let (_, _, listed) =
        read(hpx::Client::new().get(format!("{base}/widgets")).send().await?).await?;
    let ids: Vec<&str> =
        listed.as_array().expect("array").iter().filter_map(|w| w["id"].as_str()).collect();
    assert_eq!(
        ids,
        ["seeded-1", "w1"],
        "a rejected request must not leave a partial resource behind: {ids:?}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_session_cap_is_reported_rather_than_recycled() -> Result<(), Box<dyn std::error::Error>>
{
    let config = ServerConfig { max_sessions: 1, ..store_config() };
    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    read(
        hpx::Client::new()
            .get(format!("{base}/widgets/seeded-1"))
            .header("Prefer", "session=alpha")
            .send()
            .await?,
    )
    .await?;

    let (status, _, body) = read(
        hpx::Client::new()
            .get(format!("{base}/widgets/seeded-1"))
            .header("Prefer", "session=beta")
            .send()
            .await?,
    )
    .await?;
    assert_eq!(status, 429);
    assert!(
        body["detail"].as_str().unwrap_or_default().contains("max-sessions"),
        "the error must name the knob to raise: {body}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scenarios_combined_with_proxy_mode_fail_startup() -> Result<(), Box<dyn std::error::Error>>
{
    let config = ServerConfig {
        mode: MockMode::Proxy,
        upstream: Some("http://example.invalid".to_owned()),
        ..store_config()
    };

    let error = start(config).await.err().ok_or("startup should fail")?;
    let message = error.to_string();
    assert!(message.contains("--scenarios"), "got {message}");
    assert!(message.contains("proxy"), "the error must name both settings: {message}");

    Ok(())
}

#[tokio::test]
async fn a_malformed_sidecar_fails_startup_with_a_pointer() -> Result<(), Box<dyn std::error::Error>>
{
    let bad = spec("openapi-store.yaml").with_file_name("scenarios-broken.yaml");
    std::fs::write(
        &bad,
        "version: 1\nscenarios:\n  - name: s\n    initialState: absent\n    states:\n      other: {}\n",
    )
    .map_err(|e| e.to_string())?;

    let config = ServerConfig { scenarios_spec: Some(bad.clone()), ..store_config() };
    let error = start(config).await.err().ok_or("startup should fail")?;
    assert!(
        error.to_string().contains("absent"),
        "the error must name the undeclared state: {error}"
    );

    let _ = std::fs::remove_file(&bad);
    Ok(())
}

#[tokio::test]
async fn an_absent_sidecar_leaves_plain_mocking_untouched() -> Result<(), Box<dyn std::error::Error>>
{
    // The regression guard for the whole feature: with no sidecar, every request
    // is still served exactly as it was before scenarios existed.
    let config = ServerConfig {
        openapi_spec: Some(spec("openapi-store.yaml")),
        scenarios_spec: None,
        ..store_config()
    };
    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    // Every response is freshly generated, so nothing is remembered.
    let first =
        read(hpx::Client::new().get(format!("{base}/widgets/anything")).send().await?).await?;
    assert_eq!(
        first.0, 200,
        "with no store the operation is served by ordinary generation, not by a 404"
    );

    let created = read(
        hpx::Client::new()
            .post(format!("{base}/widgets"))
            .header("Content-Type", "application/json")
            .body(json!({"id": "w1", "name": "First"}).to_string())
            .send()
            .await?,
    )
    .await?;
    assert_eq!(created.0, 201);

    let (_, _, listed) =
        read(hpx::Client::new().get(format!("{base}/widgets")).send().await?).await?;
    assert!(
        !listed.as_array().is_some_and(|items| items.iter().any(|item| item["id"] == "w1")),
        "a collection read must be generated afresh, not served from a store: {listed}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn the_control_endpoints_require_the_bearer_token() -> Result<(), Box<dyn std::error::Error>>
{
    let config = ServerConfig { control_token: Some("test-token".to_owned()), ..store_config() };
    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let denied = hpx::Client::new().get(format!("{base}/_specmock/state")).send().await?;
    assert_eq!(denied.status().as_u16(), 401, "an unguarded reset must not be reachable");

    let wrong = hpx::Client::new()
        .get(format!("{base}/_specmock/state"))
        .header("Authorization", "Bearer nope")
        .send()
        .await?;
    assert_eq!(wrong.status().as_u16(), 401);

    let allowed = hpx::Client::new()
        .get(format!("{base}/_specmock/state"))
        .header("Authorization", "Bearer test-token")
        .send()
        .await?;
    assert_eq!(allowed.status().as_u16(), 200);

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn resetting_through_the_control_endpoint_returns_instances_to_their_initial_state()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { control_token: Some("t".to_owned()), ..store_config() };
    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    hpx::Client::new()
        .get(format!("{base}/gadgets/fragile-1"))
        .header("Prefer", "scenario=fragile, session=alpha")
        .send()
        .await?;

    let reset = hpx::Client::new()
        .post(format!("{base}/_specmock/reset"))
        .header("Authorization", "Bearer t")
        .send()
        .await?;
    assert_eq!(reset.status().as_u16(), 200);

    let (_, _, body) = read(
        hpx::Client::new()
            .get(format!("{base}/_specmock/state"))
            .header("Authorization", "Bearer t")
            .send()
            .await?,
    )
    .await?;
    let states: Vec<String> = body["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|entry| entry["state"].as_str().map(str::to_owned))
        .collect();
    assert!(
        states.iter().all(|state| state == "browsing" || state == "healthy"),
        "every instance must be back at its initial state after reset: {body}"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn control_endpoints_are_absent_unless_a_token_is_configured()
-> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig { control_token: None, ..store_config() };
    let Some(server) = boot(config).await? else {
        return Ok(());
    };
    let base = format!("http://{}", server.http_addr);

    let response = hpx::Client::new().get(format!("{base}/_specmock/state")).send().await?;
    assert_eq!(
        response.status().as_u16(),
        404,
        "with no token configured the control surface should not be registered at all"
    );

    server.shutdown().await;
    Ok(())
}
