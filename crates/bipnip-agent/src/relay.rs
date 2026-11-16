//! Publishing binding events to relays and reading them back.
//!
//! Relays are untrusted: every event read back is run through the full binding checks
//! (`SignedStatement::from_event`) and anything that fails is dropped, so a relay can withhold a
//! binding but cannot invent one.

use std::time::Duration;

use mostro_core::node_binding::{KIND_NODE_BINDING, SignedStatement};
use nostr_sdk::prelude::{Client, Event, Filter, Kind};

/// How long to wait for relays to connect, accept an event or answer a query.
pub const RELAY_TIMEOUT: Duration = Duration::from_secs(20);

/// Which relays took an event and which did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishReport {
    /// Relays that accepted the event.
    pub accepted: Vec<String>,
    /// Relays that refused or did not answer, with the reason.
    pub failed: Vec<(String, String)>,
}

/// A client connected to `relays`, or an error when there are none.
pub(crate) async fn connected(relays: &[String]) -> Result<Client, String> {
    if relays.is_empty() {
        return Err("no relays configured".into());
    }
    let client = Client::default();
    for url in relays {
        client
            .add_relay(url.as_str())
            .await
            .map_err(|e| format!("relay {url}: {e}"))?;
    }
    client.connect().and_wait(RELAY_TIMEOUT).await;
    Ok(client)
}

/// Sends `event` to every relay and reports each one's answer. `Ok` means at least one accepted.
pub async fn publish(event: &Event, relays: &[String]) -> Result<PublishReport, String> {
    let client = connected(relays).await?;
    let sent = tokio::time::timeout(RELAY_TIMEOUT, client.send_event(event)).await;
    client.shutdown().await;
    let output = match sent {
        Err(_) => return Err("no relay answered in time".into()),
        Ok(Err(e)) => return Err(format!("publish: {e}")),
        Ok(Ok(output)) => output,
    };
    let mut report = PublishReport {
        accepted: output.success.keys().map(ToString::to_string).collect(),
        failed: output
            .failed
            .iter()
            .map(|(url, why)| (url.to_string(), why.to_string()))
            .collect(),
    };
    report.accepted.sort();
    report.failed.sort();
    if report.accepted.is_empty() {
        return Err(format!("no relay accepted the event: {:?}", report.failed));
    }
    Ok(report)
}

/// Every verified binding statement the relays hold for `node_pubkey` (66 hex), binds and
/// revocations alike, for `node_binding::is_bound` to resolve.
pub async fn fetch_statements(
    node_pubkey: &str,
    relays: &[String],
) -> Result<Vec<SignedStatement>, String> {
    let client = connected(relays).await?;
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_NODE_BINDING))
        .identifier(node_pubkey);
    let fetched = client.fetch_events(filter).timeout(RELAY_TIMEOUT).await;
    client.shutdown().await;
    let events = fetched.map_err(|e| format!("fetch: {e}"))?;
    let mut statements: Vec<SignedStatement> = events
        .iter()
        .filter_map(|event| SignedStatement::from_event(event).ok())
        .filter(|s| s.statement.node_pubkey.to_string() == node_pubkey)
        .collect();
    statements.sort_by_key(|s| s.statement.created_at);
    statements.dedup();
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::node_binding::{Action, Network, Statement, is_bound, sign_ln_message};
    use nostr_sdk::local_relay::MockRelay;
    use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Keys, Tag};
    use secp256k1::{Secp256k1, SecretKey};

    fn node_key(n: u8) -> (SecretKey, secp256k1::PublicKey) {
        let mut b = [0u8; 32];
        b[31] = n;
        let sk = SecretKey::from_slice(&b).expect("key");
        let pk = secp256k1::PublicKey::from_secret_key(&Secp256k1::signing_only(), &sk);
        (sk, pk)
    }

    fn binding(keys: &Keys, node: u8, action: Action, at: u64) -> Event {
        let (sk, pk) = node_key(node);
        let statement = Statement::new(
            action,
            Network::Regtest,
            &keys.public_key().to_hex(),
            pk,
            at,
        )
        .expect("statement");
        let node_signature = sign_ln_message(statement.text().as_bytes(), &sk);
        SignedStatement {
            statement,
            node_signature,
        }
        .to_event_builder()
        .finalize(keys)
        .expect("sign")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publishes_and_reads_back_verified_statements() {
        let relay = MockRelay::run().await.expect("relay");
        let relays = vec![relay.url().await.to_string()];
        let keys = Keys::generate();
        let (_, node) = node_key(1);
        let node_hex = node.to_string();

        let report = publish(&binding(&keys, 1, Action::Bind, 1_000), &relays)
            .await
            .expect("publish");
        assert_eq!(report.accepted.len(), 1);
        assert!(report.failed.is_empty());

        let seen = fetch_statements(&node_hex, &relays).await.expect("fetch");
        assert_eq!(seen.len(), 1);
        assert!(is_bound(
            &node,
            &keys.public_key().to_hex(),
            Network::Regtest,
            seen.iter(),
            1_000
        ));
        // Another node's bindings are not returned for this node.
        publish(&binding(&keys, 2, Action::Bind, 1_000), &relays)
            .await
            .expect("publish");
        assert_eq!(
            fetch_statements(&node_hex, &relays)
                .await
                .expect("fetch")
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forged_events_on_a_relay_are_dropped() {
        let relay = MockRelay::run().await.expect("relay");
        let relays = vec![relay.url().await.to_string()];
        let (_, node) = node_key(1);
        let node_hex = node.to_string();
        // Same kind and `d` tag, signed by a stranger, with no valid node signature.
        let forged = EventBuilder::new(Kind::Custom(KIND_NODE_BINDING), "")
            .tags([
                Tag::identifier(node_hex.clone()),
                Tag::custom("node", vec![node_hex.clone()]),
                Tag::custom("network", vec!["regtest".to_string()]),
                Tag::custom("statement", vec!["nostr-node-binding:v1:bind".to_string()]),
                Tag::custom("node_sig", vec!["y".repeat(104)]),
            ])
            .finalize(&Keys::generate())
            .expect("sign");
        publish(&forged, &relays)
            .await
            .expect("the relay stores it");
        assert!(
            fetch_statements(&node_hex, &relays)
                .await
                .expect("fetch")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn no_relays_is_an_error() {
        let event = binding(&Keys::generate(), 1, Action::Bind, 1_000);
        assert!(publish(&event, &[]).await.is_err());
        assert!(fetch_statements("02", &[]).await.is_err());
    }
}
