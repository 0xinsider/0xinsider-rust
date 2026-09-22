//! The client: configuration, the credential check, and the request path every
//! generated method goes through (retries, error decoding, typed bodies).

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, RETRY_AFTER, USER_AGENT};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::download::Download;
use crate::error::{ApiError, DecodeError, Error, Result};
use crate::policy::assert_credential_destination;

/// The production API origin. Every operation path is relative to it.
pub const PRODUCTION_BASE_URL: &str = "https://api.0xinsider.com";

/// The sandbox: no credential, no production data, every documented operation
/// answered with its example. Add `sandbox_status=<code>` to a query to get one
/// of the errors the operation documents.
pub const SANDBOX_BASE_URL: &str = "https://0xinsider.com/sandbox";

/// The environment variable [`Client::from_env`] reads the API key from.
pub const API_KEY_ENV: &str = "OXINSIDER_API_KEY";

/// This crate's version, sent in the `User-Agent` header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_RETRIES: u32 = 2;
/// A `Retry-After` longer than this is not waited out: the error is returned
/// with `retry_after` for the caller to schedule.
const RETRY_AFTER_CEILING: Duration = Duration::from_secs(60);
const RETRY_JITTER_MS: u64 = 250;
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_CAP_MS: u64 = 8_000;
/// Error bodies are small; a larger one is truncated rather than buffered.
const MAX_ERROR_BODY_BYTES: usize = 1 << 20;
const RETRY_STATUSES: [u16; 5] = [408, 429, 502, 503, 504];
const LIVE_KEY_PREFIX: &str = "oxi_sk_live_";

/// When a failed request may be sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetryClass {
    /// A read (every GET, and the batch reads that store nothing): always safe to repeat.
    Read,
    /// A write the API replays under `Idempotency-Key`: retried only when the call carries one.
    Keyed,
    /// Never retried: repeating it could repeat its effect.
    Never,
}

/// One operation in the OpenAPI document. [`OPERATIONS`](crate::OPERATIONS) lists them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Operation {
    /// The operationId (`listLeaderboard`).
    pub id: &'static str,
    /// The HTTP method.
    pub method: &'static str,
    /// The path template (`/api/v1/trader/{address}`).
    pub path: &'static str,
    /// The media type a success answers with.
    pub accept: &'static str,
    /// When a failure may be retried.
    pub retry: RetryClass,
    /// Whether the operation needs a credential. Discovery, health, platforms
    /// and agent registration do not.
    pub requires_credential: bool,
}

/// A typed client for the 0xinsider Developer API.
///
/// Cheap to clone: clones share one connection pool. Build one with
/// [`Client::from_env`], [`Client::new`], [`Client::sandbox`] or
/// [`Client::builder`].
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    timeout: Option<Duration>,
    max_retries: u32,
    strict_query: bool,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.inner.base_url)
            .field("credential", &self.inner.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.inner.timeout)
            .field("max_retries", &self.inner.max_retries)
            .field("strict_query", &self.inner.strict_query)
            .finish()
    }
}

