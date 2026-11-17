//! Runs every `LightningBackend` method against two real nodes on regtest (payer A, payee B), the
//! way a lease uses them. The steps know only the trait: the same run is the conformance test
//! of every backend. Driven by `run-lnd.sh`; exits non-zero on the first failed step.
//!
//! Environment: `A_BACKEND`, `B_BACKEND` (or `BACKEND` for both; `lnd` by default), how to reach
//! each node (`A_ADDR`/`A_CERT`/`A_MAC`, `B_…`; see `node`), `B_P2P` (host:port), `MINE` (a
//! command that mines its argument's blocks), `HASHES` (three "preimage:hash" pairs,
//! comma-separated), and `HOLD_CLTV` (the lock time asked of B's hold invoices, in blocks; 144 by
//! default; a backend that cannot give what is asked must refuse, so a node with a fixed lock
//! time is tested with its own).
//!
//! What a backend may lack, and the run then says so and goes on: `graph_channels`
//! (`BackendError::Unsupported`). Everything else is required.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bipnip_ln::ldk::LdkBackend;
use bipnip_ln::lnd::LndBackend;
use bipnip_ln::{
    BackendError, HoldInvoiceRequest, HoldState, LightningBackend, OpenRequest, PaymentState,
};
use mostro_core::node_binding::recover_ln_signer;

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
/// `_CERT` (its tls.crt), `_MAC` (its hex macaroon file).
async fn node(prefix: &str) -> Box<dyn LightningBackend> {
    let backend = std::env::var(format!("{prefix}_BACKEND"))
        .or_else(|_| std::env::var("BACKEND"))
        .unwrap_or_else(|_| "lnd".into());
    let (addr, cert, mac) = (
        env(&format!("{prefix}_ADDR")),
        env(&format!("{prefix}_CERT")),
        env(&format!("{prefix}_MAC")),
    );
    match backend.as_str() {
        "lnd" => Box::new(
            LndBackend::connect(&addr, Path::new(&cert), Path::new(&mac))
                .await
                .expect("connect"),
        ),
        "ldk" => Box::new(
            LdkBackend::connect(&addr, Path::new(&cert), Path::new(&mac)).expect("connect"),
        ),
        other => panic!("no backend {other:?} in this build"),
    }
}

