//! Lease tests on a two-node simulator: hold invoices are real, signed BOLT 11 invoices; payments,
//! invoice states and channels live in one shared table both nodes read and write.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bipnip_ln::NodeInfo;
use bitcoin::hashes::{Hash, sha256};
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning_invoice::{InvoiceBuilder, PaymentSecret};

use super::*;

mod run_tests;

pub(crate) const NOW: u64 = 1_794_800_000;

#[derive(Clone)]
pub(crate) struct Channel {
    pub(crate) opener: String,
    pub(crate) peer: String,
    pub(crate) outpoint: String,
    pub(crate) capacity_sat: u64,
    pub(crate) announced: bool,
    pub(crate) confirmations: u32,
}

#[derive(Default)]
pub(crate) struct World {
    pub(crate) invoices: HashMap<String, HoldState>,
    pub(crate) payments: HashMap<String, PaymentState>,
    pub(crate) channels: Vec<Channel>,
    /// Payment hashes of plain (not hold) invoices: paying one settles it at once.
    plain: HashSet<String>,
}

pub(crate) struct Node {
    pub(crate) world: Arc<Mutex<World>>,
    pub(crate) secret: SecretKey,
    pub(crate) network: &'static str,
}

pub(crate) fn secret(n: u8) -> SecretKey {
    let mut b = [0u8; 32];
    b[31] = n;
    SecretKey::from_slice(&b).expect("key")
}

fn pubkey(sk: &SecretKey) -> String {
    PublicKey::from_secret_key(&Secp256k1::signing_only(), sk).to_string()
}

/// A signed BOLT 11 invoice from `sk`.
fn invoice(
    sk: &SecretKey,
    currency: Currency,
    payment_hash: &str,
    amount_msat: u64,
    expiry_secs: u32,
    cltv_delta: u32,
) -> String {
    let hash =
        sha256::Hash::from_slice(&hexutil::decode(payment_hash).expect("hex")).expect("hash");
    InvoiceBuilder::new(currency)
        .description("test".into())
        .payment_hash(hash)
        .payment_secret(PaymentSecret([7u8; 32]))
        .duration_since_epoch(Duration::from_secs(NOW))
        .min_final_cltv_expiry_delta(u64::from(cltv_delta))
        .expiry_time(Duration::from_secs(u64::from(expiry_secs)))
        .amount_milli_satoshis(amount_msat)
        .build_signed(|m| Secp256k1::signing_only().sign_ecdsa_recoverable(m, sk))
        .expect("invoice")
        .to_string()
}

impl Node {
    pub(crate) fn id(&self) -> String {
        pubkey(&self.secret)
    }
}

fn unsupported<T>() -> Result<T, BackendError> {
    Err(BackendError::Unsupported("not simulated"))
}