impl Client {
    /// A client for the production API with this API key (`oxi_sk_live_...`) or
    /// OAuth 2.1 access token (`oxi_at_...`).
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::builder().api_key(api_key).build()
    }

    /// A client for the production API with the key in `OXINSIDER_API_KEY`.
    ///
    /// When the variable is unset or empty the client has no credential: the
    /// public operations (discovery, health, platforms) still work, and every
    /// other operation answers `401 invalid_api_key`.
    pub fn from_env() -> Result<Self> {
        Self::builder().api_key_from_env().build()
    }

    /// A client for the sandbox: no credential, example data only.
    pub fn sandbox() -> Result<Self> {
        Self::builder().sandbox().build()
    }

    /// A builder for any other configuration.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// The server URL every operation path is appended to.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// Whether requests carry a credential.
    pub fn has_credential(&self) -> bool {
        self.inner.api_key.is_some()
    }

    /// Send any request and decode the JSON answer, for an operation this
    /// release does not know yet. `path` is appended to the base URL
    /// (`/api/v1/leaderboard`). GETs are retried like a generated read.
    pub async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let retry = if method == Method::GET {
            RetryClass::Read
        } else {
            RetryClass::Never
        };
        let mut call = Call::raw(method, path, retry);
        for (name, value) in query {
            call.query_owned((*name).to_owned(), (*value).to_owned());
        }
        if let Some(body) = body {
            call.json(body)?;
        }
        self.send_json(call).await
    }

    pub(crate) fn url(&self, path: &str, query: &[(String, String)]) -> Result<Url> {
        let mut url = Url::parse(&format!("{}{}", self.inner.base_url, path))
            .map_err(|error| Error::Config(format!("cannot build a URL from {path:?}: {error}")))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query {
                pairs.append_pair(name, value);
            }
        }
        Ok(url)
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    /// Build one attempt of a call. The credential goes on only after the
    /// destination passes the policy.
    pub(crate) fn build_request(&self, call: &Call, url: &Url, timeout: Option<Duration>) -> Result<reqwest::Request> {
        let mut builder = self
            .inner
            .http
            .request(call.method.clone(), url.clone())
            .header(ACCEPT, call.accept.as_str())
            .header(USER_AGENT, concat!("0xinsider-rust/", env!("CARGO_PKG_VERSION")));
        if let Some(key) = &self.inner.api_key {
            assert_credential_destination(url, false)?;
            builder = builder.bearer_auth(key);
        }
        if self.inner.strict_query {
            builder = builder.header("X-Query-Validation", "strict");
        }
        for (name, value) in &call.headers {
            builder = builder.header(*name, value.as_str());
        }
        if let Some(body) = &call.body {
            builder = builder.header(CONTENT_TYPE, "application/json").body(body.clone());
        }
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let request = builder.build()?;
        if request.headers().contains_key(AUTHORIZATION) {
            assert_credential_destination(request.url(), false)?;
        }
        Ok(request)
    }

    /// Send a call, retrying when its class and the failure allow, and return
    /// the final response whatever its status.
    pub(crate) async fn execute(&self, call: &Call) -> Result<reqwest::Response> {
        let url = self.url(&call.path, &call.query)?;
        let retryable = match call.retry {
            RetryClass::Read => true,
            RetryClass::Keyed => call
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Idempotency-Key")),
            RetryClass::Never => false,
        };
        let mut attempt = 0;
        loop {
            let request = self.build_request(call, &url, self.inner.timeout)?;
            match self.inner.http.execute(request).await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if retryable && attempt < self.inner.max_retries && RETRY_STATUSES.contains(&status) {
                        if let Some(delay) = retry_delay(response.headers(), attempt) {
                            drop(response);
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    return Ok(response);
                }
                Err(error) if retryable && attempt < self.inner.max_retries && error.is_connect() => {
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                }
                Err(error) => return Err(Error::Transport(error)),
            }
        }
    }

    pub(crate) async fn send_json<T: DeserializeOwned>(&self, call: Call) -> Result<T> {
        let (status, body) = self.send_for_body(&call).await?;
        decode(&call, status, &body)
    }

    pub(crate) async fn send_json_optional<T: DeserializeOwned>(&self, call: Call) -> Result<Option<T>> {
        let (status, body) = self.send_for_body(&call).await?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        decode(&call, status, &body).map(Some)
    }

    pub(crate) async fn send_text(&self, call: Call) -> Result<String> {
        let (_, body) = self.send_for_body(&call).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    pub(crate) async fn send_download(&self, call: Call) -> Result<Download> {
        let response = self.execute(&call).await?;
        Download::follow(self, &call, response).await
    }

    async fn send_for_body(&self, call: &Call) -> Result<(u16, bytes::Bytes)> {
        let response = self.execute(call).await?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            let etag = header_string(response.headers(), ETAG.as_str());
            return Err(Error::NotModified { etag });
        }
        if !status.is_success() {
            return Err(api_error(response).await);
        }
        Ok((status.as_u16(), response.bytes().await?))
    }
}

fn decode<T: DeserializeOwned>(call: &Call, status: u16, body: &[u8]) -> Result<T> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        Error::from(DecodeError {
            operation: call.label(),
            status,
            path,
            message: inner.to_string(),
            body: String::from_utf8_lossy(body).into_owned(),
        })
    })
}

