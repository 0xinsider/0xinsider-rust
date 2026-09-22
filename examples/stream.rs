//! Read the live event stream and resume from the last sequence after a disconnect.
//!
//!     OXINSIDER_API_KEY=oxi_sk_live_... cargo run --example stream

use std::time::Duration;

use oxinsider::{Client, Error, StreamOptions};

#[tokio::main]
async fn main() -> oxinsider::Result<()> {
    let client = Client::from_env()?;
    let mut cursor: Option<i64> = None;
    loop {
        let mut options = StreamOptions::default().events(["WhaleTradesInserted", "wallet_grade_changed"]);
        options.last_event_id = cursor;
        let mut reader = match client.open_stream(&options).await {
            Ok(reader) => reader,
            Err(Error::Api(error)) if error.is_rate_limited() => {
                tokio::time::sleep(error.retry_after.unwrap_or(Duration::from_secs(5))).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        while let Some(frame) = reader.next().await? {
            if frame.resync {
                println!("resync: the resume point is outside the retained window; refetch current state");
                continue;
            }
            cursor = frame.seq;
            println!("{:>10} {}", frame.seq.unwrap_or_default(), frame.event_type);
        }
        // The server closed the stream cleanly: reconnect after the last delivered seq.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
