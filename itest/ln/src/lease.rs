//! A whole liquidity lease between two real LND nodes on regtest, through `bipnip-agent`'s lease
//! engine: buyer = node A (has a channel to B to pay over), seller = node B (has on-chain funds).
//! Three leases: the honest one; one where the seller never opens and cancels; and one where the
//! seller opens a smaller channel than agreed. Same environment as `ln-conformance`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bipnip_agent::lease::{
    BuyerLease, DEFAULT_MAX_CLTV_DELTA, DEFAULT_SELLER_CLTV_DELTA, LeaseStepError, SellerLease,
};
use bipnip_ln::cln::ClnBackend;
use bipnip_ln::ldk::LdkBackend;
use bipnip_ln::lnd::LndBackend;
use bipnip_ln::{LightningBackend, OpenRequest, PaymentState};
use mostro_core::lease::LeaseTerms;
use mostro_core::node_binding::Network;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("missing env {k}"))
}

fn mine(n: u32) {
    let ok = Command::new("bash")
        .arg(env("MINE"))
        .arg(n.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "mining {n} blocks failed");
}

fn step(name: &str) {
    println!("STEP {name}");
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

async fn until<F, Fut>(what: &str, secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..secs {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The node behind `prefix` (`A` or `B`), through the backend `<prefix>_BACKEND` names, or else
/// `BACKEND`, or else `lnd`. For `lnd`: `<prefix>_ADDR` (https://host:port), `_CERT` (tls.cert),
/// `_MAC` (a macaroon file). For `ldk`: `_ADDR` (host:port of the LDK Server's gRPC service),
/// `_CERT` (its tls.crt), `_MAC` (its hex macaroon file). For `cln`: `_ADDR` (the path of the
/// node's `lightning-rpc` socket).
async fn node(prefix: &str) -> Box<dyn LightningBackend> {
    let backend = std::env::var(format!("{prefix}_BACKEND"))
        .or_else(|_| std::env::var("BACKEND"))
        .unwrap_or_else(|_| "lnd".into());
    let addr = env(&format!("{prefix}_ADDR"));
    let file = |what: &str| env(&format!("{prefix}_{what}"));
    match backend.as_str() {
        "lnd" => Box::new(
            LndBackend::connect(&addr, Path::new(&file("CERT")), Path::new(&file("MAC")))
                .await
                .expect("connect"),
        ),
        "ldk" => Box::new(
            LdkBackend::connect(&addr, Path::new(&file("CERT")), Path::new(&file("MAC")))
                .expect("connect"),
        ),
        "cln" => Box::new(ClnBackend::connect(Path::new(&addr))),
        other => panic!("no backend {other:?} in this build"),
    }
}

fn terms() -> LeaseTerms {
    LeaseTerms {
        capacity_sat: 500_000,
        duration_blocks: 4_320,
        fee_base_sat: 1_000,
        fee_ppm: 2_000,
        upfront_percent: 50,
        min_confirmations: 3,
        announce: true,
    }
}

/// Runs a lease up to the buyer's payment being locked at the seller.
async fn until_locked(
    buyer: &dyn LightningBackend,
    seller: &dyn LightningBackend,
) -> (BuyerLease, SellerLease) {
    let seller_id = seller.node_info().await.expect("info").node_pubkey;
    let b = BuyerLease::start(buyer, &seller_id, Network::Regtest, terms(), now())
        .await
        .expect("buyer start");
    let s = SellerLease::accept(seller, &b.contract().text())
        .await
        .expect("seller accept");
    let bolt11 = s
        .upfront_invoice(
            seller,
            // The seller's usual lock time, or as much of it as its node can give.
            seller
                .max_hold_cltv_delta()
                .map_or(DEFAULT_SELLER_CLTV_DELTA, |most| {
                    most.min(DEFAULT_SELLER_CLTV_DELTA)
                }),
        )
        .await
        .expect("upfront invoice");
    let state = b
        .pay_upfront(buyer, &bolt11, 10_000, DEFAULT_MAX_CLTV_DELTA)
        .await
        .expect("pay upfront");
    assert_eq!(
        state,
        PaymentState::InFlight,
        "the payment must be held, not settled"
    );
    until("payment locked at the seller", 60, || async {
        matches!(s.payment_locked(seller).await, Ok(true))
    })
    .await;
    (b, s)
}

#[tokio::main]
async fn main() {
    let (buyer, seller) = (node("A").await, node("B").await);
    let (buyer, seller) = (buyer.as_ref(), seller.as_ref());
    let buyer_id = buyer.node_info().await.expect("info").node_pubkey;
    let buyer_addr = env("A_P2P");

    step("lease 1: honest seller");
    let (b, s) = until_locked(buyer, seller).await;
    println!(
        "  contract {} upfront {} sat, completion {} sat",
        &b.contract().id()[..16],
        terms().upfront_sat().expect("upfront"),
        terms().completion_sat().expect("completion")
    );
    let outpoint = s
        .open(seller, Some(buyer_addr.clone()), 2)
        .await
        .expect("seller opens");
    let early = b.verify_channel(buyer, &outpoint).await;
    assert!(
        matches!(early, Err(LeaseStepError::Channel(_))),
        "an unconfirmed channel must not verify: {early:?}"
    );
    mine(2);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        b.verify_channel(buyer, &outpoint).await.is_err(),
        "2 of 3 confirmations must not verify"
    );
    mine(4);
    let mut proof = None;
    for _ in 0..90 {
        match b.verify_channel(buyer, &outpoint).await {
            Ok(p) => {
                proof = Some(p);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    let proof = proof.expect("the buyer's node verifies the channel");
    assert_eq!(
        b.payment_state(buyer).await.expect("state"),
        PaymentState::InFlight,
        "still unsettled before the release"
    );
    let preimage = b.release(&proof).expect("release");
    s.settle(seller, &preimage).await.expect("seller settles");
    until("buyer sees the payment settled", 60, || async {
        b.payment_state(buyer).await.ok()
            == Some(PaymentState::Succeeded {
                preimage: preimage.clone(),
            })
    })
    .await;
    println!("  channel {outpoint} verified by the buyer's node; upfront share settled");

    step("lease 2: the seller never opens and cancels; the buyer is refunded");
    let (b, s) = until_locked(buyer, seller).await;
    s.cancel(seller).await.expect("cancel");
    until("buyer sees the refund", 60, || async {
        matches!(
            b.payment_state(buyer).await,
            Ok(PaymentState::Failed { .. })
        )
    })
    .await;
    assert!(
        matches!(
            s.open(seller, Some(buyer_addr.clone()), 2).await,
            Err(LeaseStepError::NotLocked(_))
        ),
        "no open after a cancel"
    );

    step("lease 3: the seller opens a smaller channel; nothing is released");
    let (b, s) = until_locked(buyer, seller).await;
    let cheat = seller
        .open_channel(&OpenRequest {
            remote_pubkey: buyer_id.clone(),
            remote_addr: Some(buyer_addr.clone()),
            capacity_sat: 400_000,
            announce: true,
            feerate_sat_vb: 2,
        })
        .await
        .expect("the smaller channel opens");
    mine(6);
    until("the smaller channel is active at the buyer", 90, || async {
        buyer
            .channels_with(&s.contract().seller_node.to_string())
            .await
            .is_ok_and(|c| c.iter().any(|c| c.funding_outpoint == cheat && c.active))
    })
    .await;
    match b.verify_channel(buyer, &cheat).await {
        Err(LeaseStepError::Channel(why)) => println!("  refused: {why}"),
        other => panic!("a smaller channel must be refused: {other:?}"),
    }
    assert_eq!(
        b.payment_state(buyer).await.expect("state"),
        PaymentState::InFlight
    );
    s.cancel(seller).await.expect("cancel");
    until("buyer sees the refund", 60, || async {
        matches!(
            b.payment_state(buyer).await,
            Ok(PaymentState::Failed { .. })
        )
    })
    .await;

    println!("LEASE E2E PASS: honest lease settled; no-open and wrong-channel leases refunded");
}
