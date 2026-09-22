//! The request path against a scripted local server: what goes on the wire,
//! retries, conditional requests, error decoding, the event stream, and the
//! credential-free download hop.

mod support;

use std::time::Duration;

use oxinsider::models::{GetHealthResponseDataStatus, Grade, WebhookEventType};
use oxinsider::{
    Client, CreateWebhookParams, DownloadErrorReason, Error, GetTraderParams, ListLeaderboardParams, StreamOptions,
    StreamProtocolErrorReason,
};
use serde_json::{Value, json};
use support::server::{Reply, Server};

const KEY: &str = "oxi_sk_live_test_key";

/// The first documented 2xx example for an operation.
fn example(operation_id: &str) -> Value {
    let doc: Value =
        serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json")).unwrap()).unwrap();
    for item in doc["paths"].as_object().unwrap().values() {
        for operation in item.as_object().unwrap().values() {
            if operation.get("operationId").and_then(Value::as_str) != Some(operation_id) {
                continue;
            }
            for (code, response) in operation["responses"].as_object().unwrap() {
                let Some(media) = response.pointer("/content/application~1json") else {
                    continue;
                };
                if !code.starts_with('2') {
                    continue;
                }
                if let Some(example) = media.get("example") {
                    return example.clone();
                }
                if let Some(example) = media
                    .get("examples")
                    .and_then(Value::as_object)
                    .and_then(|e| e.values().next())
                {
                    return example["value"].clone();
                }
            }
        }
    }
    panic!("no 2xx example for {operation_id}");
}

fn client(server: &Server) -> Client {
    Client::builder()
        .api_key(KEY)
        .base_url(&server.base_url)
        .build()
        .unwrap()
}

fn error_body(code: &str, reason: Option<&str>) -> Value {
    json!({
        "object": "error",
        "error": { "code": code, "message": "scripted", "reason": reason, "param": "limit", "doc_url": null },
        "meta": { "request_id": "req_scripted" }
    })
}

#[tokio::test]
async fn sends_the_credential_user_agent_query_and_encoded_path() {
    let server = Server::start(vec![
        Reply::json(200, &example("listLeaderboard")),
        Reply::json(200, &example("getTrader")),
    ])
    .await;
    let client = client(&server);
    client
        .list_leaderboard(&ListLeaderboardParams::default().limit(10).category("NBA Finals"))
        .await
        .unwrap();
    let params =
        GetTraderParams::default().expand([oxinsider::models::Expand::Strategy, oxinsider::models::Expand::Trust]);
    client.get_trader("a/b", &params).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/api/v1/leaderboard?limit=10&category=NBA+Finals");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(requests[0].header("user-agent").unwrap().starts_with("0xinsider-rust/"));
    assert_eq!(requests[0].header("accept"), Some("application/json"));
    assert_eq!(requests[1].target, "/api/v1/trader/a%2Fb?expand=strategy&expand=trust");
}

#[tokio::test]
async fn a_keyless_client_sends_no_authorization() {
    let server = Server::start(vec![Reply::json(200, &example("getHealth"))]).await;
    let client = Client::builder().base_url(&server.base_url).build().unwrap();
    client.get_health(&Default::default()).await.unwrap();
    assert_eq!(server.requests()[0].header("authorization"), None);
}

