//! Discovery and health need no key; with OXINSIDER_API_KEY set this also reads
//! the top of the leaderboard.
//!
//!     cargo run --example discovery
//!     OXINSIDER_API_KEY=oxi_sk_live_... cargo run --example discovery

use oxinsider::{Client, ListLeaderboardParams};

#[tokio::main]
async fn main() -> oxinsider::Result<()> {
    let client = Client::from_env()?;

    let discovery = client.get_api_discovery().await?;
    println!("docs: {}", discovery.data.docs_url);

    let health = client.get_health(&Default::default()).await?;
    println!("health: {:?}", health.data.status);

    if !client.has_credential() {
        println!("set OXINSIDER_API_KEY to read the leaderboard");
        return Ok(());
    }
    let board = client
        .list_leaderboard(&ListLeaderboardParams::default().limit(5))
        .await?;
    for entry in &board.data {
        // An absent grade is an ungraded wallet: uncovered, not unskilled.
        let grade = entry.grade.as_deref().unwrap_or("ungraded");
        println!("{grade:>8}  {}", entry.address);
    }
    Ok(())
}
