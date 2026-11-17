//! Publishing binding events to relays and reading them back.
//!
//! Relays are untrusted: every event read back is run through the full binding checks
//! (`SignedStatement::from_event`) and anything that fails is dropped, so a relay can withhold a
//! binding but cannot invent one.

use std::time::Duration;

use mostro_core::lease_order::{KIND_LEASE_ORDER, LeaseOrder};
use mostro_core::node_binding::{
    Action as BindingAction, KIND_NODE_BINDING, Network, SignedStatement,
};
use mostro_core::ring_order::RingOrder;
use nostr_sdk::prelude::{Client, Event, Filter, Kind, PublicKey};
use uuid::Uuid;

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

/// The node `instance` runs on `network`, by its own published binding (66 hex), if the relays
/// hold one. Only statements that verify count: signed by the instance's key and by the node
/// they name. The newest decides, so a revocation ends an older binding. A relay can withhold
/// the binding, but cannot point it at another node.
pub async fn fetch_instance_node(
    instance: &PublicKey,
    network: Network,
    relays: &[String],
) -> Result<Option<String>, String> {
    let client = connected(relays).await?;
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_NODE_BINDING))
        .author(*instance);
    let fetched = client.fetch_events(filter).timeout(RELAY_TIMEOUT).await;
    client.shutdown().await;
    let events = fetched.map_err(|e| format!("fetch: {e}"))?;
    Ok(newest_instance_node(
        events
            .iter()
            .filter(|event| event.pubkey == *instance)
            .filter_map(|event| SignedStatement::from_event(event).ok()),
        instance,
        network,
    ))
}

/// The node the newest of `statements` binds to `instance` on `network`; none if that newest
/// statement is a revocation.
fn newest_instance_node(
    statements: impl Iterator<Item = SignedStatement>,
    instance: &PublicKey,
    network: Network,
) -> Option<String> {
    let instance = instance.to_hex();
    statements
        .map(|signed| signed.statement)
        .filter(|statement| statement.nostr_pubkey == instance && statement.network == network)
        .max_by_key(|statement| statement.created_at)
        .filter(|statement| statement.action == BindingAction::Bind)
        .map(|statement| statement.node_pubkey.to_string())
}

/// The newest lease order `instance` published under `order_id`, if the relays hold one. Only
/// events that verify, are signed by `instance` and name that order count, so a relay can
/// withhold an order or serve an older one, but cannot invent one or change its terms.
pub async fn fetch_lease_order(
    order_id: Uuid,
    instance: &PublicKey,
    relays: &[String],
) -> Result<Option<LeaseOrder>, String> {
    let client = connected(relays).await?;
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_LEASE_ORDER))
        .author(*instance)
        .identifier(order_id.to_string());
    let fetched = client.fetch_events(filter).timeout(RELAY_TIMEOUT).await;
    client.shutdown().await;
    let events = fetched.map_err(|e| format!("fetch: {e}"))?;
    Ok(events
        .iter()
        .filter(|event| event.pubkey == *instance)
        .filter_map(|event| {
            let order = LeaseOrder::from_event(event).ok()?;
            (order.id == order_id).then_some((event.created_at, order))
        })
        .max_by_key(|(created_at, _)| *created_at)
        .map(|(_, order)| order))
}

