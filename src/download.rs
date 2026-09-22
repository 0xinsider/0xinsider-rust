//! Redirect-only operations: a finished trader export (302 to the file host)
//! and the OpenAPI document (307 to the web origin).
//!
//! The API request carries the credential. The redirect's `Location` is fetched
//! by a fresh request that carries none, so the bearer never reaches the file
//! host. Only `https` locations are followed (`http` for a loopback host), and
//! one hop only.

use std::fmt;
use std::path::Path;

use bytes::Bytes;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, ETAG, LOCATION, USER_AGENT};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::client::{Call, Client, api_error, header_string};
use crate::error::{Error, Result};
use crate::policy::{is_trusted_destination, origin_of};

/// `Download::bytes` refuses a file larger than this unless told otherwise: 64 MiB.
pub const DEFAULT_MAX_READ_BYTES: usize = 64 << 20;

/// Why a download failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DownloadErrorReason {
    /// The redirect pointed at plain HTTP on a host that is not loopback. Not followed.
    InsecureLocation,
    /// The file host redirected again; only one hop is followed.
    UnexpectedRedirect,
    /// The file host answered with an error. A 403 means the location expired:
    /// call the download operation again for a fresh one.
    Unavailable,
    /// The file is larger than the limit given to `bytes`.
    TooLarge,
    /// The transfer failed part way.
    Interrupted,
    /// The API answered 2xx instead of a redirect.
    NotRedirected,
    /// The API redirected without a `Location` header.
    MissingLocation,
}

/// A download that could not be completed. The signed file URL is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DownloadError {
    /// Why it failed.
    pub reason: DownloadErrorReason,
    /// What happened, in prose.
    pub message: String,
    /// The HTTP status involved, when there was one.
    pub status: Option<u16>,
    /// The file host (scheme and authority), when the redirect was read.
    pub host: Option<String>,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DownloadError {}

fn download_error(reason: DownloadErrorReason, message: String, status: Option<u16>, host: Option<String>) -> Error {
    Error::Download(DownloadError {
        reason,
        message,
        status,
        host,
    })
}

/// What [`Download::save`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SavedDownload {
    /// Bytes written.
    pub bytes_written: u64,
    /// Lowercase hex SHA-256 of exactly the bytes written. No checksum is
    /// published for an export; this is the one to record.
    pub sha256: String,
}

/// An open file download. Read it with [`chunk`](Self::chunk), [`bytes`](Self::bytes)
/// or [`save`](Self::save); dropping it closes the connection.
#[derive(Debug)]
pub struct Download {
    response: reqwest::Response,
    host: String,
}

