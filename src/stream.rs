//! Incremental reader for `GET /api/v1/stream`.
//!
//! The endpoint answers an unbounded `text/event-stream`. [`Client::open_stream`]
//! delivers each frame as it arrives, bounds the bytes held for one
//! undelivered frame, and closes the connection when the reader is dropped.
//!
//! Wire format (the OpenAPI document, `paths./api/v1/stream`, response 200):
//! - a data frame is `id: <seq>\ndata: <json-envelope>\n\n`, where the envelope
//!   is `{seq, published_at, type, ...event-specific}` and the SSE id equals `seq`;
//! - a resync marker is `event: resync\nid: <seq>\ndata: <json>\n\n`, where the
//!   JSON is `{type: "resync", completeness, from_sequence, to_sequence}`: the
//!   resume point is outside the retained window or a live gap was observed, so
//!   refetch current state;
//! - an idle connection sends `: keep-alive` comment frames.
//!
//! Protocol validity matches the Go and TypeScript clients: a 2xx that is not
//! `text/event-stream`, a data frame whose payload is not a JSON object, a frame
//! with no usable sequence, a malformed resync marker, and a frame past
//! `max_frame_bytes` without its delimiter each end the stream with a
//! [`StreamProtocolError`]. A malformed frame never moves `last_seq`.

use std::fmt;
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde::de::DeserializeOwned;

use crate::client::{Call, Client, api_error};
use crate::error::{Error, Result};
use crate::models::Grade;
use crate::{Operation, RetryClass};

/// Bytes held for one undelivered frame by default: 1 MiB. The largest real frame is a few KB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1 << 20;

/// Options for [`Client::open_stream`]. Start from `Default::default()`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StreamOptions {
    /// Resume after this sequence: the last `seq` you processed. The stream
    /// replays the strictly newer retained window, then continues live. `None`
    /// attaches live from now.
    pub last_event_id: Option<i64>,
    /// Only these frame types (`WhaleTradesInserted`, `wallet_grade_changed`, ...).
    /// Empty means every type.
    pub events: Vec<String>,
    /// Only frames for this market (a condition id or `mkt_` id). Frames that carry no market are excluded.
    pub condition_id: Option<String>,
    /// Only frames whose grade is this or better. Frames that carry no grade are excluded.
    pub min_grade: Option<Grade>,
    /// Bytes held for one undelivered frame; past this without a delimiter the
    /// stream ends with [`StreamProtocolErrorReason::FrameTooLarge`].
    pub max_frame_bytes: usize,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            last_event_id: None,
            events: Vec::new(),
            condition_id: None,
            min_grade: None,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

impl StreamOptions {
    /// Sets `last_event_id`.
    #[must_use]
    pub fn last_event_id(mut self, seq: i64) -> Self {
        self.last_event_id = Some(seq);
        self
    }

    /// Sets `events`.
    #[must_use]
    pub fn events<I, S>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.events = events.into_iter().map(Into::into).collect();
        self
    }

    /// Sets `condition_id`.
    #[must_use]
    pub fn condition_id(mut self, condition_id: impl Into<String>) -> Self {
        self.condition_id = Some(condition_id.into());
        self
    }

    /// Sets `min_grade`.
    #[must_use]
    pub fn min_grade(mut self, grade: Grade) -> Self {
        self.min_grade = Some(grade);
        self
    }

    /// Sets `max_frame_bytes`.
    #[must_use]
    pub fn max_frame_bytes(mut self, bytes: usize) -> Self {
        self.max_frame_bytes = bytes;
        self
    }
}

/// What the stream did that the SSE contract does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamProtocolErrorReason {
    /// A 2xx whose `Content-Type` is not `text/event-stream`.
    UnexpectedMediaType,
    /// A data frame whose payload is not JSON, or is empty.
    InvalidJson,
    /// A data frame whose payload is JSON but not an object.
    InvalidEnvelope,
    /// A data frame with no integer `seq` in the envelope and no integer SSE id.
    UnusableSequence,
    /// A resync frame whose payload is not an object, or whose `type` is not `resync`.
    InvalidResync,
    /// A frame grew past `max_frame_bytes` without reaching its delimiter.
    FrameTooLarge,
}

impl StreamProtocolErrorReason {
    /// The reason as the other official clients spell it (`frame_too_large`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedMediaType => "unexpected_media_type",
            Self::InvalidJson => "invalid_json",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::UnusableSequence => "unusable_sequence",
            Self::InvalidResync => "invalid_resync",
            Self::FrameTooLarge => "frame_too_large",
        }
    }
}

