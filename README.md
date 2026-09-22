# 0xinsider Rust SDK

The official Rust client for the [0xinsider Developer API](https://0xinsider.com/developers): Polymarket analytics for sports and esports. Every tracked wallet is graded S to F from settled P&L; the API returns grades, large trades, sharp-money flow, positions, market snapshots, reports and search.

- Website and API keys: https://0xinsider.com/developers
- Authentication (API keys and OAuth 2.1): https://0xinsider.com/auth.md
- OpenAPI 3.1: https://0xinsider.com/api/v1/openapi.json
- Documentation: https://docs.0xinsider.com
- API reference for this crate: https://docs.rs/oxinsider
- MCP server: https://api.0xinsider.com/api/v1/mcp

## Install

```sh
cargo add oxinsider
cargo add tokio --features macros,rt-multi-thread
```

The crate is `oxinsider`, because a crate name cannot start with a digit. It is async on Tokio, uses rustls by default (`default-features = false, features = ["native-tls"]` switches to the platform TLS), and needs Rust 1.87 or later.

## Try it without a key

The [sandbox](https://0xinsider.com/sandbox/api/v1) answers every documented operation with example data. It needs no credential and never touches production data.

```rust,no_run
use oxinsider::{Client, Error, ListLeaderboardParams, Method};

#[tokio::main]
async fn main() -> oxinsider::Result<()> {
    let client = Client::sandbox()?;
    let board = client.list_leaderboard(&ListLeaderboardParams::default().limit(5)).await?;
    println!("{:?}", board.data[0].username);

    // Any error the operation documents, on demand:
    let limited = client
        .request_json::<serde_json::Value>(Method::GET, "/api/v1/leaderboard", &[("sandbox_status", "429")], None)
        .await;
    if let Err(Error::Api(error)) = limited {
        println!("{:?} {:?}", error.code, error.retry_after);
    }
    Ok(())
}
```

## Live data

Data operations need an API key from [0xinsider.com/developers](https://0xinsider.com/developers) or an OAuth 2.1 access token ([auth.md](https://0xinsider.com/auth.md)), plus an active Pro subscription. Discovery, health and platforms are public.

```rust,no_run
use oxinsider::models::{Expand, Grade};
use oxinsider::pagination::Pager;
use oxinsider::{Client, GetTraderParams, ListWhaleTradesParams};

#[tokio::main]
async fn main() -> oxinsider::Result<()> {
    let client = Client::from_env()?; // reads OXINSIDER_API_KEY

    let trader = client
        .get_trader("swisstony", &GetTraderParams::default().expand([Expand::Strategy, Expand::Categories]))
        .await?;
    println!("{:?}", trader.data.grade);

    let mut pager = Pager::new(ListWhaleTradesParams::default().min_grade(Grade::A).limit(100));
    while let Some(page) = pager.next_page(async |params| client.list_whale_trades(params).await).await? {
        for trade in &page.data {
            println!("{:?} {:?}", trade.size_usd, trade.market.title);
        }
    }
    Ok(())
}
```

- **One method per operation.** Every operation in the OpenAPI document is an async method named after its operationId in snake_case (`listLeaderboard` is `list_leaderboard`). Path parameters and required query parameters are arguments; optional ones go in the operation's `...Params` struct, built with `Default::default()` and chainable setters. [`OPERATIONS`](https://docs.rs/oxinsider/latest/oxinsider/constant.OPERATIONS.html) lists them all with method, path and retry class.
- **Typed responses.** Each method returns the typed envelope for its operation (`ListWhaleTradesResponse { object, data, has_more, next_cursor, meta, .. }`). Every schema in the document is in [`oxinsider::models`](https://docs.rs/oxinsider/latest/oxinsider/models/index.html). A body that does not match returns `Error::Decode`, which names the JSON path that failed and keeps the body.
- **Absent is not zero.** A field the API did not report is `None`. Serve a truthful unavailable state; never read it as `0`. An ungraded wallet is uncovered, not unskilled.
- **Additive changes do not break you.** Unknown JSON keys are ignored, and an enum value this release does not know lands in `Other(String)` instead of failing the response.
- **Numbers.** Money and price fields are `f64`, the precision the API sends. Do not round before you display them.
- **Conditional reads.** Set `if_none_match` on the params; an unchanged resource returns `Error::NotModified { etag }`.
- **Writes.** Webhook writes take `idempotency_key` on their params. Reuse the same key when you retry by hand, and never reuse it with a different body.
- **Markdown and files.** `get_trader_context_markdown` and `get_market_context_markdown` return `String`. `download_trader_export` returns a streaming `Download`.

## Errors

A non-2xx answer is `Error::Api(ApiError)` with `status`, `code`, `reason`, `message`, `param`, `retry_at`, `retry_after` and `request_id`. Branch on `code` and `reason`, never on `message`; `ApiError::kind()` maps the status to `BadRequest`, `Authentication`, `SubscriptionRequired`, `PermissionDenied`, `NotFound`, `RateLimited`, `Server` and the rest.

```rust,ignore
match client.get_trader("swisstony", &Default::default()).await {
    Ok(trader) => println!("{:?}", trader.data.grade),
    Err(oxinsider::Error::Api(error)) if error.is_rate_limited() => {
        tokio::time::sleep(error.retry_after.unwrap_or(std::time::Duration::from_secs(1))).await;
    }
    Err(error) => return Err(error),
}
```

The other variants: `NotModified`, `InsecureTransport` (see below), `Transport` (no HTTP response), `Decode`, `Stream`, `Download`, `Pagination` and `Config`.

## Retries

A read (every GET, and the batch reads `batch_get_traders` and `batch_get_market_intel`, which store nothing) is retried on 408, 429, 502, 503 and 504, and on a failed connection. A webhook write is retried only when it carries an `idempotency_key`; every other write is sent once. The client waits for `Retry-After` (seconds or an HTTP date) plus up to 250 ms of jitter, or a jittered backoff from 500 ms up to 8 s without one. A `Retry-After` longer than 60 seconds is not waited out: the error comes back with `retry_after` for you to schedule. `max_retries` defaults to 2; `Client::builder().max_retries(0)` sends each request once. Each attempt has a 30-second deadline (`timeout`, `no_timeout`).

## Pagination

List operations answer `{ object: "list", data, has_more, next_cursor, meta }`. `Pager` follows `next_cursor` until `has_more` is false; `pagination::collect_all` gathers every row. A page that says `has_more` with no usable `next_cursor`, or that repeats a cursor the walk already requested, is a broken response rather than the end: the walk stops with `Error::Pagination` (`MissingCursor` or `RepeatedCursor`) instead of truncating or looping. Feeds move while you read them: follow the cursor to completion, never page by offset, and never total a partial walk. `Pager::params()` holds the next cursor if you stop early and want to resume.

## Stream

`GET /api/v1/stream` is an unbounded Server-Sent Events stream. Read it with `open_stream`, which delivers each frame as it arrives, holds at most `max_frame_bytes` (1 MiB by default) for one undelivered frame, and closes the connection when the reader is dropped:

```rust,ignore
use oxinsider::StreamOptions;

let mut cursor: Option<i64> = None; // the last seq you processed
let mut options = StreamOptions::default().events(["WhaleTradesInserted"]);
options.last_event_id = cursor;
let mut reader = client.open_stream(&options).await?; // Error::Api for a 401, a 429 with retry_after, ...
while let Some(frame) = reader.next().await? {
    if frame.resync {
        // The resume point is outside the retained window: refetch state, then continue live.
        continue;
    }
    cursor = frame.seq;
    println!("{} {:?}", frame.event_type, frame.data);
}
// The server closed the stream: reconnect with `cursor`.
```

A frame that breaks the SSE contract (not JSON, not an object, no usable sequence, a malformed `resync` marker, a `200` that is not `text/event-stream`, or a frame past the byte ceiling) ends the stream with `Error::Stream(StreamProtocolError)` whose `reason`, `last_seq` and `frame_id` say which frame and where to resume from; a malformed frame never moves `last_seq`. Keep-alive comments are consumed silently, `retry:` is exposed as `retry_hint()`, and the response headers are on `headers()`. The stream has no deadline and is never retried for you. `StreamFrame::decode::<T>()` turns a frame into your own type.

## Exports

```rust,ignore
use oxinsider::models::TraderExportJobDataStatus;

let job_id = client.submit_trader_export("swisstony", &Default::default()).await?.data.job_id;
loop {
    let job = client.get_trader_export_status("swisstony", job_id).await?;
    match job.data.status {
        TraderExportJobDataStatus::Ready => break,
        TraderExportJobDataStatus::Failed => return Ok(()), // read job.data for why
        _ => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
    }
}
let download = client.download_trader_export("swisstony", job_id).await?;
println!("{:?} {:?}", download.content_type(), download.filename());
let saved = download.save("swisstony.ndjson").await?;
println!("{} bytes, sha256 {}", saved.bytes_written, saved.sha256);
```

The API answers the download with a redirect to a short-lived file location. The client follows it once, with a fresh request that carries no credential and no cookies, and only to `https://` (or `http://` on a loopback host). `chunk()` streams the file and `save()` writes it with bounded memory; `bytes(max)` reads it into memory and refuses more than `max`. No checksum is published for an export: `save` returns the SHA-256 of exactly what it wrote, and `etag()` is the file host's object identity, not a content hash. A failed hop is `Error::Download` with a `reason` (`InsecureLocation`, `UnexpectedRedirect`, `Unavailable`, `TooLarge`, `Interrupted`, `NotRedirected`, `MissingLocation`) and never the signed URL.

## Webhooks

```rust,ignore
use oxinsider::webhooks::{SIGNATURE_HEADER, TIMESTAMP_HEADER, verify_signature};

// Verify the exact bytes you received, before parsing them.
let ok = verify_signature(&signing_secret, timestamp_header, signature_header, raw_body)?;
if !ok {
    // answer 400: the delivery is forged, stale or corrupted
}
```

The API signs every delivery as `v1=hex(HMAC_SHA256(signing_secret, "<timestamp>.<raw_body>"))`. `verify_signature` enforces the published 300-second replay window and compares every candidate in the header in constant time, so it accepts both signatures during a staged secret rotation. It returns `Ok(false)` for a bad delivery and `Err` only for an empty secret, which is your configuration bug, not the sender's. `verify_signature_at` takes an explicit tolerance and clock; `compute_signature` signs a payload for your tests.

## Where the key goes

The key is sent over `https://` only, or over `http://` to a loopback host (`localhost`, `127.0.0.1`, `[::1]`) for a backend you run yourself. A `base_url` that would send it anywhere else fails `build()` with `Error::InsecureTransport`, and every credentialed request is checked again before it is sent; the error names the destination, never the key. A keyless client may still call the public operations on such a base. The client this crate builds follows no redirects, so an `https://` answer cannot downgrade a request to `http://`. A live key (`oxi_sk_live_...`) is refused on the sandbox so it is never sent there. `Debug` on a `Client` redacts the key.

Bring your own `reqwest::Client` (a proxy, custom roots) with `Client::builder().http_client(...)`; this crate re-exports `reqwest` so the versions match.

## How it is built

`src/models.rs` and `src/operations.rs` are generated from the published [OpenAPI document](https://0xinsider.com/api/v1/openapi.json) by `scripts/generate.py` (Python 3 standard library, then `rustfmt`), and a weekly workflow opens a pull request when the contract changes. `python3 scripts/generate.py --check` fails when the generated files are stale. The generator understands every construct the document uses and stops on one it does not, rather than emitting an untyped value that hides the gap.

`src/provenance.rs` says which document a release was generated from: `OPENAPI_SHA256` (the SHA-256 of the document bytes), `OPENAPI_VERSION`, `OPERATION_COUNT`, and `APP_COMMIT`, the `0xinsider/0xinsider` commit that last changed `web/public/api/v1/openapi.json` (or `None` when it could not be resolved). Compare `oxinsider::provenance::OPENAPI_SHA256` with `shasum -a 256` of the live document to see whether a release is behind the API.

The tests decode every example the document publishes into its generated type, run the request path against a scripted local server, and (with `OXINSIDER_SANDBOX_TESTS=1`) decode the live sandbox's answer for every JSON operation.

```sh
cargo test
OXINSIDER_SANDBOX_TESTS=1 cargo test --test sandbox
cargo run --example sandbox
OXINSIDER_API_KEY=oxi_sk_live_... cargo run --example stream
```

## Other official tools

- Python SDK: `pip install 0xinsider` ([0xinsider/0xinsider-python](https://github.com/0xinsider/0xinsider-python))
- Go SDK: `go get github.com/0xinsider/0xinsider-go` ([0xinsider/0xinsider-go](https://github.com/0xinsider/0xinsider-go))
- Node.js and TypeScript SDK: `npm install @0xinsider/sdk` ([0xinsider/0xinsider-node](https://github.com/0xinsider/0xinsider-node))
- CLI and MCP server: `npm install --global @0xinsider/mcp` or `brew install 0xinsider/tap/oxinsider`
- Remote MCP server: `https://api.0xinsider.com/api/v1/mcp`
- Agent Plugin and skills: [0xinsider/agent-plugin](https://github.com/0xinsider/agent-plugin)

## License

MIT