#[tokio::test]
async fn retries_a_429_after_its_retry_after() {
    let server = Server::start(vec![
        Reply::json(429, &error_body("rate_limited", None)).header("Retry-After", "0"),
        Reply::json(200, &example("listLeaderboard")),
    ])
    .await;
    client(&server).list_leaderboard(&Default::default()).await.unwrap();
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn returns_a_retry_after_past_the_ceiling_instead_of_waiting() {
    let server = Server::start(vec![
        Reply::json(429, &error_body("rate_limited", None)).header("Retry-After", "3600"),
    ])
    .await;
    let error = client(&server).list_leaderboard(&Default::default()).await.unwrap_err();
    let api = error.as_api().unwrap();
    assert_eq!(api.retry_after, Some(Duration::from_secs(3600)));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn never_retries_a_write_without_an_idempotency_key() {
    let server = Server::start(vec![
        Reply::json(503, &error_body("internal_error", None)).header("Retry-After", "0"),
        Reply::json(200, &example("createWebhook")),
    ])
    .await;
    let body = webhook_body();
    let error = client(&server)
        .create_webhook(&body, &CreateWebhookParams::default())
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn retries_a_keyed_write_with_the_same_key_and_bytes() {
    let server = Server::start(vec![
        Reply::json(503, &error_body("internal_error", None)).header("Retry-After", "0"),
        Reply::json(200, &example("createWebhook")),
    ])
    .await;
    let body = webhook_body();
    let params = CreateWebhookParams::default().idempotency_key("idem-1");
    client(&server).create_webhook(&body, &params).await.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("idempotency-key"), Some("idem-1"));
    assert_eq!(requests[1].header("idempotency-key"), Some("idem-1"));
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["event_types"], json!(["whale_trades_inserted"]));
}

fn webhook_body() -> oxinsider::models::CreateWebhookRequest {
    serde_json::from_value(json!({
        "name": "Large trades",
        "url": "https://example.com/hooks/0xinsider",
        "event_types": [WebhookEventType::WhaleTradesInserted]
    }))
    .unwrap()
}

#[tokio::test]
async fn a_304_is_not_modified_with_its_etag() {
    let server = Server::start(vec![Reply::new(304).header("ETag", "W/\"abc\"")]).await;
    let params = GetTraderParams::default().if_none_match("W/\"abc\"");
    let error = client(&server).get_trader("swisstony", &params).await.unwrap_err();
    assert!(matches!(&error, Error::NotModified { etag: Some(etag) } if etag == "W/\"abc\""));
    assert_eq!(server.requests()[0].header("if-none-match"), Some("W/\"abc\""));
}

#[tokio::test]
async fn an_error_envelope_is_decoded() {
    let server = Server::start(vec![Reply::json(
        404,
        &error_body("not_found", Some("trader_not_tracked")),
    )])
    .await;
    let error = client(&server)
        .get_trader("nobody", &Default::default())
        .await
        .unwrap_err();
    let api = error.as_api().unwrap();
    assert_eq!(api.kind(), oxinsider::ApiErrorKind::NotFound);
    assert_eq!(api.code.as_deref(), Some("not_found"));
    assert_eq!(api.reason.as_deref(), Some("trader_not_tracked"));
    assert_eq!(api.param.as_deref(), Some("limit"));
    assert_eq!(api.request_id.as_deref(), Some("req_scripted"));
    assert!(!error.to_string().contains(KEY));
}

#[tokio::test]
async fn the_per_ip_limiter_shape_is_decoded_too() {
    let body = json!({ "error": "rate_limited", "message": "Too many requests" });
    let server = Server::start(vec![
        Reply::json(429, &body)
            .header("Retry-After", "120")
            .header("X-Request-Id", "req_ip"),
    ])
    .await;
    let error = client(&server).list_leaderboard(&Default::default()).await.unwrap_err();
    let api = error.as_api().unwrap();
    assert_eq!(api.code.as_deref(), Some("rate_limited"));
    assert_eq!(api.message, "Too many requests");
    assert_eq!(api.request_id.as_deref(), Some("req_ip"));
}

#[tokio::test]
async fn an_unknown_enum_value_is_kept_not_rejected() {
    let mut body = example("getHealth");
    body["data"]["status"] = json!("brand_new_state");
    let server = Server::start(vec![Reply::json(200, &body)]).await;
    let health = client(&server).get_health(&Default::default()).await.unwrap();
    let status = health.data.status.expect("the example reports a status");
    assert_eq!(status, GetHealthResponseDataStatus::Other("brand_new_state".to_owned()));
    assert_eq!(status.as_str(), "brand_new_state");
}

#[tokio::test]
async fn a_body_that_breaks_the_contract_names_the_field() {
    let mut body = example("listLeaderboard");
    body["has_more"] = json!("yes");
    let server = Server::start(vec![Reply::json(200, &body)]).await;
    let error = client(&server).list_leaderboard(&Default::default()).await.unwrap_err();
    let Error::Decode(decode) = error else {
        panic!("expected a decode error, got {error:?}")
    };
    assert_eq!(decode.operation, "listLeaderboard");
    assert_eq!(decode.path, "has_more");
}

#[tokio::test]
async fn the_stream_delivers_frames_and_stops_on_a_malformed_one() {
    let server = Server::start(vec![
        Reply::new(200)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .chunks([
                ": keep-alive\n\n",
                "id: 5\ndata: {\"seq\":5,\"type\":\"Whale",
                "TradesInserted\",\"published_at\":\"2026-09-22T00:00:00Z\"}\r\n\r\n",
                "event: resync\nid: 9\ndata: {\"type\":\"resync\",\"from_sequence\":6}\n\n",
                "id: 10\ndata: not json\n\n",
            ]),
    ])
    .await;
    let options = StreamOptions::default()
        .last_event_id(4)
        .events(["WhaleTradesInserted", "wallet_grade_changed"])
        .min_grade(Grade::A);
    let mut reader = client(&server).open_stream(&options).await.unwrap();

    let first = reader.next().await.unwrap().unwrap();
    assert!(!first.resync);
    assert_eq!(first.seq, Some(5));
    assert_eq!(first.event_type, "WhaleTradesInserted");
    assert_eq!(first.published_at.as_deref(), Some("2026-09-22T00:00:00Z"));

    let second = reader.next().await.unwrap().unwrap();
    assert!(second.resync);
    assert_eq!(second.seq, Some(9));
    assert_eq!(reader.last_seq(), Some(5), "a resync marker does not move the cursor");

    let error = reader.next().await.unwrap_err();
    let Error::Stream(protocol) = error else {
        panic!("expected a protocol error, got {error:?}")
    };
    assert_eq!(protocol.reason, StreamProtocolErrorReason::InvalidJson);
    assert_eq!(protocol.last_seq, Some(5));
    assert_eq!(protocol.frame_id, Some(10));
    assert!(reader.next().await.unwrap().is_none(), "every stream error is terminal");

    let request = &server.requests()[0];
    assert_eq!(request.header("last-event-id"), Some("4"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(
        request.target,
        "/api/v1/stream?event=WhaleTradesInserted%2Cwallet_grade_changed&min_grade=A"
    );
}

#[tokio::test]
async fn the_stream_bounds_one_frame() {
    let server = Server::start(vec![
        Reply::new(200).header("Content-Type", "text/event-stream").chunks([
            "data: {\"seq\":1,",
            "\"padding\":\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"}\n\n",
        ]),
    ])
    .await;
    let mut reader = client(&server)
        .open_stream(&StreamOptions::default().max_frame_bytes(64))
        .await
        .unwrap();
    let error = reader.next().await.unwrap_err();
    assert!(
        matches!(error, Error::Stream(ref e) if e.reason == StreamProtocolErrorReason::FrameTooLarge),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_stream_refuses_a_non_sse_answer() {
    let server = Server::start(vec![Reply::json(200, &json!({"ok": true}))]).await;
    let error = client(&server)
        .open_stream(&StreamOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Stream(ref e) if e.reason == StreamProtocolErrorReason::UnexpectedMediaType),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_stream_surfaces_a_refusal_as_an_api_error() {
    let server = Server::start(vec![
        Reply::json(429, &error_body("rate_limited", None)).header("Retry-After", "30"),
    ])
    .await;
    let error = client(&server)
        .open_stream(&StreamOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.as_api().unwrap().retry_after, Some(Duration::from_secs(30)));
    assert_eq!(
        server.requests().len(),
        1,
        "the stream is never retried behind your back"
    );
}

#[tokio::test]
async fn a_download_follows_one_hop_without_the_credential() {
    let files = Server::start(vec![
        Reply::new(200)
            .header("Content-Type", "application/x-ndjson")
            .header("Content-Disposition", "attachment; filename=\"swisstony.ndjson\"")
            .body("{\"a\":1}\n{\"a\":2}\n"),
    ])
    .await;
    let api = Server::start(vec![
        Reply::new(302).header("Location", &format!("{}/exports/signed?sig=secret", files.base_url)),
    ])
    .await;
    let download = client(&api).download_trader_export("swisstony", 42).await.unwrap();
    assert_eq!(download.content_type(), Some("application/x-ndjson"));
    assert_eq!(download.filename().as_deref(), Some("swisstony.ndjson"));
    assert!(!download.host().contains("sig=secret"));
    let path = std::env::temp_dir().join(format!("oxinsider-download-{}.ndjson", std::process::id()));
    let saved = download.save(&path).await.unwrap();
    assert_eq!(saved.bytes_written, 16);
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}\n{\"a\":2}\n");
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        api.requests()[0].target,
        "/api/v1/trader/swisstony/export/download?job_id=42"
    );
    assert_eq!(
        api.requests()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(
        files.requests()[0].header("authorization"),
        None,
        "the credential never reaches the file host"
    );
}

#[tokio::test]
async fn a_download_refuses_a_plain_http_location() {
    let api = Server::start(vec![
        Reply::new(302).header("Location", "http://files.example.com/export.ndjson"),
    ])
    .await;
    let error = client(&api).download_trader_export("swisstony", 42).await.unwrap_err();
    assert!(
        matches!(error, Error::Download(ref e) if e.reason == DownloadErrorReason::InsecureLocation),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_download_surfaces_the_api_error_for_a_job_that_is_not_ready() {
    let api = Server::start(vec![Reply::json(400, &error_body("bad_request", None))]).await;
    let error = client(&api).download_trader_export("swisstony", 42).await.unwrap_err();
    assert_eq!(error.status(), Some(400));
}

#[tokio::test]
async fn strict_query_validation_sends_its_header() {
    let server = Server::start(vec![Reply::json(200, &example("getHealth"))]).await;
    let client = Client::builder()
        .base_url(&server.base_url)
        .strict_query_validation(true)
        .build()
        .unwrap();
    client.get_health(&Default::default()).await.unwrap();
    assert_eq!(server.requests()[0].header("x-query-validation"), Some("strict"));
}

#[tokio::test]
async fn a_stream_error_is_terminal_even_with_frames_buffered() {
    // Both frames arrive in one chunk: the bad one first, a good one after it.
    let server = Server::start(vec![
        Reply::new(200).header("Content-Type", "text/event-stream").chunks([
            "data: [1]\n\nid: 2\ndata: {\"seq\":2,\"type\":\"X\"}\n\n",
            ": keep-alive\n\n",
        ]),
    ])
    .await;
    let mut reader = client(&server).open_stream(&StreamOptions::default()).await.unwrap();
    let error = reader.next().await.unwrap_err();
    assert!(
        matches!(error, Error::Stream(ref e) if e.reason == StreamProtocolErrorReason::InvalidEnvelope),
        "{error:?}"
    );
    assert!(reader.next().await.unwrap().is_none());
}

#[tokio::test]
async fn a_cursor_is_sent_exactly_as_received() {
    let mut first = example("listLeaderboard");
    first["has_more"] = json!(true);
    first["next_cursor"] = json!(" c1 ");
    let mut last = example("listLeaderboard");
    last["has_more"] = json!(false);
    let server = Server::start(vec![Reply::json(200, &first), Reply::json(200, &last)]).await;
    let client = client(&server);
    let rows = oxinsider::pagination::collect_all(ListLeaderboardParams::default(), async |params| {
        client.list_leaderboard(params).await
    })
    .await
    .unwrap();
    assert!(!rows.is_empty() || first["data"].as_array().is_some_and(Vec::is_empty));
    assert_eq!(server.requests()[1].target, "/api/v1/leaderboard?cursor=+c1+");
}

#[tokio::test]
async fn never_retries_a_write_the_api_does_not_replay() {
    // submit_trader_export starts a job; repeating it could start another.
    let server = Server::start(vec![
        Reply::json(503, &error_body("internal_error", None)).header("Retry-After", "0"),
        Reply::json(503, &error_body("internal_error", None)).header("Retry-After", "0"),
    ])
    .await;
    let error = client(&server)
        .submit_trader_export("swisstony", &Default::default())
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert_eq!(server.requests().len(), 1);
}