#[async_trait]
impl LightningBackend for Node {
    async fn node_info(&self) -> Result<NodeInfo, BackendError> {
        Ok(NodeInfo {
            node_pubkey: self.id(),
            network: self.network.to_string(),
            block_height: 200,
            synced: true,
        })
    }
    async fn sign_message(&self, _: &[u8]) -> Result<String, BackendError> {
        unsupported()
    }
    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError> {
        let mut w = self.world.lock().expect("world");
        w.invoices.insert(req.payment_hash.clone(), HoldState::Open);
        Ok(invoice(
            &self.secret,
            Currency::Regtest,
            &req.payment_hash,
            req.amount_msat,
            req.expiry_secs,
            req.cltv_delta,
        ))
    }
    async fn hold_invoice_state(&self, hash: &str) -> Result<HoldState, BackendError> {
        let w = self.world.lock().expect("world");
        w.invoices
            .get(hash)
            .copied()
            .ok_or(BackendError::Rejected("no such invoice".into()))
    }
    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        let hash = sha256_hex(&hexutil::decode(preimage).expect("hex"));
        let mut w = self.world.lock().expect("world");
        match w.invoices.get(&hash) {
            Some(HoldState::Accepted) => {
                w.invoices.insert(hash.clone(), HoldState::Settled);
                w.payments.insert(
                    hash,
                    PaymentState::Succeeded {
                        preimage: preimage.to_string(),
                    },
                );
                Ok(())
            }
            _ => Err(BackendError::Rejected("invoice not accepted".into())),
        }
    }
    async fn cancel_hold_invoice(&self, hash: &str) -> Result<(), BackendError> {
        let mut w = self.world.lock().expect("world");
        w.invoices.insert(hash.to_string(), HoldState::Canceled);
        if w.payments.contains_key(hash) {
            w.payments.insert(
                hash.to_string(),
                PaymentState::Failed {
                    reason: "cancelled by the payee".into(),
                },
            );
        }
        Ok(())
    }
    async fn pay_invoice(
        &self,
        bolt11: &str,
        _: u64,
        _: u32,
    ) -> Result<PaymentState, BackendError> {
        let parsed = Bolt11Invoice::from_str(bolt11).expect("invoice");
        let hash = parsed.payment_hash().to_string();
        let mut w = self.world.lock().expect("world");
        if w.plain.contains(&hash) {
            let paid = PaymentState::Succeeded {
                preimage: "00".repeat(32),
            };
            w.invoices.insert(hash.clone(), HoldState::Settled);
            w.payments.insert(hash, paid.clone());
            return Ok(paid);
        }
        if w.invoices.get(&hash) == Some(&HoldState::Open) {
            w.invoices.insert(hash.clone(), HoldState::Accepted);
        }
        w.payments.insert(hash, PaymentState::InFlight);
        Ok(PaymentState::InFlight)
    }
    async fn payment_state(&self, hash: &str) -> Result<PaymentState, BackendError> {
        let w = self.world.lock().expect("world");
        w.payments
            .get(hash)
            .cloned()
            .ok_or(BackendError::Rejected("no such payment".into()))
    }
    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        _: &str,
    ) -> Result<String, BackendError> {
        let mut w = self.world.lock().expect("world");
        let hash = sha256_hex(format!("plain invoice {}", w.plain.len()).as_bytes());
        w.invoices.insert(hash.clone(), HoldState::Open);
        w.plain.insert(hash.clone());
        Ok(invoice(
            &self.secret,
            Currency::Regtest,
            &hash,
            amount_msat,
            expiry_secs,
            18,
        ))
    }
    async fn connect(&self, _: &str, _: &str) -> Result<(), BackendError> {
        Ok(())
    }
    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError> {
        let mut w = self.world.lock().expect("world");
        let outpoint = format!("{}:{}", "cd".repeat(32), w.channels.len());
        w.channels.push(Channel {
            opener: self.id(),
            peer: req.remote_pubkey.clone(),
            outpoint: outpoint.clone(),
            capacity_sat: req.capacity_sat,
            announced: req.announce,
            confirmations: 0,
        });
        Ok(outpoint)
    }
    async fn channels_with(&self, remote: &str) -> Result<Vec<ChannelView>, BackendError> {
        let me = self.id();
        let w = self.world.lock().expect("world");
        Ok(w.channels
            .iter()
            .filter(|c| {
                (c.opener == me && c.peer == remote) || (c.peer == me && c.opener == remote)
            })
            .map(|c| ChannelView {
                remote_pubkey: remote.to_string(),
                funding_outpoint: c.outpoint.clone(),
                capacity_sat: c.capacity_sat,
                local_initiator: c.opener == me,
                announced: c.announced,
                confirmations: c.confirmations,
                active: c.confirmations > 0,
            })
            .collect())
    }
    async fn graph_node(&self, _: &str) -> Result<Option<(u32, u64)>, BackendError> {
        unsupported()
    }
    /// The simulated graph: every announced channel with six confirmations, as gossip would
    /// carry it.
    async fn graph_channels(
        &self,
        node: &str,
    ) -> Result<Vec<bipnip_ln::GraphChannel>, BackendError> {
        let w = self.world.lock().expect("world");
        Ok(w.channels
            .iter()
            .filter(|c| c.announced && c.confirmations >= 6 && (c.opener == node || c.peer == node))
            .map(|c| bipnip_ln::GraphChannel {
                peer: if c.opener == node {
                    c.peer.clone()
                } else {
                    c.opener.clone()
                },
                capacity_sat: c.capacity_sat,
                funding_outpoint: c.outpoint.clone(),
            })
            .collect())
    }
}