/// The stream broke the SSE contract. The connection is closed.
///
/// The raw payload is deliberately not carried: it may hold data you would not
/// want in a log, and `frame_id` plus `last_seq` name the frame exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamProtocolError {
    /// What was wrong.
    pub reason: StreamProtocolErrorReason,
    /// The last sequence delivered on this connection. No malformed frame moves it.
    pub last_seq: Option<i64>,
    /// The SSE id of the offending frame, when it carried one.
    pub frame_id: Option<i64>,
    /// The offending frame's SSE event name.
    pub event: Option<String>,
    /// The offending frame's size in bytes.
    pub bytes: usize,
    /// The response's `Content-Type`, for [`StreamProtocolErrorReason::UnexpectedMediaType`].
    pub media_type: Option<String>,
}

impl fmt::Display for StreamProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = match self.frame_id {
            Some(id) => format!("frame id {id}"),
            None => "a frame".to_owned(),
        };
        let after = match self.last_seq {
            Some(seq) => format!("after seq {seq}"),
            None => "before any event was delivered".to_owned(),
        };
        match self.reason {
            StreamProtocolErrorReason::UnexpectedMediaType => match &self.media_type {
                Some(media) => write!(
                    f,
                    "oxinsider stream answered Content-Type {media} instead of text/event-stream"
                ),
                None => write!(
                    f,
                    "oxinsider stream answered with no Content-Type instead of text/event-stream"
                ),
            },
            StreamProtocolErrorReason::InvalidJson => {
                write!(
                    f,
                    "oxinsider stream sent {place} whose data is not JSON ({} bytes) {after}",
                    self.bytes
                )
            }
            StreamProtocolErrorReason::InvalidEnvelope => {
                write!(
                    f,
                    "oxinsider stream sent {place} whose data is not an envelope object {after}"
                )
            }
            StreamProtocolErrorReason::UnusableSequence => {
                write!(
                    f,
                    "oxinsider stream sent {place} with no integer seq and no integer id {after}"
                )
            }
            StreamProtocolErrorReason::InvalidResync => {
                write!(
                    f,
                    "oxinsider stream sent a resync marker ({place}) whose data is not a resync object {after}"
                )
            }
            StreamProtocolErrorReason::FrameTooLarge => write!(
                f,
                "oxinsider stream sent a frame past {} bytes with no delimiter {after}; the connection was closed",
                self.bytes
            ),
        }
    }
}

impl std::error::Error for StreamProtocolError {}

/// One delivered frame.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StreamFrame {
    /// True for an `event: resync` marker. `data` is then the resync object
    /// (`{type, completeness, from_sequence, to_sequence}`): refetch current
    /// state and continue live.
    pub resync: bool,
    /// The cluster-shared sequence: the envelope's `seq`, else the SSE id.
    /// Pass the last one you processed as [`StreamOptions::last_event_id`] to
    /// resume. Always set on a data frame; a resync marker may carry none.
    pub seq: Option<i64>,
    /// The envelope's wire type (`WhaleTradesInserted`, `wallet_grade_changed`,
    /// ...), or `resync` for a marker.
    pub event_type: String,
    /// The envelope's `published_at` (RFC 3339), when present.
    pub published_at: Option<String>,
    /// The frame's JSON object.
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl StreamFrame {
    /// Decode the frame into the type for its `event_type`.
    pub fn decode<T: DeserializeOwned>(&self) -> std::result::Result<T, serde_json::Error> {
        serde_json::from_value(serde_json::Value::Object(self.data.clone()))
    }
}

/// Reads the frames of one stream connection. Dropping it closes the connection.
#[derive(Debug)]
pub struct StreamReader {
    response: Option<reqwest::Response>,
    headers: HeaderMap,
    buffer: Vec<u8>,
    max_frame_bytes: usize,
    last_seq: Option<i64>,
    retry: Option<Duration>,
    closed: bool,
}

#[derive(Default)]
struct PendingFrame {
    event: Option<String>,
    id: Option<i64>,
    data: Vec<u8>,
    has_data: bool,
    saw_field: bool,
    bytes: usize,
}