/// Pays `invoice` from `a` until the HTLC is out (a brand-new channel can refuse the first tries).
/// A try made while the one before is still unresolved is answered with that payment's state,
/// by the rule of [`LightningBackend::pay_invoice`]; no backend's error text is read here.
async fn pay_until_in_flight(a: &dyn LightningBackend, invoice: &str) -> PaymentState {
    for attempt in 1..=6 {
        match a.pay_invoice(invoice, 10_000, 5).await {
            Ok(PaymentState::Failed { reason }) => {
                println!("  attempt {attempt} failed ({reason}); retrying");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Ok(state) => return state,
            Err(e) => panic!("pay_invoice: {e}"),
        }
    }
    panic!("payment never left");
}

#[tokio::main]
async fn main() {
    let (a, b) = (node("A").await, node("B").await);
    let (a, b) = (a.as_ref(), b.as_ref());
    let hashes: Vec<(String, String)> = env("HASHES")
        .split(',')
        .map(|p| {
            let (pre, hash) = p.split_once(':').expect("preimage:hash");
            (pre.to_string(), hash.to_string())
        })
        .collect();

    step("node_info");
    let (ia, ib) = (
        a.node_info().await.expect("info a"),
        b.node_info().await.expect("info b"),
    );
    assert_eq!(ia.network, "regtest");
    assert_eq!(ib.network, "regtest");
    assert_ne!(ia.node_pubkey, ib.node_pubkey);
    assert!(ia.block_height > 100);

    step("sign_message recovers to the node key in bipnip-core");
    let msg = b"bipnip-ln conformance";
    let sig = a.sign_message(msg).await.expect("sign");
    let recovered = recover_ln_signer(msg, &sig).expect("recover");
    assert_eq!(recovered.to_string(), ia.node_pubkey);

    step("argument checks reject before any RPC");
    assert!(matches!(
        a.hold_invoice_state("zz").await,
        Err(BackendError::Rejected(_))
    ));
    assert!(matches!(
        a.channels_with("02abc").await,
        Err(BackendError::Rejected(_))
    ));

    step("graph_node: unknown key is None");
    let unknown = format!("02{}", "11".repeat(32));
    assert_eq!(a.graph_node(&unknown).await.expect("graph"), None);

    step("open_channel A -> B (announced) and channels_with on both sides");
    assert!(
        a.channels_with(&ib.node_pubkey)
            .await
            .expect("channels")
            .is_empty()
    );
    let outpoint = a
        .open_channel(&OpenRequest {
            remote_pubkey: ib.node_pubkey.clone(),
            remote_addr: Some(env("B_P2P")),
            capacity_sat: 1_000_000,
            announce: true,
            feerate_sat_vb: 2,
        })
        .await
        .expect("open");
    assert_eq!(
        outpoint.split(':').next().map(str::len),
        Some(64),
        "{outpoint}"
    );
    let pending = a.channels_with(&ib.node_pubkey).await.expect("channels");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].funding_outpoint, outpoint);
    assert!(pending[0].local_initiator && !pending[0].active && pending[0].confirmations == 0);
    mine(6);
    until("channel active on A", 120, || async {
        a.channels_with(&ib.node_pubkey)
            .await
            .is_ok_and(|c| c.iter().any(|c| c.active))
    })
    .await;
    until("channel active on B", 120, || async {
        b.channels_with(&ia.node_pubkey)
            .await
            .is_ok_and(|c| c.iter().any(|c| c.active))
    })
    .await;
    let on_a = &a.channels_with(&ib.node_pubkey).await.expect("channels")[0];
    let on_b = &b.channels_with(&ia.node_pubkey).await.expect("channels")[0];
    assert_eq!(on_a.funding_outpoint, outpoint);
    assert_eq!(on_b.funding_outpoint, outpoint);
    assert_eq!(
        (on_a.capacity_sat, on_b.capacity_sat),
        (1_000_000, 1_000_000)
    );
    assert!(on_a.local_initiator && !on_b.local_initiator);
    assert!(on_a.announced && on_b.announced);
    assert!(
        on_a.confirmations >= 6 && on_b.confirmations >= 6,
        "{on_a:?} {on_b:?}"
    );

    step("hold invoice on the payer's hash: held without the preimage, then settled with it");
    let hold_cltv: u32 = std::env::var("HOLD_CLTV")
        .ok()
        .map(|v| v.parse().expect("HOLD_CLTV is a number of blocks"))
        .unwrap_or(144);
    let (pre, hash) = &hashes[0];
    let hold = HoldInvoiceRequest {
        payment_hash: hash.clone(),
        amount_msat: 1_500_000,
        expiry_secs: 3600,
        cltv_delta: hold_cltv,
        description: "bipnip-ln conformance: hold".into(),
    };
    let invoice = b.add_hold_invoice(&hold).await.expect("add hold");
    assert_eq!(
        b.hold_invoice_state(hash).await.expect("state"),
        HoldState::Open
    );
    assert_eq!(
        pay_until_in_flight(a, &invoice).await,
        PaymentState::InFlight
    );
    until("hold invoice accepted", 60, || async {
        b.hold_invoice_state(hash).await == Ok(HoldState::Accepted)
    })
    .await;
    assert_eq!(
        a.payment_state(hash).await.expect("state"),
        PaymentState::InFlight
    );
    // Paying an invoice whose payment is under way starts no second payment and is no error.
    assert_eq!(
        a.pay_invoice(&invoice, 10_000, 5).await,
        Ok(PaymentState::InFlight),
        "paying again while the payment is held"
    );
    b.settle_hold_invoice(pre).await.expect("settle");
    until("payer sees the preimage", 60, || async {
        a.payment_state(hash).await
            == Ok(PaymentState::Succeeded {
                preimage: pre.clone(),
            })
    })
    .await;
    assert_eq!(
        b.hold_invoice_state(hash).await.expect("state"),
        HoldState::Settled
    );
    // Nor is paying one that is already paid: the answer is the payment that was made.
    assert_eq!(
        a.pay_invoice(&invoice, 10_000, 5).await,
        Ok(PaymentState::Succeeded {
            preimage: pre.clone(),
        }),
        "paying again after the payment settled"
    );

    step("hold invoice cancelled: the payer is refunded");
    let (_, hash2) = &hashes[1];
    let invoice = b
        .add_hold_invoice(&HoldInvoiceRequest {
            payment_hash: hash2.clone(),
            ..hold.clone()
        })
        .await
        .expect("add hold");
    assert_eq!(
        pay_until_in_flight(a, &invoice).await,
        PaymentState::InFlight
    );
    until("hold invoice accepted", 60, || async {
        b.hold_invoice_state(hash2).await == Ok(HoldState::Accepted)
    })
    .await;
    b.cancel_hold_invoice(hash2).await.expect("cancel");
    until("payer sees the failure", 60, || async {
        matches!(
            a.payment_state(hash2).await,
            Ok(PaymentState::Failed { .. })
        )
    })
    .await;
    assert_eq!(
        b.hold_invoice_state(hash2).await.expect("state"),
        HoldState::Canceled
    );

    step("settle is idempotent; a preimage matching no invoice is refused");
    // Settling an already-settled invoice again is accepted by LND: a retry after a lost reply is safe.
    let again = b.settle_hold_invoice(pre).await;
    assert!(again.is_ok(), "repeat settle: {again:?}");
    let (_, hash3) = &hashes[2];
    b.add_hold_invoice(&HoldInvoiceRequest {
        payment_hash: hash3.clone(),
        ..hold.clone()
    })
    .await
    .expect("add hold");
    let stranger = "5a".repeat(32);
    let refused = b.settle_hold_invoice(&stranger).await;
    assert!(refused.is_err(), "unknown preimage: {refused:?}");
    assert_eq!(
        b.hold_invoice_state(hash3).await.expect("state"),
        HoldState::Open
    );
    // An open (unpaid) hold invoice cannot be settled even with its own preimage.
    let unpaid = b.settle_hold_invoice(&hashes[2].0).await;
    assert!(unpaid.is_err(), "settle of an unpaid invoice: {unpaid:?}");

    step("plain invoice paid within the wait");
    let invoice = b
        .add_invoice(250_000, 600, "bipnip-ln conformance: plain")
        .await
        .expect("invoice");
    match a.pay_invoice(&invoice, 10_000, 30).await.expect("pay") {
        PaymentState::Succeeded { preimage } => assert_eq!(preimage.len(), 64),
        other => panic!("plain invoice not paid: {other:?}"),
    }

    step("graph_node sees the announced channel");
    until("B in A's graph with a channel", 120, || async {
        matches!(a.graph_node(&ib.node_pubkey).await, Ok(Some((n, cap))) if n >= 1 && cap >= 1_000_000)
    })
    .await;

    step("graph_channels: the announced channel, from either end, and nothing for a stranger");
    if let Err(BackendError::Unsupported(why)) = a.graph_channels(&ib.node_pubkey).await {
        println!("  not supported by A's backend ({why}); skipped");
    } else {
        let ia = a.node_info().await.expect("A info");
        let in_graph = a
            .graph_channels(&ib.node_pubkey)
            .await
            .expect("graph channels of B");
        let ours = in_graph
            .iter()
            .find(|c| c.funding_outpoint == outpoint)
            .expect("the channel we opened is in the graph");
        assert_eq!(ours.peer, ia.node_pubkey, "seen from B, the peer is A");
        assert_eq!(ours.capacity_sat, 1_000_000);
        let from_a = a
            .graph_channels(&ia.node_pubkey)
            .await
            .expect("graph channels of A");
        assert!(
            from_a
                .iter()
                .any(|c| c.funding_outpoint == outpoint && c.peer == ib.node_pubkey),
            "seen from A, the peer is B"
        );
        assert!(
            a.graph_channels(&unknown)
                .await
                .expect("graph channels of a stranger")
                .is_empty()
        );
        assert!(matches!(
            a.graph_channels("02abc").await,
            Err(BackendError::Rejected(_))
        ));
    }

    let named = |prefix: &str| {
        std::env::var(format!("{prefix}_BACKEND"))
            .or_else(|_| std::env::var("BACKEND"))
            .unwrap_or_else(|_| "lnd".into())
    };
    println!(
        "CONFORMANCE PASS: payer {}, payee {}, all steps",
        named("A"),
        named("B")
    );
}
