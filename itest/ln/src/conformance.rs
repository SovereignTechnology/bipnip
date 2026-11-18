//! Runs every `LightningBackend` method against two real nodes on regtest (payer A, payee B), the
//! way a lease uses them. The steps know only the trait: the same run is the conformance test
//! of every backend. Driven by `run-lnd.sh` and `run-ldk.sh`; exits non-zero on the first failed step.
//!
//! Environment: `A_BACKEND`, `B_BACKEND` (or `BACKEND` for both; `lnd` by default), how to reach
//! each node (`A_ADDR`/`A_CERT`/`A_MAC`, `B_…`; see `node`), `B_P2P` (host:port), `MINE` (a
//! command that mines its argument's blocks) and `HASHES` (three "preimage:hash" pairs,
//! comma-separated).
//!
//! What a backend may lack, and the run then says so and goes on: hold invoices as the payee,
//! `graph_channels` and `closed_channel` (each by answering `BackendError::Unsupported`).
//! Everything else is required, and a backend that does offer hold invoices must pass every hold
//! step.
//!
//! With `CLOSE` set (a command that closes, from A's side, the channel whose funding outpoint is
//! its first argument; the second is B's node key), the run also opens a second, small channel,
//! closes it, and checks that both ends know it closed and by whom.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use bipnip_agent::ring::bond::payment_hash_of;
use bipnip_ln::cln::ClnBackend;
use bipnip_ln::ldk::LdkBackend;
use bipnip_ln::lnd::LndBackend;
use bipnip_ln::{
    BackendError, Closer, HoldInvoiceRequest, HoldState, InvoiceStatus, LightningBackend,
    OpenRequest, PaymentState,
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

/// The hold invoice steps, from the first invoice on: held and settled, held and cancelled, and
/// what a settle is refused for.
async fn hold_steps(
    a: &dyn LightningBackend,
    b: &dyn LightningBackend,
    hashes: &[(String, String)],
    hold: &HoldInvoiceRequest,
    invoice: String,
) {
    let (pre, hash) = &hashes[0];
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
}

/// Opens a second, small channel from A to B, closes it from A's side with the `close`
/// command, and checks both ends: it leaves `channels_with`, and `closed_channel` knows it, with
/// A as the closer (or "unknown"; never the other side).
async fn close_steps(
    a: &dyn LightningBackend,
    b: &dyn LightningBackend,
    a_key: &str,
    b_key: &str,
    close: &str,
) {
    step("closed_channel: a second channel, closed by A, is closed at both ends");
    let second = a
        .open_channel(&OpenRequest {
            remote_pubkey: b_key.to_string(),
            remote_addr: Some(env("B_P2P")),
            capacity_sat: 200_000,
            announce: true,
            feerate_sat_vb: 2,
        })
        .await
        .expect("open the second channel");
    mine(6);
    let active = |node: &'static str| {
        let (n, peer) = if node == "A" { (a, b_key) } else { (b, a_key) };
        let second = second.clone();
        async move {
            n.channels_with(peer)
                .await
                .is_ok_and(|c| c.iter().any(|c| c.funding_outpoint == second && c.active))
        }
    };
    until("second channel active on A", 120, || active("A")).await;
    until("second channel active on B", 120, || active("B")).await;
    let closed = Command::new("bash")
        .arg(close)
        .arg(&second)
        .arg(b_key)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(closed, "the close command failed");
    mine(6);
    for (side, node, peer, wrong) in [
        ("A", a, b_key, Closer::Remote),
        ("B", b, a_key, Closer::Local),
    ] {
        if let Err(BackendError::Unsupported(why)) = node.closed_channel(peer, &second).await {
            println!("  not supported by {side}'s backend ({why}); skipped");
            continue;
        }
        until(&format!("{side} knows the channel closed"), 120, || async {
            matches!(node.closed_channel(peer, &second).await, Ok(Some(_)))
        })
        .await;
        let seen = node
            .closed_channel(peer, &second)
            .await
            .expect("closed_channel")
            .expect("closed");
        println!("  {side} sees: {seen:?}");
        assert_ne!(seen.closer, wrong, "{side} names the wrong closer");
        assert!(
            !node
                .channels_with(peer)
                .await
                .expect("channels")
                .iter()
                .any(|c| c.funding_outpoint == second),
            "{side} still lists the closed channel"
        );
    }
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
    // A node that follows the chain by polling counts a block a moment later than it is mined.
    until("six confirmations on both sides", 120, || async {
        let six = |c: Result<Vec<bipnip_ln::ChannelView>, BackendError>| {
            c.is_ok_and(|c| c.iter().any(|c| c.confirmations >= 6))
        };
        six(a.channels_with(&ib.node_pubkey).await) && six(b.channels_with(&ia.node_pubkey).await)
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
    let hold = HoldInvoiceRequest {
        payment_hash: hashes[0].1.clone(),
        amount_msat: 1_500_000,
        expiry_secs: 3600,
        // A day of blocks, or as many as B's node can give.
        cltv_delta: b.max_hold_cltv_delta().map_or(144, |most| most.min(144)),
        description: "bipnip-ln conformance: hold".into(),
    };
    // A backend may be unable to hold a payment at all, and then says so before anything is
    // made. One that can must pass every hold step.
    match b.add_hold_invoice(&hold).await {
        Err(BackendError::Unsupported(why)) => {
            println!("  not supported by B's backend ({why}); the hold invoice steps are skipped");
        }
        first => hold_steps(a, b, &hashes, &hold, first.expect("add hold")).await,
    }

    step("plain invoice paid within the wait");
    let invoice = b
        .add_invoice(250_000, 600, "bipnip-ln conformance: plain")
        .await
        .expect("invoice");
    assert_eq!(
        b.invoice_state(&invoice).await.expect("invoice state"),
        InvoiceStatus::Unpaid
    );
    // As for a hold invoice, a brand-new channel can refuse the first tries.
    let mut paid = None;
    for attempt in 1..=6 {
        match a.pay_invoice(&invoice, 10_000, 30).await.expect("pay") {
            PaymentState::Succeeded { preimage } => {
                paid = Some(preimage);
                break;
            }
            PaymentState::Failed { reason } => {
                println!("  attempt {attempt} failed ({reason}); retrying");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            PaymentState::InFlight => panic!("a plain invoice was not paid within the wait"),
        }
    }
    assert_eq!(paid.expect("plain invoice paid").len(), 64);

    step("invoice_state: the paid invoice is paid; one left past its expiry is expired");
    until("the payee sees its invoice paid", 30, || async {
        b.invoice_state(&invoice).await == Ok(InvoiceStatus::Paid)
    })
    .await;
    let brief = b
        .add_invoice(1_000, 1, "bipnip-ln conformance: expires")
        .await
        .expect("invoice");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        b.invoice_state(&brief).await.expect("invoice state"),
        InvoiceStatus::Expired
    );

    step("payment_lookup: the payer has the payment, the payee and a stranger's hash have none");
    let hash = payment_hash_of(&invoice).expect("payment hash");
    assert!(
        matches!(
            a.payment_lookup(&hash).await.expect("lookup"),
            Some(PaymentState::Succeeded { .. })
        ),
        "the payer's node has the payment"
    );
    // The payee was paid; it sent nothing.
    assert_eq!(b.payment_lookup(&hash).await.expect("lookup"), None);
    assert_eq!(
        a.payment_lookup(&"42".repeat(32)).await.expect("lookup"),
        None
    );
    assert!(matches!(
        a.payment_lookup("zz").await,
        Err(BackendError::Rejected(_))
    ));

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

    step("closed_channel: an open channel is not closed");
    for (side, node, peer) in [("A", a, &ib.node_pubkey), ("B", b, &ia.node_pubkey)] {
        match node.closed_channel(peer, &outpoint).await {
            Err(BackendError::Unsupported(why)) => {
                println!("  not supported by {side}'s backend ({why}); skipped");
            }
            answer => assert_eq!(answer.expect("closed_channel"), None, "{side}"),
        }
    }
    if let Ok(close) = std::env::var("CLOSE") {
        close_steps(a, b, &ia.node_pubkey, &ib.node_pubkey, &close).await;
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
