//! A throwaway Nostr relay on 127.0.0.1 for the agent end-to-end run. Usage: test-relay PORT

use nostr_sdk::local_relay::LocalRelay;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .expect("usage: test-relay PORT");
    let relay = LocalRelay::builder().port(port).build();
    relay.run().await.expect("relay starts");
    println!("relay listening on {}", relay.url().await);
    let _ = tokio::signal::ctrl_c().await;
}
