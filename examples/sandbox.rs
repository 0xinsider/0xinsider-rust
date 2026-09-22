//! Build against the sandbox first: no key, example data, and any documented
//! error on demand.
//!
//!     cargo run --example sandbox

use oxinsider::{Client, Error, ListLeaderboardParams, Method};

#[tokio::main]
async fn main() -> oxinsider::Result<()> {
    let client = Client::sandbox()?;

    let board = client
        .list_leaderboard(&ListLeaderboardParams::default().limit(3))
        .await?;
    for entry in &board.data {
        println!("{:?} {}", entry.username, entry.address);
    }

    // Ask the sandbox for the 429 the operation documents.
    let limited = client
        .request_json::<serde_json::Value>(Method::GET, "/api/v1/leaderboard", &[("sandbox_status", "429")], None)
        .await;
    match limited {
        Err(Error::Api(error)) if error.is_rate_limited() => {
            println!(
                "rate limited: code {:?}, retry after {:?}",
                error.code, error.retry_after
            );
        }
        other => println!("unexpected: {other:?}"),
    }
    Ok(())
}