fn pair() -> (Node, Node, Arc<Mutex<World>>) {
    let world = Arc::new(Mutex::new(World::default()));
    let node = |n: u8| Node {
        world: world.clone(),
        secret: secret(n),
        network: "regtest",
    };
    (node(1), node(2), world)
}

fn terms() -> LeaseTerms {
    LeaseTerms {
        capacity_sat: 2_000_000,
        duration_blocks: 4_320,
        fee_base_sat: 1_000,
        fee_ppm: 2_500,
        upfront_percent: 50,
        min_confirmations: 3,
        announce: true,
    }
}

pub(crate) fn confirm(world: &Arc<Mutex<World>>, outpoint: &str, confirmations: u32) {
    let mut w = world.lock().expect("world");
    for c in w.channels.iter_mut().filter(|c| c.outpoint == outpoint) {
        c.confirmations = confirmations;
    }
}

/// Buyer and seller up to the point where the buyer's payment is locked.
async fn locked(seller: &Node, buyer: &Node) -> (BuyerLease, SellerLease) {
    let b = BuyerLease::start(buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("start");
    let s = SellerLease::accept(seller, &b.contract().text())
        .await
        .expect("accept");
    let bolt11 = s
        .upfront_invoice(seller, DEFAULT_SELLER_CLTV_DELTA)
        .await
        .expect("invoice");
    let state = b
        .pay_upfront(buyer, &bolt11, 10_000, DEFAULT_MAX_CLTV_DELTA)
        .await
        .expect("pay");
    assert_eq!(state, PaymentState::InFlight);
    (b, s)
}

#[tokio::test]
async fn a_lease_runs_end_to_end_and_the_preimage_leaves_only_after_the_check() {
    let (seller, buyer, world) = pair();
    let (b, s) = locked(&seller, &buyer).await;
    assert!(s.payment_locked(&seller).await.expect("locked"));

    let outpoint = s.open(&seller, None, 2).await.expect("open");
    // Unconfirmed: the buyer's node sees it, but the check refuses, so nothing can be released.
    let early = b
        .verify_channel(&buyer, &outpoint)
        .await
        .expect_err("too early");
    assert!(matches!(early, LeaseStepError::Channel(_)), "{early}");
    confirm(&world, &outpoint, 2);
    assert!(b.verify_channel(&buyer, &outpoint).await.is_err());

    confirm(&world, &outpoint, 3);
    let proof = b.verify_channel(&buyer, &outpoint).await.expect("verified");
    assert_eq!(proof.funding_outpoint(), outpoint);
    let preimage = b.release(&proof).expect("release");
    s.settle(&seller, &preimage).await.expect("settle");

    assert_eq!(
        b.payment_state(&buyer).await.expect("state"),
        PaymentState::Succeeded { preimage }
    );
    assert!(
        !s.payment_locked(&seller)
            .await
            .expect("settled, not locked")
    );
}

#[tokio::test]
async fn the_seller_cannot_open_before_the_payment_is_locked() {
    let (seller, buyer, world) = pair();
    let b = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("start");
    let s = SellerLease::accept(&seller, &b.contract().text())
        .await
        .expect("accept");
    s.upfront_invoice(&seller, DEFAULT_SELLER_CLTV_DELTA)
        .await
        .expect("invoice");
    let err = s.open(&seller, None, 2).await.expect_err("not locked");
    assert!(
        matches!(err, LeaseStepError::NotLocked(HoldState::Open)),
        "{err}"
    );
    assert!(world.lock().expect("world").channels.is_empty());
}

#[tokio::test]
async fn the_hold_invoice_must_outlast_the_confirmation_wait() {
    let (seller, buyer, world) = pair();
    let slow = LeaseTerms {
        min_confirmations: 120,
        ..terms()
    };
    let b = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, slow, NOW)
        .await
        .expect("start");
    let s = SellerLease::accept(&seller, &b.contract().text())
        .await
        .expect("accept");
    let needed = 120 + SELLER_SETTLE_MARGIN_BLOCKS;
    // One day of blocks is not enough room for a 120-confirmation wait: no invoice is made.
    let err = s
        .upfront_invoice(&seller, DEFAULT_SELLER_CLTV_DELTA)
        .await
        .expect_err("too short");
    assert!(
        matches!(err, LeaseStepError::HoldWindow { cltv_delta, needed: n }
            if cltv_delta == DEFAULT_SELLER_CLTV_DELTA && n == needed),
        "{err}"
    );
    assert!(s.upfront_invoice(&seller, needed - 1).await.is_err());
    assert!(world.lock().expect("world").invoices.is_empty());
    // The smallest delta that fits is accepted, and the buyer's default limit still covers it.
    let bolt11 = s.upfront_invoice(&seller, needed).await.expect("invoice");
    assert!(needed <= DEFAULT_MAX_CLTV_DELTA);
    assert!(check_upfront_invoice(&bolt11, b.contract(), DEFAULT_MAX_CLTV_DELTA).is_ok());
}

