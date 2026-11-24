//! One order book from many instances: the open orders each instance published, filtered to what
//! the caller asks for and sorted by price. Pure: the orders come in, read and verified by
//! [`crate::relay::newest_open_orders`].
//!
//! A lease order's price is its fee for the capacity asked for (or, with none asked, for the
//! largest it offers), per satoshi of capacity per block of the lease, in parts per billion: the
//! cheapest liquidity first, whatever its size and length. Rings cost no fee and come after the
//! leases, largest channel first.

use std::cmp::Ordering;

use mostro_core::node_binding::Network;
use nostr_sdk::prelude::PublicKey;

use crate::relay::OpenOrder;

/// What to list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Query {
    /// Only leases (`Some(true)`) or only rings (`Some(false)`).
    pub leases: Option<bool>,
    /// A lease that offers this capacity, or a ring with channels at least this large, in sat.
    pub capacity_sat: Option<u64>,
    /// Leases whose whole fee, at the capacity priced, is at most this, in sat.
    pub max_fee_sat: Option<u64>,
}

/// An order on the book.
#[derive(Debug, Clone)]
pub struct Listed {
    /// The instance that published it.
    pub instance: PublicKey,
    /// The order.
    pub order: OpenOrder,
    /// For a lease: the capacity priced, its whole fee, and the price in parts per billion per
    /// block.
    pub price: Option<Price>,
    /// For a lease: how many open lease orders its seller's node has on the book, this one
    /// included (the same seller on several instances).
    pub seller_listings: usize,
}

/// A lease's price at one capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    /// The capacity priced, in sat.
    pub capacity_sat: u64,
    /// The whole fee at that capacity, upfront and completion, in sat.
    pub fee_sat: u64,
    /// The fee per sat of capacity per block, in parts per billion.
    pub ppb_per_block: u64,
}

fn price(order: &mostro_core::lease_order::LeaseOrder, capacity_sat: Option<u64>) -> Option<Price> {
    let capacity_sat = capacity_sat.unwrap_or(order.max_capacity_sat);
    let terms = order.terms_for(capacity_sat).ok()?;
    let fee_sat = terms.total_fee_sat().ok()?;
    Some(Price {
        capacity_sat,
        fee_sat,
        ppb_per_block: ppb_per_block(fee_sat, capacity_sat, terms.duration_blocks)?,
    })
}

/// A lease's price as the book shows it: `fee_sat` for `capacity_sat` over `duration_blocks`,
/// as the fee per sat of capacity per block in parts per billion (rounded down). `None` for
/// no capacity or no blocks.
pub fn ppb_per_block(fee_sat: u64, capacity_sat: u64, duration_blocks: u32) -> Option<u64> {
    let per = u128::from(capacity_sat).checked_mul(u128::from(duration_blocks))?;
    let ppb = u128::from(fee_sat)
        .checked_mul(1_000_000_000)?
        .checked_div(per)?;
    Some(u64::try_from(ppb).unwrap_or(u64::MAX))
}

/// The book: `orders` from every instance, keeping those on `network` that fit `query`, sorted
/// as the module says.
pub fn merge(orders: Vec<(PublicKey, OpenOrder)>, network: Network, query: &Query) -> Vec<Listed> {
    let mut listed: Vec<Listed> = Vec::new();
    for (instance, order) in orders {
        let (is_lease, on) = match &order {
            OpenOrder::Lease(order) => (true, order.network),
            OpenOrder::Ring(order) => (false, order.network),
        };
        if on != network || query.leases.is_some_and(|leases| leases != is_lease) {
            continue;
        }
        let price = match &order {
            OpenOrder::Lease(lease) => {
                let Some(price) = price(lease, query.capacity_sat) else {
                    // It does not offer the capacity asked for.
                    continue;
                };
                if query.max_fee_sat.is_some_and(|most| price.fee_sat > most) {
                    continue;
                }
                Some(price)
            }
            OpenOrder::Ring(ring) => {
                if query
                    .capacity_sat
                    .is_some_and(|least| ring.capacity_sat < least)
                {
                    continue;
                }
                None
            }
        };
        listed.push(Listed {
            instance,
            order,
            price,
            seller_listings: 0,
        });
    }
    let sellers: Vec<Option<secp256k1::PublicKey>> = listed.iter().map(seller).collect();
    for (i, entry) in listed.iter_mut().enumerate() {
        if let Some(node) = sellers[i] {
            entry.seller_listings = sellers.iter().filter(|s| **s == Some(node)).count();
        }
    }
    listed.sort_by(order_of);
    listed
}

