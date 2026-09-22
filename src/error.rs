//! Errors this crate returns.

use std::fmt;
use std::time::Duration;

use crate::download::DownloadError;
use crate::pagination::PaginationError;
use crate::policy::InsecureTransportError;
use crate::stream::StreamProtocolError;

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every error this crate returns.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The API answered with a non-2xx status. Branch on [`ApiError::code`] and
    /// [`ApiError::reason`], never on the message.
    #[error(transparent)]
    Api(Box<ApiError>),
    /// The API answered `304 Not Modified` to a request that sent `If-None-Match`:
    /// what you hold is still current.
    #[error("not modified")]
    NotModified {
        /// The `ETag` the API sent back, when it sent one.
        etag: Option<String>,
    },
    /// A credential would have gone over plain HTTP to a host that is not loopback.
    /// Nothing was sent.
    #[error(transparent)]
    InsecureTransport(#[from] InsecureTransportError),
    /// The request produced no HTTP response (DNS, TLS, connect, timeout, reset),
    /// or the body could not be read.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// A 2xx body did not match the type this release expects for it.
    #[error(transparent)]
    Decode(Box<DecodeError>),
    /// The event stream broke the SSE contract. The connection is closed.
    #[error(transparent)]
    Stream(#[from] StreamProtocolError),
    /// A file download could not be completed.
    #[error(transparent)]
    Download(#[from] DownloadError),
    /// A cursor walk received a page that cannot be continued.
    #[error(transparent)]
    Pagination(#[from] PaginationError),
    /// The client was configured with a value it cannot use.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// The API error, when this is one.
    pub fn as_api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(error) => Some(error),
            _ => None,
        }
    }

    /// The HTTP status, when the API answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api(error) => Some(error.status),
            Self::NotModified { .. } => Some(304),
            _ => None,
        }
    }

    /// True for `304 Not Modified`.
    pub fn is_not_modified(&self) -> bool {
        matches!(self, Self::NotModified { .. })
    }
}

impl From<ApiError> for Error {
    fn from(error: ApiError) -> Self {
        Self::Api(Box::new(error))
    }
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        Self::Decode(Box::new(error))
    }
}

/// What kind of failure an [`ApiError`] is, from its HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApiErrorKind {
    /// 400: a parameter is invalid (see [`ApiError::param`]).
    BadRequest,
    /// 401: the credential is missing, invalid, expired or revoked.
    Authentication,
    /// 402: the account needs an active Pro subscription for this operation.
    SubscriptionRequired,
    /// 403: the credential cannot use this operation (for OAuth, a missing scope).
    PermissionDenied,
    /// 404: the resource does not exist, or is not released yet (see [`ApiError::reason`]).
    NotFound,
    /// 408: the server did not answer inside its own timeout. A timed-out write
    /// may have completed: check its state and reuse its `Idempotency-Key`.
    RequestTimeout,
    /// 409: the request conflicts with the resource's state.
    Conflict,
    /// 422: the request is well formed but cannot be applied (for example an
    /// `Idempotency-Key` reused with a different body).
    Unprocessable,
    /// 423: the account is locked.
    AccountLocked,
    /// 429: a rate limit or quota was exceeded. Wait [`ApiError::retry_after`].
    RateLimited,
    /// 5xx: the API failed. A 503 carries `retry_after` when retrying is safe.
    Server,
    /// Any other status.
    Other,
}

impl ApiErrorKind {
    /// The kind for an HTTP status.
    pub fn from_status(status: u16) -> Self {
        match status {
            400 => Self::BadRequest,
            401 => Self::Authentication,
            402 => Self::SubscriptionRequired,
            403 => Self::PermissionDenied,
            404 => Self::NotFound,
            408 => Self::RequestTimeout,
            409 => Self::Conflict,
            422 => Self::Unprocessable,
            423 => Self::AccountLocked,
            429 => Self::RateLimited,
            500..=599 => Self::Server,
            _ => Self::Other,
        }
    }
}

/// A non-2xx answer from the API, decoded from its JSON error envelope.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// The stable error class: `bad_request`, `invalid_api_key`,
    /// `subscription_required`, `forbidden`, `insufficient_scope`, `not_found`,
    /// `account_locked`, `rate_limited`, `rate_limit_unavailable`,
    /// `internal_error` or `request_timeout`. `None` when the body carried none
    /// (a transport-level 408 has an empty body).
    pub code: Option<String>,
    /// The specific cause, when the API sends one (`cursor_expired`,
    /// `pick_not_released`, `read_model_warming`, ...). Branch on it first.
    pub reason: Option<String>,
    /// Human-readable prose. Do not branch on it.
    pub message: String,
    /// The request parameter at fault, for a 400.
    pub param: Option<String>,
    /// A documentation link for this error.
    pub doc_url: Option<String>,
    /// The recommended next request instant (RFC 3339), on retryable errors.
    pub retry_at: Option<String>,
    /// The `Retry-After` header: how long to wait before retrying.
    pub retry_after: Option<Duration>,
    /// The request id (`meta.request_id`, else the `X-Request-Id` header). Quote it to support.
    pub request_id: Option<String>,
    /// The decoded body, or the body text when it was not JSON.
    pub body: Option<serde_json::Value>,
}

impl ApiError {
    /// What kind of failure this is, from the status.
    pub fn kind(&self) -> ApiErrorKind {
        ApiErrorKind::from_status(self.status)
    }

    /// True for a 429.
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}", self.status, self.code.as_deref().unwrap_or("error"))?;
        if let Some(reason) = &self.reason {
            write!(f, " ({reason})")?;
        }
        write!(f, ": {}", self.message)?;
        if let Some(request_id) = &self.request_id {
            write!(f, " [request {request_id}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// A 2xx body that did not match the type this release expects.
///
/// `path` names the JSON location that failed (`data[3].grade`). The body is
/// kept for inspection and never printed by `Display`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DecodeError {
    /// The operation id (`listLeaderboard`), or the method and path for a raw request.
    pub operation: String,
    /// The HTTP status of the response.
    pub status: u16,
    /// Where in the body decoding failed.
    pub path: String,
    /// What serde reported.
    pub message: String,
    /// The response body.
    pub body: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} answered HTTP {} with a body this release cannot decode at `{}`: {}",
            self.operation, self.status, self.path, self.message
        )
    }
}

impl std::error::Error for DecodeError {}
