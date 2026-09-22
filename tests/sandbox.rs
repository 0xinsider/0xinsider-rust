//! Live checks against https://0xinsider.com/sandbox: every operation's answer
//! decodes into its generated type, and the typed methods, errors and
//! pagination work end to end. The sandbox needs no credential and serves no
//! production data.
//!
//! These reach the network, so they run only with OXINSIDER_SANDBOX_TESTS=1:
//!
//!     OXINSIDER_SANDBOX_TESTS=1 cargo test --test sandbox

mod support;

use oxinsider::models::Grade;
use oxinsider::pagination::Pager;
use oxinsider::{ApiErrorKind, Client, Error, ListLeaderboardParams, ListWhaleTradesParams, Method, Operation};
use serde_json::Value;

fn enabled() -> bool {
    std::env::var("OXINSIDER_SANDBOX_TESTS").is_ok_and(|v| v == "1")
}

fn sandbox() -> Client {
    Client::sandbox().expect("sandbox client")
}

/// A value for a path or required query parameter the sandbox accepts.
fn sample(name: &str) -> &'static str {
    match name {
        "address" | "trader" => "swisstony",
        "condition_id" => "0x5f1c2d3e4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d",
        "id" | "delivery_id" | "job_id" => "1",
        "execution_id" | "snapshot_id" => "sample",
        "cohort" => "wider_holder",
        "date" | "period" => "2026-09-21",
        "granularity" => "daily",
        "month" => "2026-09",
        "q" => "nba",
        other => panic!("no sample value for the required parameter {other}"),
    }
}

fn required_params(doc: &Value, operation: &Operation) -> (String, Vec<(String, String)>) {
    let spec = &doc["paths"][operation.path][operation.method.to_ascii_lowercase()];
    let mut path = operation.path.to_owned();
    let mut query = Vec::new();
    for param in spec["parameters"].as_array().into_iter().flatten() {
        let name = param["name"].as_str().unwrap_or_default();
        match param["in"].as_str() {
            Some("path") => path = path.replace(&format!("{{{name}}}"), sample(name)),
            Some("query") if param["required"].as_bool() == Some(true) => {
                query.push((name.to_owned(), sample(name).to_owned()))
            }
            _ => {}
        }
    }
    (path, query)
}

#[tokio::test]
async fn every_json_read_decodes_into_its_generated_type() {
    if !enabled() {
        return;
    }
    let doc: Value =
        serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json")).unwrap()).unwrap();
    let client = sandbox();
    let mut failures = Vec::new();
    let mut checked = 0;
    for operation in oxinsider::OPERATIONS {
        if operation.method != "GET" || operation.accept != "application/json" || operation.id == "openMcpEventStream" {
            continue;
        }
        let (path, query) = required_params(&doc, operation);
        let query: Vec<(&str, &str)> = query.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        match client.request_json::<Value>(Method::GET, &path, &query, None).await {
            Ok(value) => match support::generated::decode_response(operation.id, value) {
                Some(Ok(())) => checked += 1,
                Some(Err(error)) => failures.push(format!("{} {path}: {error}", operation.id)),
                None => failures.push(format!("{}: no generated type", operation.id)),
            },
            Err(error) => failures.push(format!("{} {path}: {error}", operation.id)),
        }
    }
    assert!(
        failures.is_empty(),
        "{} sandbox answers failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked >= 40, "only {checked} operations were checked");
}

#[tokio::test]
async fn typed_methods_work_end_to_end() {
    if !enabled() {
        return;
    }
    let client = sandbox();
    let discovery = client.get_api_discovery().await.expect("discovery");
    assert_eq!(discovery.object, "api_discovery");
    let board = client
        .list_leaderboard(&ListLeaderboardParams::default().limit(5))
        .await
        .expect("leaderboard");
    assert!(board.data.len() <= 5);
    let trader = client
        .get_trader("swisstony", &Default::default())
        .await
        .expect("trader");
    assert!(!trader.data.address.is_empty());
    let explore = client.explore_markets(&Default::default()).await.expect("explore");
    let _ = explore.data.len();
    // The sandbox does not simulate Markdown, streams or downloads; it refuses
    // them with its own typed 400, which is what a text method must surface.
    let refused = client.get_trader_context_markdown("swisstony").await.unwrap_err();
    assert_eq!(refused.as_api().map(|api| api.kind()), Some(ApiErrorKind::BadRequest));
}

#[tokio::test]
async fn a_documented_error_comes_back_typed() {
    if !enabled() {
        return;
    }
    let client = sandbox();
    let error = client
        .request_json::<Value>(Method::GET, "/api/v1/leaderboard", &[("sandbox_status", "402")], None)
        .await
        .unwrap_err();
    let Error::Api(api) = error else {
        panic!("expected an API error, got {error:?}")
    };
    assert_eq!(api.kind(), ApiErrorKind::SubscriptionRequired);
    assert_eq!(api.code.as_deref(), Some("subscription_required"));
}

#[tokio::test]
async fn a_429_carries_retry_after_once_retries_are_spent() {
    if !enabled() {
        return;
    }
    let client = Client::builder().sandbox().max_retries(0).build().unwrap();
    let error = client
        .request_json::<Value>(Method::GET, "/api/v1/leaderboard", &[("sandbox_status", "429")], None)
        .await
        .unwrap_err();
    let api = error.as_api().expect("an API error");
    assert!(api.is_rate_limited());
    assert!(api.retry_after.is_some(), "the sandbox's 429 carries Retry-After");
}

#[tokio::test]
async fn pager_walks_a_list() {
    if !enabled() {
        return;
    }
    let client = sandbox();
    let mut pager = Pager::new(ListWhaleTradesParams::default().min_grade(Grade::A).limit(10));
    let mut pages = 0;
    while let Some(page) = pager
        .next_page(async |params| client.list_whale_trades(params).await)
        .await
        .unwrap()
    {
        pages += 1;
        let _ = page.data.len();
        if pages == 3 {
            break;
        }
    }
    assert!(pages >= 1);
}