impl Download {
    pub(crate) async fn follow(client: &Client, call: &Call, response: reqwest::Response) -> Result<Self> {
        let status = response.status();
        if !status.is_redirection() {
            if status.is_success() {
                return Err(download_error(
                    DownloadErrorReason::NotRedirected,
                    format!(
                        "{} answered {} instead of a redirect to the file",
                        call.label(),
                        status.as_u16()
                    ),
                    Some(status.as_u16()),
                    None,
                ));
            }
            return Err(api_error(response).await);
        }
        let Some(location) = header_string(response.headers(), LOCATION.as_str()) else {
            return Err(download_error(
                DownloadErrorReason::MissingLocation,
                format!("{} redirected without a Location header", call.label()),
                Some(status.as_u16()),
                None,
            ));
        };
        let location = response.url().join(&location).map_err(|_| {
            download_error(
                DownloadErrorReason::MissingLocation,
                format!("{} redirected to a Location that is not a URL", call.label()),
                Some(status.as_u16()),
                None,
            )
        })?;
        drop(response);
        let host = origin_of(&location);
        if !is_trusted_destination(&location) {
            return Err(download_error(
                DownloadErrorReason::InsecureLocation,
                format!(
                    "refusing to fetch the file from {host}: only https:// (or http:// on a loopback host) is followed"
                ),
                None,
                Some(host),
            ));
        }
        // A fresh request. A caller's own reqwest::Client may carry default
        // headers; the credential and cookies stay on the API origin regardless.
        let mut request = client
            .http()
            .get(location)
            .header(ACCEPT, "*/*")
            .header(USER_AGENT, concat!("0xinsider-rust/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| Error::Transport(error.without_url()))?;
        request.headers_mut().remove(AUTHORIZATION);
        request.headers_mut().remove(COOKIE);
        let file = client.http().execute(request).await.map_err(|error| {
            download_error(
                DownloadErrorReason::Interrupted,
                format!("fetching the file from {host} failed: {}", error.without_url()),
                None,
                Some(host.clone()),
            )
        })?;
        let file_status = file.status();
        if file_status.is_redirection() {
            return Err(download_error(
                DownloadErrorReason::UnexpectedRedirect,
                format!(
                    "{host} answered {} with another redirect; only one hop is followed",
                    file_status.as_u16()
                ),
                Some(file_status.as_u16()),
                Some(host),
            ));
        }
        if !file_status.is_success() {
            let hint = if file_status.as_u16() == 403 {
                "; the download location has expired, call the download operation again for a fresh one"
            } else {
                ""
            };
            return Err(download_error(
                DownloadErrorReason::Unavailable,
                format!("{host} answered {} for the file{hint}", file_status.as_u16()),
                Some(file_status.as_u16()),
                Some(host),
            ));
        }
        Ok(Self { response: file, host })
    }

    /// The file host: scheme and authority, never the signed URL.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The HTTP status of the file response.
    pub fn status(&self) -> u16 {
        self.response.status().as_u16()
    }

    /// The file's `Content-Type`.
    pub fn content_type(&self) -> Option<&str> {
        self.response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
    }

    /// The file's size in bytes, when the host sent it. Absent when the body is
    /// decompressed on the fly.
    pub fn content_length(&self) -> Option<u64> {
        self.response.content_length()
    }

    /// The file host's `ETag`: its object identity, not a content hash.
    pub fn etag(&self) -> Option<&str> {
        self.response.headers().get(ETAG).and_then(|v| v.to_str().ok())
    }

    /// The file name from `Content-Disposition`, when the host sent one.
    pub fn filename(&self) -> Option<String> {
        let disposition = self.response.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
        disposition.split(';').map(str::trim).find_map(|part| {
            let value = part.strip_prefix("filename=")?;
            let name = value.trim_matches('"');
            (!name.is_empty() && !name.contains(['/', '\\'])).then(|| name.to_owned())
        })
    }

    /// The next chunk of the file, or `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.response.chunk().await.map_err(|error| {
            download_error(
                DownloadErrorReason::Interrupted,
                format!("the transfer from {} failed: {}", self.host, error.without_url()),
                None,
                Some(self.host.clone()),
            )
        })
    }

    /// The whole file in memory, refusing more than `max_bytes`
    /// ([`DEFAULT_MAX_READ_BYTES`] is a sensible limit).
    pub async fn bytes(mut self, max_bytes: usize) -> Result<Bytes> {
        let mut out = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            if out.len() + chunk.len() > max_bytes {
                return Err(download_error(
                    DownloadErrorReason::TooLarge,
                    format!(
                        "the file from {} is larger than {max_bytes} bytes; save it instead",
                        self.host
                    ),
                    None,
                    Some(self.host.clone()),
                ));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(out))
    }

    /// Stream the file to `path` and return how many bytes were written and
    /// their SHA-256. Memory stays bounded whatever the size.
    pub async fn save(mut self, path: impl AsRef<Path>) -> Result<SavedDownload> {
        let path = path.as_ref();
        let io_error = |error: std::io::Error| {
            download_error(
                DownloadErrorReason::Interrupted,
                format!("writing {} failed: {error}", path.display()),
                None,
                None,
            )
        };
        let mut file = tokio::fs::File::create(path).await.map_err(io_error)?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        while let Some(chunk) = self.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(io_error)?;
            written += chunk.len() as u64;
        }
        file.flush().await.map_err(io_error)?;
        let digest = hasher.finalize();
        let sha256 = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(SavedDownload {
            bytes_written: written,
            sha256,
        })
    }
}
