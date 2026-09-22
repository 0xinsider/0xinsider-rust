//! Verifying webhook deliveries.
//!
//! The API signs every delivery as
//! `v1=hex(HMAC_SHA256(signing_secret, "<timestamp>.<raw_body>"))` on the
//! `x-0xinsider-signature` header; the Unix-seconds timestamp is on
//! `x-0xinsider-timestamp`. During a staged secret rotation the header carries
//! a comma-separated list, one signature per active secret.
//!
//! Verify the exact bytes you received before parsing them: re-serializing
//! changes the bytes and breaks the HMAC. The endpoint-activation challenge
//! (`{"type": "webhook.verification", ...}`) is signed the same way.
//!
//! ```
//! use oxinsider::webhooks::{compute_signature, verify_signature_at};
//!
//! let body = br#"{"type":"whale_trades_inserted","data":{"count":1}}"#;
//! let signature = compute_signature("whsec_example", "1700000000", body);
//! assert!(verify_signature_at("whsec_example", "1700000000", &signature, body, 300, 1_700_000_100).unwrap());
//! assert!(!verify_signature_at("whsec_example", "1700000000", &signature, b"{}", 300, 1_700_000_100).unwrap());
//! ```

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The header that carries the signature (`v1=<hex>`, possibly a comma-separated list).
pub const SIGNATURE_HEADER: &str = "x-0xinsider-signature";
/// The header that carries the Unix-seconds signing timestamp.
pub const TIMESTAMP_HEADER: &str = "x-0xinsider-timestamp";
/// The published replay tolerance: a delivery more than 300 seconds from now is rejected.
pub const DEFAULT_TOLERANCE_SECONDS: u64 = 300;

type HmacSha256 = Hmac<Sha256>;

/// The receiver is misconfigured, as opposed to the delivery being bad (which
/// is `Ok(false)`). Log it; do not answer the sender as if the delivery were forged.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WebhookConfigError {
    /// The signing secret is empty.
    EmptySecret,
}

impl fmt::Display for WebhookConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySecret => f.write_str("webhook verification needs a non-empty signing secret"),
        }
    }
}

impl std::error::Error for WebhookConfigError {}

fn mac(secret: &str, timestamp: &str, body: &[u8]) -> HmacSha256 {
    // HMAC accepts a key of any length (a long one is hashed), so this cannot fail.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC-SHA256 accepts a key of any length"));
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The signature the API would send for this payload: `v1=<hex>`. For tests
/// and local tooling.
pub fn compute_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let digest = mac(secret, timestamp, body).finalize().into_bytes();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("v1={hex}")
}

/// Verify a delivery against the current time and the published 300-second tolerance.
///
/// `timestamp` and `signature` are the raw header values; `body` is the exact
/// request body. Returns `Ok(false)` for a bad signature, a malformed or stale
/// timestamp, or a malformed header; `Err` only for an empty secret.
pub fn verify_signature(
    secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
) -> Result<bool, WebhookConfigError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    verify_signature_at(secret, timestamp, signature, body, DEFAULT_TOLERANCE_SECONDS, now)
}

/// [`verify_signature`] with an explicit tolerance and "now" (Unix seconds).
///
/// A delivery exactly `tolerance_seconds` away passes and one second further
/// fails; `0` accepts only the current second. Each candidate in the header is
/// compared in constant time.
pub fn verify_signature_at(
    secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    tolerance_seconds: u64,
    now_seconds: i64,
) -> Result<bool, WebhookConfigError> {
    if secret.is_empty() {
        return Err(WebhookConfigError::EmptySecret);
    }
    let timestamp = timestamp.trim();
    let Ok(signed_at) = timestamp.parse::<i64>() else {
        return Ok(false);
    };
    if now_seconds.abs_diff(signed_at) > tolerance_seconds {
        return Ok(false);
    }
    let expected = mac(secret, timestamp, body);
    Ok(signature.split(',').map(str::trim).any(|candidate| {
        let Some(hex) = candidate.strip_prefix("v1=") else {
            return false;
        };
        let Some(bytes) = decode_hex(hex) else {
            return false;
        };
        expected.clone().verify_slice(&bytes).is_ok()
    }))
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test";
    const BODY: &[u8] = br#"{"type":"wallet_grade_changed","data":{"new_grade":"S"}}"#;

    #[test]
    fn matches_the_published_recipe() {
        // Computed independently with Python's hmac and Node's crypto.
        assert_eq!(
            compute_signature("whsec_test", "1700000000", br#"{"type":"wallet_grade_changed"}"#),
            "v1=2473ec017a9a27fe8a8652d988d885f1cb430f0ee40aea80db5c31d9ae31dc41"
        );
    }

    #[test]
    fn accepts_a_valid_signature_and_rejects_a_tampered_body() {
        let signature = compute_signature(SECRET, "1000", BODY);
        assert!(verify_signature_at(SECRET, "1000", &signature, BODY, 300, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &signature, b"{}", 300, 1000).unwrap());
        assert!(!verify_signature_at("other", "1000", &signature, BODY, 300, 1000).unwrap());
    }

    #[test]
    fn tolerance_edges() {
        let signature = compute_signature(SECRET, "1000", BODY);
        assert!(verify_signature_at(SECRET, "1000", &signature, BODY, 300, 1300).unwrap());
        assert!(verify_signature_at(SECRET, "1000", &signature, BODY, 300, 700).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &signature, BODY, 300, 1301).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &signature, BODY, 300, 699).unwrap());
        assert!(verify_signature_at(SECRET, "1000", &signature, BODY, 0, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &signature, BODY, 0, 1001).unwrap());
    }

    #[test]
    fn accepts_any_candidate_in_a_rotation_list() {
        let old = compute_signature("old_secret", "1000", BODY);
        let new = compute_signature(SECRET, "1000", BODY);
        assert!(verify_signature_at(SECRET, "1000", &format!("{old}, {new}"), BODY, 300, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &old, BODY, 300, 1000).unwrap());
    }

    #[test]
    fn malformed_input_is_false_not_an_error() {
        let signature = compute_signature(SECRET, "1000", BODY);
        assert!(!verify_signature_at(SECRET, "soon", &signature, BODY, 300, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", "v1=zz", BODY, 300, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", "", BODY, 300, 1000).unwrap());
        assert!(!verify_signature_at(SECRET, "1000", &signature.replace("v1=", "v2="), BODY, 300, 1000).unwrap());
    }

    #[test]
    fn an_empty_secret_is_a_configuration_error() {
        assert_eq!(
            verify_signature_at("", "1000", "v1=00", BODY, 300, 1000),
            Err(WebhookConfigError::EmptySecret)
        );
    }
}