impl Client {
    /// Open `GET /api/v1/stream` and return a reader that delivers frames as they arrive.
    ///
    /// A status other than 200 is an [`Error::Api`] (a 429 carries
    /// `retry_after`); a 200 that is not `text/event-stream` is an
    /// [`Error::Stream`]. The stream has no deadline.
    ///
    /// ```no_run
    /// # async fn run() -> oxinsider::Result<()> {
    /// use oxinsider::{Client, StreamOptions};
    ///
    /// let client = Client::from_env()?;
    /// let mut cursor: Option<i64> = None; // the last seq you processed
    /// let mut options = StreamOptions::default().events(["WhaleTradesInserted"]);
    /// options.last_event_id = cursor;
    /// let mut reader = client.open_stream(&options).await?;
    /// while let Some(frame) = reader.next().await? {
    ///     if frame.resync {
    ///         // The resume point is outside the retained window: refetch state, then continue.
    ///         continue;
    ///     }
    ///     cursor = frame.seq;
    ///     println!("{} {:?}", frame.event_type, frame.seq);
    /// }
    /// // The server closed the stream: reconnect with `cursor`.
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open_stream(&self, options: &StreamOptions) -> Result<StreamReader> {
        if options.max_frame_bytes == 0 {
            return Err(Error::Config("max_frame_bytes must be at least 1".to_owned()));
        }
        let mut call = Call::new(&Operation::GET_STREAM, String::from("/api/v1/stream"));
        call.retry = RetryClass::Never;
        if !options.events.is_empty() {
            call.query("event", options.events.join(","));
        }
        if let Some(condition_id) = &options.condition_id {
            call.query("condition_id", condition_id.clone());
        }
        if let Some(grade) = &options.min_grade {
            call.query("min_grade", grade.as_str().to_owned());
        }
        let last_event_id = options.last_event_id.map(|seq| seq.to_string());
        call.header("Last-Event-ID", last_event_id.as_deref());
        let url = self.url(&call.path, &call.query)?;
        let request = self.build_request(&call, &url, None)?;
        let response = self.http().execute(request).await?;
        if response.status().as_u16() != 200 {
            return Err(api_error(response).await);
        }
        let media = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let is_event_stream = media
            .as_deref()
            .and_then(|m| m.split(';').next())
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"));
        if !is_event_stream {
            return Err(Error::Stream(StreamProtocolError {
                reason: StreamProtocolErrorReason::UnexpectedMediaType,
                last_seq: None,
                frame_id: None,
                event: None,
                bytes: 0,
                media_type: media,
            }));
        }
        let headers = response.headers().clone();
        Ok(StreamReader {
            response: Some(response),
            headers,
            buffer: Vec::new(),
            max_frame_bytes: options.max_frame_bytes,
            last_seq: None,
            retry: None,
            closed: false,
        })
    }
}

impl StreamReader {
    /// The stream response's headers (`X-Request-Id`, the rate-limit headers).
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The sequence of the last data frame delivered on this connection. Resend
    /// it as [`StreamOptions::last_event_id`] to resume. A malformed frame never moves it.
    pub fn last_seq(&self) -> Option<i64> {
        self.last_seq
    }

    /// The reconnection delay the server last sent in an SSE `retry:` field. The
    /// API does not send one today; also honor `retry_after` on a refused reconnect.
    pub fn retry_hint(&self) -> Option<Duration> {
        self.retry
    }

    /// Close the connection. `next` returns `Ok(None)` afterwards.
    pub fn close(&mut self) {
        self.response = None;
        self.buffer = Vec::new();
        self.closed = true;
    }

