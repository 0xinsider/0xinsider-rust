//! The official Rust client for the [0xinsider Developer API](https://0xinsider.com/developers):
//! Polymarket analytics for sports and esports. Every tracked wallet is graded S
//! to F from settled P&L; the API returns grades, large trades, sharp-money
//! flow, positions, market snapshots, reports and search.
//!
//! Every operation in the [OpenAPI document](https://0xinsider.com/api/v1/openapi.json)
//! is a typed async method on [`Client`], named after its operationId in
//! snake_case (`listLeaderboard` is [`Client::list_leaderboard`]). Required
//! parameters are arguments; optional ones go in a `...Params` struct.
//!
//! ```no_run
//! # async fn run() -> oxinsider::Result<()> {
//! use oxinsider::{Client, ListLeaderboardParams};
//!
//! let client = Client::from_env()?; // reads OXINSIDER_API_KEY
//! let board = client.list_leaderboard(&ListLeaderboardParams::default().limit(10)).await?;
//! for entry in &board.data {
//!     println!("{} {:?}", entry.address, entry.grade);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Build against the [sandbox](SANDBOX_BASE_URL) first: [`Client::sandbox`]
//! needs no key and answers every documented operation with example data.
//!
//! - **Absent is not zero.** A field the API did not report is `None`. Serve a
//!   truthful unavailable state; never read it as `0`.
//! - **Errors.** A non-2xx answer is [`Error::Api`]. Branch on
//!   [`ApiError::code`] and [`ApiError::reason`], never on the message.
//! - **Retries.** Reads, and writes that carry an `Idempotency-Key`, are retried
//!   on 408, 429, 502, 503 and 504, honoring `Retry-After` up to 60 seconds.
//! - **Pagination.** Follow cursors to completion with [`pagination::Pager`].
//! - **Stream.** Read `GET /api/v1/stream` with [`Client::open_stream`].
//! - **Webhooks.** Verify deliveries with [`webhooks::verify_signature`].
//! - **Where the credential goes.** Over `https`, or `http` to a loopback host
//!   only; anything else fails with [`Error::InsecureTransport`] before a request is sent.

#![cfg_attr(docsrs, feature(doc_cfg))]

mod client;
mod download;
mod error;
pub mod models;
mod operations;
pub mod pagination;
mod policy;
pub mod provenance;
mod stream;
pub mod webhooks;

pub use client::{
    API_KEY_ENV, Client, ClientBuilder, Operation, PRODUCTION_BASE_URL, RetryClass, SANDBOX_BASE_URL, VERSION,
};
pub use download::{DEFAULT_MAX_READ_BYTES, Download, DownloadError, DownloadErrorReason, SavedDownload};
pub use error::{ApiError, ApiErrorKind, DecodeError, Error, Result};
pub use operations::*;
pub use pagination::{PaginationError, PaginationErrorReason};
pub use policy::{InsecureTransportError, is_loopback_host, is_trusted_destination};
pub use reqwest::{self, Method};
pub use stream::{
    DEFAULT_MAX_FRAME_BYTES, StreamFrame, StreamOptions, StreamProtocolError, StreamProtocolErrorReason, StreamReader,
};

// Compiles the README's complete programs as doctests, so its examples cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