#[tokio::test]
async fn invoices_that_differ_from_the_contract_are_never_paid() {
    let (seller, buyer, world) = pair();
    let b = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("start");
    let hash = b.contract().payment_hash.clone();
    let upfront_msat = 3_000_000;
    let stranger = secret(9);
    let other_hash = "ef".repeat(32);
    let cases = [
        (
            invoice(
                &secret(1),
                Currency::Regtest,
                &hash,
                upfront_msat + 1,
                3600,
                144,
            ),
            "amount",
        ),
        (
            invoice(&stranger, Currency::Regtest, &hash, upfront_msat, 3600, 144),
            "payee",
        ),
        (
            invoice(
                &secret(1),
                Currency::Regtest,
                &other_hash,
                upfront_msat,
                3600,
                144,
            ),
            "hash",
        ),
        (
            invoice(
                &secret(1),
                Currency::Bitcoin,
                &hash,
                upfront_msat,
                3600,
                144,
            ),
            "network",
        ),
        (
            invoice(
                &secret(1),
                Currency::Regtest,
                &hash,
                upfront_msat,
                MAX_HOLD_SECS + 1,
                144,
            ),
            "expiry",
        ),
        (
            invoice(
                &secret(1),
                Currency::Regtest,
                &hash,
                upfront_msat,
                3600,
                289,
            ),
            "cltv",
        ),
        ("lnbcrt1notaninvoice".to_string(), "garbage"),
    ];
    for (bolt11, what) in cases {
        let err = b
            .pay_upfront(&buyer, &bolt11, 10_000, DEFAULT_MAX_CLTV_DELTA)
            .await
            .expect_err(what);
        assert!(matches!(err, LeaseStepError::Invoice(_)), "{what}: {err}");
    }
    assert!(
        world.lock().expect("world").payments.is_empty(),
        "nothing was paid"
    );
    // The matching invoice is accepted.
    let good = invoice(
        &secret(1),
        Currency::Regtest,
        &hash,
        upfront_msat,
        MAX_HOLD_SECS,
        288,
    );
    assert!(check_upfront_invoice(&good, b.contract(), DEFAULT_MAX_CLTV_DELTA).is_ok());
}

#[tokio::test]
async fn channels_that_differ_from_the_contract_release_nothing() {
    let (seller, buyer, world) = pair();
    let (b, _s) = locked(&seller, &buyer).await;
    let stranger = pubkey(&secret(9));
    let push = |opener: &str, peer: &str, capacity_sat: u64, announced: bool| {
        let mut w = world.lock().expect("world");
        let outpoint = format!("{}:{}", "cd".repeat(32), w.channels.len());
        w.channels.push(Channel {
            opener: opener.to_string(),
            peer: peer.to_string(),
            outpoint: outpoint.clone(),
            capacity_sat,
            announced,
            confirmations: 6,
        });
        outpoint
    };
    let small = push(&seller.id(), &buyer.id(), 1_999_999, true);
    let private = push(&seller.id(), &buyer.id(), 2_000_000, false);
    let ours = push(&buyer.id(), &seller.id(), 2_000_000, true);
    let elsewhere = push(&stranger, &buyer.id(), 2_000_000, true);
    for (outpoint, what) in [
        (small.as_str(), "capacity"),
        (private.as_str(), "unannounced"),
        (ours.as_str(), "funded by the buyer"),
        (elsewhere.as_str(), "a channel with another node"),
        ("ab:0", "an outpoint our node does not have"),
    ] {
        let err = b.verify_channel(&buyer, outpoint).await.expect_err(what);
        assert!(matches!(err, LeaseStepError::Channel(_)), "{what}: {err}");
    }
    // A larger channel than agreed is fine.
    let larger = push(&seller.id(), &buyer.id(), 2_500_000, true);
    assert!(b.verify_channel(&buyer, &larger).await.is_ok());
}