    /// Wait for the next frame.
    ///
    /// Returns `Ok(None)` when the server closes the stream cleanly (a partial
    /// frame at the end is discarded, as the SSE specification says) or after
    /// [`close`](Self::close). Keep-alive comments are consumed silently. Every
    /// error is terminal: the connection is closed and later calls return `Ok(None)`.
    pub async fn next(&mut self) -> Result<Option<StreamFrame>> {
        if self.closed {
            return Ok(None);
        }
        let mut frame = PendingFrame::default();
        loop {
            let line = match self.read_line(frame.bytes).await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.close();
                    return Ok(None);
                }
                Err(error) => {
                    self.close();
                    return Err(match error {
                        Error::Stream(mut protocol) => {
                            protocol.last_seq = self.last_seq;
                            protocol.frame_id = frame.id;
                            protocol.event = frame.event;
                            Error::Stream(protocol)
                        }
                        other => other,
                    });
                }
            };
            frame.bytes += line.len() + 1;
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if line.is_empty() {
                if !frame.saw_field {
                    frame = PendingFrame::default();
                    continue;
                }
                return match self.decode(frame) {
                    Ok(delivered) => {
                        if !delivered.resync {
                            self.last_seq = delivered.seq;
                        }
                        Ok(Some(delivered))
                    }
                    Err(error) => {
                        self.close();
                        Err(Error::Stream(error))
                    }
                };
            }
            if line[0] == b':' {
                continue;
            }
            let (field, value) = split_field(line);
            match field {
                b"event" => {
                    frame.event = Some(String::from_utf8_lossy(value).into_owned());
                    frame.saw_field = true;
                }
                b"id" => {
                    // The API's ids are decimal; a non-integer id is ignored.
                    frame.id = std::str::from_utf8(value)
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .or(frame.id);
                    frame.saw_field = true;
                }
                b"data" => {
                    if frame.has_data {
                        frame.data.push(b'\n');
                    }
                    frame.data.extend_from_slice(value);
                    frame.has_data = true;
                    frame.saw_field = true;
                }
                b"retry" => {
                    if let Some(ms) = std::str::from_utf8(value).ok().and_then(|v| v.parse::<u64>().ok()) {
                        self.retry = Some(Duration::from_millis(ms));
                    }
                    frame.saw_field = true;
                }
                _ => {} // An unknown field is ignored, per the specification.
            }
        }
    }

    /// The next line without its terminator, or `None` at the end of the stream.
    /// Fails with `FrameTooLarge` once the frame so far plus the pending line
    /// passes the limit without a delimiter.
    async fn read_line(&mut self, frame_bytes: usize) -> Result<Option<Vec<u8>>> {
        let mut searched = 0;
        loop {
            if let Some(offset) = self.buffer[searched..].iter().position(|&b| b == b'\n') {
                let end = searched + offset;
                if frame_bytes + end + 1 > self.max_frame_bytes {
                    return Err(self.too_large(frame_bytes + end + 1));
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                return Ok(Some(line));
            }
            searched = self.buffer.len();
            if frame_bytes + self.buffer.len() > self.max_frame_bytes {
                return Err(self.too_large(frame_bytes + self.buffer.len()));
            }
            let Some(response) = self.response.as_mut() else {
                return Ok(None);
            };
            match response.chunk().await? {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }

    fn too_large(&self, bytes: usize) -> Error {
        Error::Stream(StreamProtocolError {
            reason: StreamProtocolErrorReason::FrameTooLarge,
            last_seq: self.last_seq,
            frame_id: None,
            event: None,
            bytes,
            media_type: None,
        })
    }

    fn decode(&self, frame: PendingFrame) -> std::result::Result<StreamFrame, StreamProtocolError> {
        let is_resync = frame.event.as_deref() == Some("resync");
        let error = |reason| StreamProtocolError {
            reason,
            last_seq: self.last_seq,
            frame_id: frame.id,
            event: frame.event.clone(),
            bytes: frame.data.len(),
            media_type: None,
        };
        let value: serde_json::Value = match serde_json::from_slice(&frame.data) {
            Ok(value) if frame.has_data => value,
            _ => {
                return Err(error(if is_resync {
                    StreamProtocolErrorReason::InvalidResync
                } else {
                    StreamProtocolErrorReason::InvalidJson
                }));
            }
        };
        let serde_json::Value::Object(object) = value else {
            return Err(error(if is_resync {
                StreamProtocolErrorReason::InvalidResync
            } else {
                StreamProtocolErrorReason::InvalidEnvelope
            }));
        };
        let wire_type = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if is_resync {
            if object.contains_key("type") && wire_type.as_deref() != Some("resync") {
                return Err(error(StreamProtocolErrorReason::InvalidResync));
            }
            return Ok(StreamFrame {
                resync: true,
                seq: frame.id,
                event_type: "resync".to_owned(),
                published_at: None,
                data: object,
            });
        }
        let seq = object.get("seq").and_then(serde_json::Value::as_i64).or(frame.id);
        let Some(seq) = seq else {
            return Err(error(StreamProtocolErrorReason::UnusableSequence));
        };
        let published_at = object
            .get("published_at")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(StreamFrame {
            resync: false,
            seq: Some(seq),
            event_type: wire_type.unwrap_or_default(),
            published_at,
            data: object,
        })
    }
}

fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        None => (line, &[]),
        Some(colon) => {
            let value = &line[colon + 1..];
            (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
        }
    }
}