/// Decode a non-2xx answer into an [`ApiError`]. Reads at most 1 MiB of body.
pub(crate) async fn api_error(mut response: reqwest::Response) -> Error {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
    let error_object = parsed.as_ref().and_then(|value| value.get("error"));
    let field = |name: &str| {
        error_object
            .and_then(|error| error.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    // The per-IP limiter answers `{"error": "<code>", "message": "..."}`
    // rather than the error envelope; read either shape.
    let code = field("code").or_else(|| error_object.and_then(serde_json::Value::as_str).map(str::to_owned));
    let message = field("message")
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("request failed")
                .to_owned()
        });
    let request_id = parsed
        .as_ref()
        .and_then(|v| v.pointer("/meta/request_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| header_string(&headers, "x-request-id"));
    let (reason, param, doc_url, retry_at) = (field("reason"), field("param"), field("doc_url"), field("retry_at"));
    let body_value = parsed
        .or_else(|| (!body.is_empty()).then(|| serde_json::Value::String(String::from_utf8_lossy(&body).into_owned())));
    Error::from(ApiError {
        status,
        code,
        reason,
        message,
        param,
        doc_url,
        retry_at,
        retry_after: parse_retry_after(&headers, SystemTime::now()),
        request_id,
        body: body_value,
    })
}

pub(crate) fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

/// `Retry-After` as delta-seconds or an HTTP-date. A negative, malformed or
/// past value is treated as absent.
pub(crate) fn parse_retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let at = httpdate::parse_http_date(raw).ok()?;
    at.duration_since(now).ok()
}

/// How long to wait before retry `attempt + 1`, or `None` when the server asked
/// for longer than the ceiling (the error is then returned for the caller to schedule).
fn retry_delay(headers: &HeaderMap, attempt: u32) -> Option<Duration> {
    match parse_retry_after(headers, SystemTime::now()) {
        Some(wait) if wait > RETRY_AFTER_CEILING => None,
        Some(wait) => Some(wait + Duration::from_millis(jitter(RETRY_JITTER_MS))),
        None => Some(backoff(attempt)),
    }
}

/// Jittered exponential backoff: 500 ms doubling per attempt, capped at 8 s.
fn backoff(attempt: u32) -> Duration {
    let ceiling = BACKOFF_BASE_MS
        .saturating_mul(1u64 << attempt.min(16))
        .min(BACKOFF_CAP_MS);
    Duration::from_millis(ceiling / 2 + jitter(ceiling / 2))
}

/// A uniform-enough value in `0..=max` from the standard library's per-process
/// random hasher keys, so retries from many clients do not align.
fn jitter(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.finish() % (max + 1)
}

/// Percent-encode one path segment: everything but unreserved characters and `@`.
pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// One request a generated method describes: its operation, path, query,
/// headers and body.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub(crate) id: Option<&'static str>,
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) accept: String,
    pub(crate) retry: RetryClass,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Option<Vec<u8>>,
}