/// The newest ring order `instance` published under `order_id`, if the relays hold one. As
/// [`fetch_lease_order`]: only events that verify, are signed by `instance` and name that order
/// count.
pub async fn fetch_ring_order(
    order_id: Uuid,
    instance: &PublicKey,
    relays: &[String],
) -> Result<Option<RingOrder>, String> {
    let client = connected(relays).await?;
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_LEASE_ORDER))
        .author(*instance)
        .identifier(order_id.to_string());
    let fetched = client.fetch_events(filter).timeout(RELAY_TIMEOUT).await;
    client.shutdown().await;
    let events = fetched.map_err(|e| format!("fetch: {e}"))?;
    Ok(events
        .iter()
        .filter(|event| event.pubkey == *instance)
        .filter_map(|event| {
            let order = RingOrder::from_event(event).ok()?;
            (order.id == order_id).then_some((event.created_at, order))
        })
        .max_by_key(|(created_at, _)| *created_at)
        .map(|(_, order)| order))
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

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_the_newest_order_the_instance_signed_and_nothing_forged() {
        use mostro_core::lease_message::LeaseOffer;
        use mostro_core::lease_order::LeaseOrderStatus;
        use nostr_sdk::prelude::Timestamp;

        let relay = MockRelay::run().await.expect("relay");
        let relays = vec![relay.url().await.to_string()];
        let (instance, seller, forger) = (Keys::generate(), Keys::generate(), Keys::generate());
        let (_, node) = node_key(1);
        let offer = LeaseOffer {
            network: Network::Regtest,
            seller_node: node.to_string(),
            binding: binding(&seller, 1, Action::Bind, 1_000).as_json(),
            min_capacity_sat: 1_000_000,
            max_capacity_sat: 5_000_000,
            duration_blocks: 4_320,
            fee_base_sat: 1_000,
            fee_ppm: 2_500,
            upfront_percent: 100,
            min_confirmations: 3,
            announce: true,
            expires_at: 2_000_000_000,
            bond_sat: 0,
        };
        let id = Uuid::new_v4();
        let order = |status, at: u64, keys: &Keys| {
            LeaseOrder::from_offer(id, status, &offer)
                .expect("order")
                .to_event_builder()
                .custom_created_at(Timestamp::from(at))
                .finalize(keys)
                .expect("event")
        };

        let none = fetch_lease_order(id, &instance.public_key(), &relays).await;
        assert_eq!(none, Ok(None));

        publish(&order(LeaseOrderStatus::Pending, 1_000, &instance), &relays)
            .await
            .expect("publish");
        // A later "order" under the same id from someone else changes nothing.
        let mut cheaper = offer.clone();
        cheaper.fee_ppm = 1;
        let forged = LeaseOrder::from_offer(id, LeaseOrderStatus::Pending, &cheaper)
            .expect("order")
            .to_event_builder()
            .custom_created_at(Timestamp::from(5_000))
            .finalize(&forger)
            .expect("event");
        publish(&forged, &relays).await.expect("publish");
        let found = fetch_lease_order(id, &instance.public_key(), &relays)
            .await
            .expect("fetch")
            .expect("the order");
        assert_eq!(found.status, LeaseOrderStatus::Pending);
        assert_eq!(found.fee_ppm, 2_500);
        assert_eq!(found.seller_node, node);

        // The instance's own newer status replaces the old one.
        publish(
            &order(LeaseOrderStatus::InProgress, 2_000, &instance),
            &relays,
        )
        .await
        .expect("publish");
        let found = fetch_lease_order(id, &instance.public_key(), &relays)
            .await
            .expect("fetch")
            .expect("the order");
        assert_eq!(found.status, LeaseOrderStatus::InProgress);
        // Another order id is not found.
        let other = fetch_lease_order(Uuid::new_v4(), &instance.public_key(), &relays).await;
        assert_eq!(other, Ok(None));
    }

    #[tokio::test]
    async fn no_relays_is_an_error() {
        let event = binding(&Keys::generate(), 1, Action::Bind, 1_000);
        assert!(publish(&event, &[]).await.is_err());
        assert!(fetch_statements("02", &[]).await.is_err());
    }

    #[test]
    fn the_instances_node_is_the_one_its_newest_binding_names() {
        let instance = Keys::generate();
        let statements = |events: &[Event]| {
            events
                .iter()
                .map(|event| SignedStatement::from_event(event).expect("verifies"))
                .collect::<Vec<_>>()
                .into_iter()
        };
        let node = |n: u8| node_key(n).1.to_string();
        let pick = |events: &[Event]| {
            newest_instance_node(statements(events), &instance.public_key(), Network::Regtest)
        };
        assert_eq!(pick(&[]), None);
        let first = binding(&instance, 1, Action::Bind, 100);
        assert_eq!(pick(std::slice::from_ref(&first)), Some(node(1)));
        // A newer binding wins, whatever order the relays return them in.
        let moved = binding(&instance, 2, Action::Bind, 300);
        assert_eq!(pick(&[moved.clone(), first.clone()]), Some(node(2)));
        assert_eq!(pick(&[first.clone(), moved.clone()]), Some(node(2)));
        // A newer revocation leaves the instance without a node.
        let revoked = binding(&instance, 2, Action::Revoke, 400);
        assert_eq!(pick(&[first.clone(), moved, revoked]), None);
        // Someone else's binding and another network's do not count.
        let other = binding(&Keys::generate(), 3, Action::Bind, 900);
        assert_eq!(pick(&[first.clone(), other]), Some(node(1)));
        assert_eq!(
            newest_instance_node(
                statements(&[first]),
                &instance.public_key(),
                Network::Signet
            ),
            None
        );
    }
}