fn seller(entry: &Listed) -> Option<secp256k1::PublicKey> {
    match &entry.order {
        OpenOrder::Lease(order) => Some(order.seller_node),
        OpenOrder::Ring(_) => None,
    }
}

fn order_of(a: &Listed, b: &Listed) -> Ordering {
    match (&a.order, &b.order, a.price, b.price) {
        (OpenOrder::Lease(x), OpenOrder::Lease(y), Some(p), Some(q)) => p
            .ppb_per_block
            .cmp(&q.ppb_per_block)
            .then(q.capacity_sat.cmp(&p.capacity_sat))
            .then(x.id.cmp(&y.id)),
        (OpenOrder::Lease(_), OpenOrder::Ring(_), _, _) => Ordering::Less,
        (OpenOrder::Ring(_), OpenOrder::Lease(_), _, _) => Ordering::Greater,
        (OpenOrder::Ring(x), OpenOrder::Ring(y), _, _) => {
            y.capacity_sat.cmp(&x.capacity_sat).then(x.id.cmp(&y.id))
        }
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::lease_message::LeaseOffer;
    use mostro_core::lease_order::{LeaseOrder, LeaseOrderStatus};
    use mostro_core::ring_message::RingOffer;
    use mostro_core::ring_order::{RingOrder, RingOrderStatus};
    use nostr_sdk::prelude::Keys;
    use secp256k1::{Secp256k1, SecretKey};
    use serde_json::json;
    use uuid::Uuid;

    fn node(n: u8) -> String {
        let mut b = [0u8; 32];
        b[31] = n;
        let sk = SecretKey::from_slice(&b).expect("key");
        secp256k1::PublicKey::from_secret_key(&Secp256k1::signing_only(), &sk).to_string()
    }

    /// A lease order, built the way an instance builds one: from the seller's offer.
    fn lease(seller: u8, fee_ppm: u32, max_capacity_sat: u64, network: Network) -> OpenOrder {
        let offer: LeaseOffer = serde_json::from_value(json!({
            "network": network.as_str(),
            "seller_node": node(seller),
            "binding": "{}",
            "min_capacity_sat": 100_000,
            "max_capacity_sat": max_capacity_sat,
            "duration_blocks": 1_000,
            "fee_base_sat": 0,
            "fee_ppm": fee_ppm,
            "upfront_percent": 100,
            "min_confirmations": 3,
            "announce": true,
            "expires_at": u64::MAX,
            "bond_sat": 0,
        }))
        .expect("an offer");
        OpenOrder::Lease(
            LeaseOrder::from_offer(Uuid::new_v4(), LeaseOrderStatus::Pending, &offer)
                .expect("an order"),
        )
    }

    fn ring(capacity_sat: u64) -> OpenOrder {
        let offer: RingOffer = serde_json::from_value(json!({
            "network": "regtest",
            "capacity_sat": capacity_sat,
            "slots": 3,
            "announce": true,
            "open_window_secs": 3_600,
            "bond_sat": 0,
            "expires_at": u64::MAX,
            "seat": {"node": node(9), "binding": "{}", "node_address": null},
        }))
        .expect("an offer");
        OpenOrder::Ring(
            RingOrder::from_offer(Uuid::new_v4(), RingOrderStatus::Pending, 1, &offer)
                .expect("an order"),
        )
    }

    fn kinds(book: &[Listed]) -> Vec<String> {
        book.iter()
            .map(|entry| match (&entry.order, entry.price) {
                (OpenOrder::Lease(order), Some(price)) => {
                    format!("lease {} {}", order.fee_ppm, price.capacity_sat)
                }
                (OpenOrder::Ring(order), _) => format!("ring {}", order.capacity_sat),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn the_book_is_sorted_by_price_then_rings_largest_first() {
        let (a, b) = (Keys::generate().public_key(), Keys::generate().public_key());
        let orders = vec![
            (a, ring(500_000)),
            (a, lease(1, 5_000, 1_000_000, Network::Regtest)),
            (b, lease(2, 1_000, 2_000_000, Network::Regtest)),
            (b, ring(2_000_000)),
            (a, lease(3, 1_000, 1_000_000, Network::Regtest)),
            // Another network: never listed.
            (b, lease(4, 1_000, 1_000_000, Network::Signet)),
        ];
        let book = merge(orders, Network::Regtest, &Query::default());
        assert_eq!(
            kinds(&book),
            [
                "lease 1000 2000000",
                "lease 1000 1000000",
                "lease 5000 1000000",
                "ring 2000000",
                "ring 500000"
            ]
        );
        // 1 000 ppm over 1 000 blocks: 1 000 parts per billion per block.
        assert_eq!(book[0].price.map(|p| p.ppb_per_block), Some(1_000));
        assert_eq!(book[0].instance, b);
    }

    #[test]
    fn filters_keep_what_the_caller_asked_for() {
        let i = Keys::generate().public_key();
        let orders = || {
            vec![
                (i, lease(1, 1_000, 1_000_000, Network::Regtest)),
                (i, lease(2, 9_000, 5_000_000, Network::Regtest)),
                (i, ring(1_000_000)),
                (i, ring(300_000)),
            ]
        };
        let only = |query: Query| kinds(&merge(orders(), Network::Regtest, &query));
        assert_eq!(
            only(Query {
                leases: Some(false),
                ..Query::default()
            }),
            ["ring 1000000", "ring 300000"]
        );
        // Two million sat: only the second lease offers it; rings at least that large: none.
        assert_eq!(
            only(Query {
                capacity_sat: Some(2_000_000),
                ..Query::default()
            }),
            ["lease 9000 2000000"]
        );
        // At 500 000 sat the first lease costs 500, the second 4 500.
        assert_eq!(
            only(Query {
                leases: Some(true),
                capacity_sat: Some(500_000),
                max_fee_sat: Some(1_000),
            }),
            ["lease 1000 500000"]
        );
    }

    #[test]
    fn a_seller_on_several_instances_is_counted_on_each_listing() {
        let (a, b) = (Keys::generate().public_key(), Keys::generate().public_key());
        let orders = vec![
            (a, lease(7, 1_000, 1_000_000, Network::Regtest)),
            (b, lease(7, 2_000, 1_000_000, Network::Regtest)),
            (b, lease(8, 3_000, 1_000_000, Network::Regtest)),
            (a, ring(1_000_000)),
        ];
        let book = merge(orders, Network::Regtest, &Query::default());
        let counts: Vec<usize> = book.iter().map(|entry| entry.seller_listings).collect();
        assert_eq!(counts, [2, 2, 1, 0]);
    }

    #[test]
    fn orders_are_told_apart_by_instance_and_only_the_newest_version_counts() {
        use nostr_sdk::prelude::{FinalizeEvent, Timestamp};
        let (a, b, stranger) = (Keys::generate(), Keys::generate(), Keys::generate());
        let OpenOrder::Lease(order) = lease(1, 1_000, 1_000_000, Network::Regtest) else {
            panic!("a lease");
        };
        let at = |keys: &Keys, order: &LeaseOrder, secs: u64| {
            order
                .to_event_builder()
                .custom_created_at(Timestamp::from(secs))
                .finalize(keys)
                .expect("signs")
        };
        let mut taken = order.clone();
        taken.status = LeaseOrderStatus::InProgress;
        let events = vec![
            at(&a, &order, 100),
            // The same order id from another instance is another order.
            at(&b, &order, 100),
            // A stranger's copy is not an instance's order.
            at(&stranger, &order, 100),
            // a's order was taken later: no longer open.
            at(&a, &taken, 200),
        ];
        let found = crate::relay::newest_open_orders(&events, &[a.public_key(), b.public_key()], 0);
        let from: Vec<PublicKey> = found.iter().map(|(instance, _)| *instance).collect();
        assert_eq!(from, [b.public_key()]);
    }
}