impl Call {
    pub(crate) fn new(operation: &Operation, path: String) -> Self {
        Self {
            id: Some(operation.id),
            method: Method::from_bytes(operation.method.as_bytes()).unwrap_or(Method::GET),
            path,
            accept: operation.accept.to_owned(),
            retry: operation.retry,
            query: Vec::new(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub(crate) fn raw(method: Method, path: &str, retry: RetryClass) -> Self {
        Self {
            id: None,
            method,
            path: path.to_owned(),
            accept: "application/json".to_owned(),
            retry,
            query: Vec::new(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub(crate) fn query(&mut self, name: &'static str, value: String) {
        self.query.push((name.to_owned(), value));
    }

    pub(crate) fn query_owned(&mut self, name: String, value: String) {
        self.query.push((name, value));
    }

    pub(crate) fn header(&mut self, name: &'static str, value: Option<&str>) {
        if let Some(value) = value {
            self.headers.push((name, value.to_owned()));
        }
    }

    pub(crate) fn json<B: Serialize + ?Sized>(&mut self, body: &B) -> Result<()> {
        let bytes = serde_json::to_vec(body)
            .map_err(|error| Error::Config(format!("the request body does not serialize: {error}")))?;
        self.body = Some(bytes);
        Ok(())
    }

    pub(crate) fn label(&self) -> String {
        match self.id {
            Some(id) => id.to_owned(),
            None => format!("{} {}", self.method, self.path),
        }
    }
}

/// Configures a [`Client`].
#[derive(Clone)]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: String,
    sandbox: bool,
    timeout: Option<Duration>,
    max_retries: u32,
    strict_query: bool,
    http: Option<reqwest::Client>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("base_url", &self.base_url)
            .field("credential", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("sandbox", &self.sandbox)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("strict_query", &self.strict_query)
            .finish_non_exhaustive()
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: PRODUCTION_BASE_URL.to_owned(),
            sandbox: false,
            timeout: Some(DEFAULT_TIMEOUT),
            max_retries: DEFAULT_MAX_RETRIES,
            strict_query: false,
            http: None,
        }
    }
}

impl ClientBuilder {
    /// Authenticate with an API key (`oxi_sk_live_...`, or `oxi_sk_test_...` on
    /// the sandbox) or an OAuth 2.1 access token (`oxi_at_...`).
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Read the key from `OXINSIDER_API_KEY`. Unset or empty leaves the client
    /// without a credential.
    #[must_use]
    pub fn api_key_from_env(mut self) -> Self {
        self.api_key = std::env::var(API_KEY_ENV).ok().filter(|key| !key.trim().is_empty());
        self
    }

    /// Target another server: a proxy, or a backend you run yourself. A path on
    /// it is kept and the `/api/v1/...` operation paths are appended after it.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Target the sandbox. A live key (`oxi_sk_live_...`) is refused, so it is
    /// never sent there; a sandbox key (`oxi_sk_test_...`) is optional.
    #[must_use]
    pub fn sandbox(mut self) -> Self {
        self.base_url = SANDBOX_BASE_URL.to_owned();
        self.sandbox = true;
        self
    }

    /// The deadline for one attempt of a request, body included. 30 seconds by
    /// default. The event stream never has one.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// No per-request deadline.
    #[must_use]
    pub fn no_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Retries after a 408, 429, 502, 503 or 504, or a failed connection: 2 by
    /// default, `0` sends each request once. Only reads, and writes that carry
    /// an `Idempotency-Key`, are ever retried.
    #[must_use]
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Send `X-Query-Validation: strict`, so an unknown query parameter answers
    /// `400 bad_request` (`reason` `unknown_query_parameter`) instead of being ignored.
    #[must_use]
    pub fn strict_query_validation(mut self, strict: bool) -> Self {
        self.strict_query = strict;
        self
    }

    /// Use your own `reqwest::Client` (a proxy, custom TLS roots). The client
    /// this crate builds follows no redirects; yours keeps its own redirect
    /// policy, and reqwest drops `Authorization` when a redirect changes host or port.
    #[must_use]
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Build the client.
    ///
    /// Fails with [`Error::InsecureTransport`] when a credential would go over
    /// plain HTTP to a host that is not loopback, and with [`Error::Config`] for
    /// an empty key, a key that is not a valid header value, a live key on the
    /// sandbox, or a base URL that is not `http(s)`.
    pub fn build(self) -> Result<Client> {
        let base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        let parsed = Url::parse(&base_url).map_err(|error| Error::Config(format!("base URL {base_url:?}: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(Error::Config(format!(
                "base URL {base_url:?} must be http:// or https:// with a host"
            )));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(Error::Config(format!(
                "base URL {base_url:?} must not carry a query or fragment"
            )));
        }
        // A key read from a file or an environment variable often carries a
        // trailing newline; keys never contain whitespace, so trim it.
        let api_key = match self.api_key.map(|key| key.trim().to_owned()) {
            Some(key) if key.is_empty() => {
                return Err(Error::Config(
                    "the API key is empty; omit it for a keyless client".to_owned(),
                ));
            }
            Some(key) => {
                HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| Error::Config("the API key is not a valid header value".to_owned()))?;
                Some(key)
            }
            None => None,
        };
        if let Some(key) = &api_key {
            if (self.sandbox || base_url == SANDBOX_BASE_URL) && key.starts_with(LIVE_KEY_PREFIX) {
                return Err(Error::Config(
                    "a live key (oxi_sk_live_...) is never sent to the sandbox; use a sandbox key (oxi_sk_test_...) or none".to_owned(),
                ));
            }
            assert_credential_destination(&parsed, false)?;
        }
        let http = match self.http {
            Some(http) => http,
            None => reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                .build()?,
        };
        Ok(Client {
            inner: Arc::new(Inner {
                http,
                base_url,
                api_key,
                timeout: self.timeout,
                max_retries: self.max_retries,
                strict_query: self.strict_query,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_keeps_unreserved_and_at() {
        assert_eq!(encode_path_segment("swisstony"), "swisstony");
        assert_eq!(encode_path_segment("@name"), "@name");
        assert_eq!(encode_path_segment("a/b c?d"), "a%2Fb%20c%3Fd");
        assert_eq!(encode_path_segment("0xAbC"), "0xAbC");
    }

    #[test]
    fn retry_after_parses_seconds_and_dates() {
        let mut headers = HeaderMap::new();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&headers, now), Some(Duration::from_secs(7)));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("-1"));
        assert_eq!(parse_retry_after(&headers, now), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&headers, now), None);
        let at = httpdate::fmt_http_date(now + Duration::from_secs(90));
        headers.insert(RETRY_AFTER, HeaderValue::from_str(&at).unwrap());
        assert_eq!(parse_retry_after(&headers, now), Some(Duration::from_secs(90)));
    }

    #[test]
    fn a_retry_after_past_the_ceiling_is_not_waited_out() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
        assert_eq!(retry_delay(&headers, 0), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
        let delay = retry_delay(&headers, 0).unwrap();
        assert!(delay >= Duration::from_secs(1) && delay <= Duration::from_millis(1250));
    }

    #[test]
    fn backoff_is_bounded() {
        for attempt in 0..40 {
            assert!(backoff(attempt) <= Duration::from_millis(BACKOFF_CAP_MS));
        }
    }

    #[test]
    fn builder_refuses_a_credential_over_plain_http() {
        let error = Client::builder()
            .api_key("oxi_sk_live_x")
            .base_url("http://api.example.com")
            .build()
            .unwrap_err();
        assert!(matches!(error, Error::InsecureTransport(_)));
        assert!(!error.to_string().contains("oxi_sk_live_x"));
        Client::builder()
            .api_key("k")
            .base_url("http://localhost:8080")
            .build()
            .unwrap();
        Client::builder().base_url("http://api.example.com").build().unwrap();
    }

    #[test]
    fn builder_refuses_a_live_key_on_the_sandbox() {
        let error = Client::builder()
            .sandbox()
            .api_key("oxi_sk_live_x")
            .build()
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
        Client::builder().sandbox().api_key("oxi_sk_test_x").build().unwrap();
    }

    #[test]
    fn builder_refuses_an_empty_key() {
        assert!(matches!(Client::new("  "), Err(Error::Config(_))));
    }

    #[test]
    fn debug_redacts_the_credential() {
        let builder = Client::builder().api_key("oxi_sk_live_secret");
        let debug = format!("{builder:?}");
        assert!(!debug.contains("secret") && debug.contains("<redacted>"));
        let client = builder.build().unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret") && debug.contains("<redacted>"));
    }

    #[test]
    fn a_trailing_newline_on_the_key_is_trimmed() {
        let client = Client::new("oxi_sk_live_x\n").unwrap();
        assert!(client.has_credential());
    }

    #[test]
    fn base_url_path_is_kept() {
        let client = Client::builder()
            .base_url("https://proxy.example/prefix/")
            .build()
            .unwrap();
        let url = client.url("/api/v1/health", &[("a".into(), "b c".into())]).unwrap();
        assert_eq!(url.as_str(), "https://proxy.example/prefix/api/v1/health?a=b+c");
    }
}
