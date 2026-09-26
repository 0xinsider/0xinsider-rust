# Architecture

## Overview

`oxinsider` is the official Rust client for the 0xinsider Developer API. Most of it is generated from the published OpenAPI 3.1 document (one typed model per schema, one async method per operation); a small hand-written core owns everything the document cannot say: where a credential may go, retries, error decoding, the event stream, cursor walks, redirect-only downloads and webhook verification.

## Directory structure

```
scripts/generate.py        OpenAPI document -> the generated files below (stdlib Python, then rustfmt)
openapi.json               the document exactly as fetched (generated; the tests read its examples)
src/
  lib.rs                   crate docs and re-exports
  models.rs                GENERATED: every schema, request body and response envelope
  operations.rs            GENERATED: Operation table, ...Params structs, one Client method per operation,
                           ListPage / CursorParams impls
  provenance.rs            GENERATED: which document this release came from (SHA-256, version, app commit)
  client.rs                Client, ClientBuilder, the request path (Call -> execute -> decode), retries
  policy.rs                where a credential may go (https, or http to loopback)
  error.rs                 Error, ApiError, ApiErrorKind, DecodeError
  stream.rs                Client::open_stream and the incremental SSE reader
  pagination.rs            ListPage, CursorParams, CursorWalk, Pager, collect_all
  download.rs              the one-hop, credential-free redirect follow for exports
  webhooks.rs              delivery signature verification
tests/
  examples.rs              every documented example decodes into its generated type
  client.rs                the request path against a scripted loopback server
  sandbox.rs               live sandbox checks (OXINSIDER_SANDBOX_TESTS=1)
  support/server.rs        the scripted HTTP/1.1 server
  support/generated.rs     GENERATED: operation id / schema name -> Rust type, for the tests
examples/                  discovery, sandbox, stream
```

## Core components

### Generator (`scripts/generate.py`)

Reads the document (the published URL by default, a file path, or the committed snapshot with `--check`) and writes the generated files through `rustfmt`, so they never drift from `cargo fmt`. Rules it applies:

- An object schema becomes a struct. A field that is not required, or is nullable, is `Option<T>`; a required nullable field serializes `null` explicitly. `additionalProperties: true` beside named properties adds a flattened `extra` map. `allOf` with one `$ref` and extra properties flattens the reference.
- A string enum becomes an enum with an `Other(String)` arm (serde `untagged`), `as_str`, `From<&str>` and `Display`. One value set is one type, named by a component when one names it, else by the property that most often carries it (`ENUM_NAME_OVERRIDES` covers the generic ones). A single-value enum stays `String`. A documented value spelled `other` becomes `OtherValue`, so `Other` always means a value this release does not know.
- A `oneOf` with a discriminator mapping becomes an enum with a hand-rolled `Deserialize` that reads the tag and keeps an unknown tag in `Unknown(Value)`. An untagged scalar union (a JSON-RPC id) is `serde_json::Value`.
- Inline objects are named owner + property. A name a component, an enum or a different inline schema already holds is never reused: `INLINE_NAME_OVERRIDES` or a numeric suffix disambiguates it.
- Response-only types are `#[non_exhaustive]`; types a caller builds (request bodies and what they nest) are not, and derive `Default` when every field is optional.
- An operation gets a method unless it is the SSE stream (`Client::open_stream`) or documented only to refuse (`GET /api/v1/mcp`). Path parameters and required query parameters are arguments, optional query parameters and documented headers go in a `...Params` struct, a JSON body is `body: &T`. `expand[]` and `wallet[]` aliases are dropped; the canonical names are sent.
- Retry class: every GET and the two read-only batch POSTs are `Read`; a write that documents `Idempotency-Key` is `Keyed`; everything else is `Never`.
- It raises on a construct it does not understand, so a contract change fails the regenerate instead of shipping an untyped field.

### Client (`client.rs`)

`ClientBuilder::build` validates the base URL and key, applies the credential policy, and builds a `reqwest::Client` that follows no redirects. Every generated method builds a `Call` (operation, path, query, headers, body) and hands it to `send_json`, `send_json_optional`, `send_text` or `send_download`. `execute` retries by the call's `RetryClass`; `api_error` decodes both the error envelope and the per-IP limiter's flat shape, reading at most 1 MiB.

### Credential policy (`policy.rs`)

`https` anywhere, `http` only to `localhost`, `127.0.0.0/8` or `::1`. Checked at build, on every credentialed request, and on a download's redirect target. The error carries the origin, never the key.

### Stream (`stream.rs`)

Reads chunks from the response and parses SSE lines itself: LF or CRLF, comments skipped, `event`/`id`/`data`/`retry` fields, a frame dispatched on a blank line. It bounds the bytes of one undelivered frame and ends on the first contract violation with a `StreamProtocolError` whose `last_seq` never moves on a bad frame. Same contract as the Go and TypeScript clients.

### Pagination (`pagination.rs`)

`CursorWalk` records pages and cursors (the last 1,024) and refuses a `has_more` page without a cursor or with a repeated one. `Pager` drives a walk with an async closure; `collect_all` drains it.

## Data flow

```
Client::list_whale_trades(&params)            generated (operations.rs)
  -> Call::new(&Operation::LIST_WHALE_TRADES)   path, query, headers, body
  -> Client::send_json(call)                    client.rs
       -> execute: build_request (policy check, bearer, UA, Accept)
                   -> reqwest -> retry on 408/429/502/503/504 per RetryClass
       -> 304: Error::NotModified   non-2xx: api_error -> Error::Api
       -> 2xx: serde_path_to_error -> ListWhaleTradesResponse (models.rs) or Error::Decode
```

## External dependencies

| Crate | Purpose |
| --- | --- |
| reqwest | HTTP (rustls by default, gzip, HTTP/2, system proxy) |
| tokio | timers for retries, file writes for downloads |
| serde, serde_json | models and bodies |
| serde_path_to_error | the JSON path in a decode error |
| thiserror | error types |
| hmac, sha2 | webhook signatures, download digests |
| httpdate | `Retry-After` as an HTTP date |
| bytes | download chunks |

## Configuration

`OXINSIDER_API_KEY` (read by `Client::from_env`). Features: `rustls` (default) or `native-tls`. MSRV 1.87, checked in CI.

## Testing strategy

- `cargo test`: unit tests in each module, `tests/examples.rs` (every `example`/`examples` in the document decodes; `x-examples` are reported, not enforced, matching the app repository's own policy), `tests/client.rs` (scripted loopback server), and the README's complete programs as doctests.
- `OXINSIDER_SANDBOX_TESTS=1 cargo test --test sandbox`: every JSON GET's live sandbox answer decodes into its type; typed calls, errors and a pager walk end to end.
- `python3 scripts/generate.py --check`: the generated files match the committed document.
