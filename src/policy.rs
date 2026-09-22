//! Where a credential may go.
//!
//! The API key travels as a bearer header, so the destination decides who
//! receives it. A credential is sent over `https` anywhere, and over `http` only
//! to a loopback host (a backend on `localhost`, `127.0.0.1` or `[::1]`).
//! Anything else is refused with [`InsecureTransportError`] before a request
//! exists, so a mistyped base URL or a plain-http proxy never sees the key.
//!
//! The check runs when the client is built, again on every credentialed
//! request, and on a download's redirect target. The default HTTP client never
//! follows a redirect on its own, so an `https` answer cannot downgrade a
//! credentialed request to `http`.
//!
//! Owned outside the generated code so regeneration cannot loosen it.

use std::fmt;
use std::net::IpAddr;

use reqwest::Url;

/// A credential would have been sent over plain HTTP to a host that is not
/// loopback. Nothing was sent. `origin` is the refused destination (scheme,
/// host and port); the credential is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InsecureTransportError {
    /// The refused destination: scheme, host and port.
    pub origin: String,
    /// True when the destination was a redirect target rather than the request's own URL.
    pub redirect: bool,
}

impl fmt::Display for InsecureTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = if self.redirect { "a redirect" } else { "the request" };
        write!(
            f,
            "refusing to send the bearer credential with {what} to {}: use https://, or http:// only on a loopback host (localhost, 127.0.0.1, [::1])",
            self.origin
        )
    }
}

impl std::error::Error for InsecureTransportError {}

/// True for `localhost`, an address in `127.0.0.0/8`, or `::1` (bracketed or not).
pub fn is_loopback_host(host: &str) -> bool {
    let name = host.trim_start_matches('[').trim_end_matches(']');
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// True when `url` may receive a credential: `https` anywhere, or `http` to a loopback host.
pub fn is_trusted_destination(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(is_loopback_host),
        _ => false,
    }
}

pub(crate) fn origin_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

pub(crate) fn assert_credential_destination(url: &Url, redirect: bool) -> Result<(), InsecureTransportError> {
    if is_trusted_destination(url) {
        return Ok(());
    }
    Err(InsecureTransportError {
        origin: origin_of(url),
        redirect,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn https_is_trusted_anywhere() {
        assert!(is_trusted_destination(&url("https://api.0xinsider.com")));
        assert!(is_trusted_destination(&url("https://example.com:8443/x")));
    }

    #[test]
    fn http_is_trusted_only_on_loopback() {
        assert!(is_trusted_destination(&url("http://localhost:8080")));
        assert!(is_trusted_destination(&url("http://127.0.0.1")));
        assert!(is_trusted_destination(&url("http://127.8.9.10")));
        assert!(is_trusted_destination(&url("http://[::1]:3000")));
        assert!(!is_trusted_destination(&url("http://api.0xinsider.com")));
        assert!(!is_trusted_destination(&url("http://10.0.0.1")));
        assert!(!is_trusted_destination(&url("http://localhost.evil.com")));
    }

    #[test]
    fn other_schemes_are_refused() {
        assert!(!is_trusted_destination(&url("ftp://localhost")));
    }

    #[test]
    fn the_error_names_the_origin_never_a_token() {
        let error = assert_credential_destination(&url("http://proxy.example:8080/api?key=x"), true).unwrap_err();
        assert_eq!(error.origin, "http://proxy.example:8080");
        assert!(error.to_string().contains("a redirect"));
        assert!(!error.to_string().contains("key=x"));
    }
}