#[tokio::test]
async fn a_proof_from_another_lease_releases_nothing() {
    let (seller, buyer, world) = pair();
    let (first, s) = locked(&seller, &buyer).await;
    let outpoint = s.open(&seller, None, 2).await.expect("open");
    confirm(&world, &outpoint, 3);
    let proof = first
        .verify_channel(&buyer, &outpoint)
        .await
        .expect("verified");

    let second = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW + 1)
        .await
        .expect("start");
    let err = second.release(&proof).expect_err("another lease");
    assert!(matches!(err, LeaseStepError::NotOurLease(_)), "{err}");
    assert!(first.release(&proof).is_ok());
}

#[tokio::test]
async fn a_wrong_preimage_is_refused_and_a_cancel_refunds_the_buyer() {
    let (seller, buyer, _world) = pair();
    let (b, s) = locked(&seller, &buyer).await;
    let err = s
        .settle(&seller, &"11".repeat(32))
        .await
        .expect_err("wrong");
    assert!(matches!(err, LeaseStepError::WrongPreimage), "{err}");
    assert!(matches!(
        s.settle(&seller, "zz").await,
        Err(LeaseStepError::WrongPreimage)
    ));
    assert!(s.payment_locked(&seller).await.expect("still locked"));

    s.cancel(&seller).await.expect("cancel");
    assert!(matches!(
        b.payment_state(&buyer).await.expect("state"),
        PaymentState::Failed { .. }
    ));
    let err = s.open(&seller, None, 2).await.expect_err("cancelled");
    assert!(
        matches!(err, LeaseStepError::NotLocked(HoldState::Canceled)),
        "{err}"
    );
}

#[tokio::test]
async fn contracts_for_other_nodes_or_networks_are_refused() {
    let (seller, buyer, _world) = pair();
    let b = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("start");
    // The buyer's own node is not the seller this contract names.
    let err = SellerLease::accept(&buyer, &b.contract().text())
        .await
        .expect_err("not the seller");
    assert!(matches!(err, LeaseStepError::NotOurLease(_)), "{err}");
    assert!(
        SellerLease::accept(&seller, "bipnip-lease:v1:nonsense")
            .await
            .is_err()
    );

    let err = BuyerLease::start(&buyer, &seller.id(), Network::Bitcoin, terms(), NOW)
        .await
        .expect_err("network");
    assert!(matches!(err, LeaseStepError::NotOurLease(_)), "{err}");
    let bad_terms = LeaseTerms {
        upfront_percent: 0,
        ..terms()
    };
    assert!(matches!(
        BuyerLease::start(&buyer, &seller.id(), Network::Regtest, bad_terms, NOW).await,
        Err(LeaseStepError::Contract(_))
    ));
    // A lease with oneself is refused.
    assert!(
        BuyerLease::start(&buyer, &buyer.id(), Network::Regtest, terms(), NOW)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_preimage_is_fresh_per_lease_and_never_printed() {
    let (seller, buyer, _world) = pair();
    let a = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("a");
    let b = BuyerLease::start(&buyer, &seller.id(), Network::Regtest, terms(), NOW)
        .await
        .expect("b");
    assert_ne!(a.contract().payment_hash, b.contract().payment_hash);
    let shown = format!("{a:?}");
    assert!(shown.contains("redacted"));
    assert!(!shown.contains(&hexutil::encode(&a.preimage)));
    assert!(!a.contract().text().contains(&hexutil::encode(&a.preimage)));
}
