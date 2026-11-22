//! The requests that only read the relays (`orders`, `record`), answered beside the engine rather
//! than by it. They touch no run, no key and no node; a relay can take up to
//! [`crate::relay::RELAY_TIMEOUT`] to answer, and while the engine waited for one it stepped no run
//! and answered no other request, an `approve` or a `deny` included. A [`Reader`] is a copy of what
//! such a request needs, handed to a task of its own.

use std::collections::{BTreeMap, BTreeSet};

use mostro_core::node_binding::Network;
use nostr_sdk::prelude::{Client, PublicKey};
use serde_json::{Value, json};

use super::api::{ApiError, Code, OrderKind, OrdersParams};
use crate::book;
use crate::lease::cmd::unix_now;
use crate::record::{self, Record};
use crate::relay::{self, OpenOrder};

/// What a read needs, copied out of the engine.
#[derive(Clone)]
pub struct Reader {
    /// The daemon's relay connection (a handle to it: the connection is shared).
    pub client: Client,
    /// The instances that are usable, in the order configured. They are the trusted authors of
    /// outcomes, and whose order books are read.
    pub instances: Vec<PublicKey>,
    /// The agent's network: orders on any other are left out.
    pub network: Network,
}

/// An order as a client sees it.
pub fn order_json(instance: &PublicKey, order: &OpenOrder) -> Value {
    match order {
        OpenOrder::Lease(order) => json!({
            "kind": "lease",
            "instance": instance.to_hex(),
            "order_id": order.id.to_string(),
            "seller_node": order.seller_node.to_string(),
            "min_capacity_sat": order.min_capacity_sat,
            "max_capacity_sat": order.max_capacity_sat,
            "duration_blocks": order.duration_blocks,
            "fee_base_sat": order.fee_base_sat,
            "fee_ppm": order.fee_ppm,
            "upfront_percent": order.upfront_percent,
            "min_confirmations": order.min_confirmations,
            "announce": order.announce,
            "bond_sat": order.bond_sat,
            "rules": order.rules,
            "expires_at": order.expires_at,
        }),
        OpenOrder::Ring(order) => json!({
            "kind": "ring",
            "instance": instance.to_hex(),
            "order_id": order.id.to_string(),
            "capacity_sat": order.capacity_sat,
            "slots": order.slots,
            "taken": order.taken,
            "announce": order.announce,
            "open_window_secs": order.open_window_secs,
            "bond_sat": order.bond_sat,
            "rules": order.rules,
            "expires_at": order.expires_at,
        }),
    }
}

impl Reader {
    /// One book over `only`, or over every usable instance: a single request through the shared
    /// relay connection, the newest version of each order, filtered and sorted
    /// ([`book::merge`]). With `records`, each lease also carries its seller's record, from one
    /// more request for all the sellers at once; if that one fails, the book is still answered and
    /// says so in `records_failed`.
    pub async fn orders(
        &self,
        params: &OrdersParams,
        only: Option<PublicKey>,
    ) -> Result<Value, ApiError> {
        let instances = match only {
            Some(key) => vec![key],
            None => self.instances.clone(),
        };
        let books = relay::fetch_books(&self.client, &instances, unix_now())
            .await
            .map_err(|e| ApiError::new(Code::Relay, e))?;
        let query = book::Query {
            leases: params.kind.map(|kind| kind == OrderKind::Lease),
            capacity_sat: params.capacity_sat,
            max_fee_sat: params.max_fee_sat,
        };
        let listed = book::merge(books.orders, self.network, &query);
        let mut orders: Vec<Value> = listed
            .iter()
            .map(|listed| {
                let mut value = order_json(&listed.instance, &listed.order);
                if let Some(price) = listed.price {
                    value["priced_capacity_sat"] = json!(price.capacity_sat);
                    value["fee_sat"] = json!(price.fee_sat);
                    value["ppb_per_block"] = json!(price.ppb_per_block);
                    value["seller_listings"] = json!(listed.seller_listings);
                }
                value
            })
            .collect();
        let mut answer = json!({
            "instances_silent": books.silent.iter().map(PublicKey::to_hex).collect::<Vec<_>>(),
            "relays_down": books.relays_down,
        });
        if params.records {
            let sellers: BTreeSet<secp256k1::PublicKey> = listed
                .iter()
                .filter_map(|listed| match &listed.order {
                    OpenOrder::Lease(order) => Some(order.seller_node),
                    OpenOrder::Ring(_) => None,
                })
                .collect();
            match self.records(&sellers).await {
                Ok(records) => {
                    for (listed, value) in listed.iter().zip(orders.iter_mut()) {
                        if let OpenOrder::Lease(order) = &listed.order
                            && let Some(record) = records.get(&order.seller_node)
                        {
                            value["seller_record"] = json!(record);
                        }
                    }
                }
                Err(e) => answer["records_failed"] = json!(e.message),
            }
        }
        answer["orders"] = json!(orders);
        Ok(answer)
    }

    /// The records of `nodes`, from one request for the outcomes about any of them, read by
    /// [`record::read`] (the usable instances are the trusted ones) and counted per node.
    pub async fn records(
        &self,
        nodes: &BTreeSet<secp256k1::PublicKey>,
    ) -> Result<BTreeMap<secp256k1::PublicKey, Record>, ApiError> {
        if nodes.is_empty() {
            return Ok(BTreeMap::new());
        }
        let hex: Vec<String> = nodes.iter().map(ToString::to_string).collect();
        let events = relay::fetch_outcomes(&self.client, &hex)
            .await
            .map_err(|e| ApiError::new(Code::Relay, e))?;
        let stated = record::read(&events, &self.instances, unix_now());
        Ok(nodes
            .iter()
            .map(|node| (*node, record::summarize(node, &stated)))
            .collect())
    }

    /// `node`'s record: the outcomes about it on the relays.
    pub async fn record(&self, node: &str) -> Result<Value, ApiError> {
        let key: secp256k1::PublicKey = node
            .parse()
            .map_err(|_| ApiError::bad_request("not a node key"))?;
        let hex = [node.to_string()];
        let events = relay::fetch_outcomes(&self.client, &hex)
            .await
            .map_err(|e| ApiError::new(Code::Relay, e))?;
        let stated = record::read(&events, &self.instances, unix_now());
        let summary = record::summarize(&key, &stated);
        Ok(json!({
            "node": node,
            "record": summary,
            "outcomes_read": stated.len(),
        }))
    }
}
