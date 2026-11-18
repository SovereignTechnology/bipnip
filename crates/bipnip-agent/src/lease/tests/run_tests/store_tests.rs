//! Lease runs stored and read back: a lease must survive a restart at any step.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use bipnip_ln::{LightningBackend, OpenRequest, PaymentState};

use super::{NOW, SELLER, Stage, to_instance};
use crate::lease::BuyerLease;
use crate::lease::run::{BuyerRun, BuyerState, SellerRun, SellerState};
use crate::lease::store::{LeaseStore, StoreError};
use crate::secretfile::SecretFileError;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bipnip-leases-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Saves the run, forgets it, and reads it back: a restart.
fn restart_seller(store: &LeaseStore, run: SellerRun) -> SellerRun {
    store.save_seller(&run).expect("save");
    let key = run.store_key();
    drop(run);
    let loaded = store.load().expect("load");
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    loaded
        .sellers
        .into_iter()
        .find(|run| run.store_key() == key)
        .expect("the seller's run")
}

fn restart_buyer(store: &LeaseStore, run: BuyerRun) -> BuyerRun {
    store.save_buyer(&run).expect("save");
    let key = run.store_key();
    drop(run);
    let loaded = store.load().expect("load");
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    loaded
        .buyers
        .into_iter()
        .find(|run| run.store_key() == key)
        .expect("the buyer's run")
}

#[tokio::test]
async fn a_lease_survives_a_restart_at_every_step() {
    let stage = Stage::new();
    let path = dir("every-step");
    let store = LeaseStore::open(&path).expect("store");
    assert_eq!(
        std::fs::metadata(&path).expect("meta").mode() & 0o777,
        0o700
    );

    let (seller, _) = SellerRun::offer(
        stage.instance_key(),
        stage.offer.clone(),
        1,
        7,
        super::SELLER,
    );
    let mut seller = restart_seller(&store, seller);
    assert_eq!(seller.state(), SellerState::Offering);
    let echo = mostro_core::prelude::Message::new_order(
        Some(stage.order_id),
        Some(1),
        None,
        mostro_core::prelude::Action::LeaseOffer,
        Some(mostro_core::prelude::Payload::Lease(
            mostro_core::lease_message::LeasePayload::Offer(stage.offer.clone()),
        )),
    );
    stage.to_seller(&mut seller, &echo).await.expect("echo");
    let mut seller = restart_seller(&store, seller);
    assert_eq!(seller.state(), SellerState::Open);

    // The buyer's run is on disk, preimage included, before its take goes anywhere.
    let (buyer, take) = stage.take(stage.take_request()).await;
    let mut buyer = restart_buyer(&store, buyer);

    let invoice = to_instance(stage.to_seller(&mut seller, &take).await.expect("take"));
    let mut seller = restart_seller(&store, seller);
    stage
        .to_seller(&mut seller, &stage.peer_key(&stage.buyer.trade))
        .await
        .expect("peer key");
    stage
        .to_buyer(&mut buyer, &stage.peer_key(&stage.seller.trade))
        .await
        .expect("peer key");
    let mut seller = restart_seller(&store, seller);
    let mut buyer = restart_buyer(&store, buyer);

    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    let mut buyer = restart_buyer(&store, buyer);
    assert_eq!(buyer.state(), BuyerState::Paid);

    assert!(
        seller
            .tick(&stage.seller_node, NOW)
            .await
            .expect("tick")
            .is_empty()
    );
    let mut seller = restart_seller(&store, seller);
    assert_eq!(seller.state(), SellerState::Opening);
    let report = to_instance(seller.tick(&stage.seller_node, NOW).await.expect("tick"));
    let mut seller = restart_seller(&store, seller);
    assert_eq!(seller.state(), SellerState::Opened);
    assert_eq!(stage.channels(), 1);

    stage.to_buyer(&mut buyer, &report).await.expect("report");
    let mut buyer = restart_buyer(&store, buyer);
    assert_eq!(buyer.state(), BuyerState::Verifying);
    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node, NOW).await.expect("tick");
    let mut buyer = restart_buyer(&store, buyer);
    assert_eq!(buyer.state(), BuyerState::Released);
    // Released once, also after a restart.
    assert!(
        buyer
            .tick(&stage.buyer_node, NOW)
            .await
            .expect("tick")
            .is_empty()
    );

    let active = to_instance(
        stage
            .peer_to_seller(&mut seller, &out[0])
            .await
            .expect("preimage"),
    );
    let seller = restart_seller(&store, seller);
    assert_eq!(seller.state(), SellerState::Active);
    stage.to_buyer(&mut buyer, &active).await.expect("active");
    let buyer = restart_buyer(&store, buyer);
    assert_eq!(buyer.state(), BuyerState::Active);

    // On disk: everything owner-only, and the preimage in the lease file alone.
    let PaymentState::Succeeded { preimage } = stage.payment(&buyer).await else {
        panic!("the payment did not settle");
    };
    let mut holding_the_preimage = Vec::new();
    for entry in std::fs::read_dir(&path).expect("dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        assert_eq!(
            entry.metadata().expect("meta").mode() & 0o777,
            0o600,
            "{name}"
        );
        if std::fs::read_to_string(entry.path())
            .expect("read")
            .contains(&preimage)
        {
            holding_the_preimage.push(name);
        }
    }
    assert_eq!(
        holding_the_preimage,
        vec![format!("buyer-{}.lease", buyer.store_key())]
    );
}

#[tokio::test]
async fn a_seller_that_stopped_mid_open_does_not_open_twice() {
    let stage = Stage::new();
    let store = LeaseStore::open(&dir("mid-open")).expect("store");
    // A channel to the buyer from before the lease, with the very capacity on offer: it must
    // not be mistaken for the leased one.
    let open = OpenRequest {
        remote_pubkey: stage.buyer_node.id(),
        remote_addr: None,
        capacity_sat: 2_000_000,
        announce: true,
        feerate_sat_vb: 2,
    };
    let old = stage.seller_node.open_channel(&open).await.expect("old");

    let (mut seller, mut buyer, invoice) = stage.taken().await;
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert!(
        seller
            .tick(&stage.seller_node, NOW)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Opening);
    store.save_seller(&seller).expect("save");

    // The next tick opened the channel, and the agent died before it could record that.
    let lost = to_instance(seller.tick(&stage.seller_node, NOW).await.expect("tick"));
    assert_eq!(stage.channels(), 2);
    drop(seller);

    let mut seller = store.load().expect("load").sellers.pop().expect("the run");
    assert_eq!(seller.state(), SellerState::Opening);
    let report = to_instance(seller.tick(&stage.seller_node, NOW).await.expect("tick"));
    assert_eq!(stage.channels(), 2, "no second channel");
    assert_eq!(
        report.as_json().expect("json"),
        lost.as_json().expect("json"),
        "the same channel is reported"
    );
    assert_ne!(seller.funding_outpoint(), Some(old.as_str()));
    assert_eq!(seller.state(), SellerState::Opened);

    // The lease then finishes as usual.
    stage.to_buyer(&mut buyer, &report).await.expect("report");
    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node, NOW).await.expect("tick");
    stage
        .peer_to_seller(&mut seller, &out[0])
        .await
        .expect("preimage");
    assert_eq!(seller.state(), SellerState::Active);
}

#[tokio::test]
async fn a_cancel_between_noticing_the_payment_and_opening_gives_it_back() {
    let stage = Stage::new();
    let (mut seller, mut buyer, invoice) = stage.taken().await;
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert!(
        seller
            .tick(&stage.seller_node, NOW)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Opening);

    stage
        .to_seller(&mut seller, &stage.canceled())
        .await
        .expect("canceled");
    assert_eq!(seller.state(), SellerState::Canceled);
    assert!(matches!(
        stage.payment(&buyer).await,
        PaymentState::Failed { .. }
    ));
    assert!(
        seller
            .tick(&stage.seller_node, NOW)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.channels(), 0);
}

#[tokio::test]
async fn stored_state_that_was_changed_or_opened_up_is_refused() {
    let stage = Stage::new();
    let path = dir("refused");
    let store = LeaseStore::open(&path).expect("store");
    let (seller, buyer) = stage.opened().await;
    store.save_seller(&seller).expect("save");
    store.save_buyer(&buyer).expect("save");
    let seller_file = path.join(format!("seller-{}.json", seller.store_key()));
    let buyer_file = path.join(format!("buyer-{}.json", buyer.store_key()));
    let lease_file = path.join(format!("buyer-{}.lease", buyer.store_key()));
    let problems = || {
        let loaded = store.load().expect("load");
        (
            loaded.sellers.len(),
            loaded.buyers.len(),
            loaded
                .problems
                .into_iter()
                .map(|(_, problem)| problem)
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(problems(), (1, 1, vec![]));

    // The lease is never rewritten, whatever a later save is given.
    let before = std::fs::read_to_string(&lease_file).expect("read");
    store.save_buyer(&buyer).expect("save again");
    assert_eq!(std::fs::read_to_string(&lease_file).expect("read"), before);
    assert_eq!(
        buyer.save_lease(&lease_file),
        Err(StoreError::File(SecretFileError::Exists))
    );

    // A state file others can read. The other run still loads.
    std::fs::set_permissions(&seller_file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert_eq!(
        problems(),
        (
            0,
            1,
            vec![StoreError::File(SecretFileError::TooOpen(0o644))]
        )
    );
    std::fs::set_permissions(&seller_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");

    // A seller's record whose state needs something the record lacks, or with a field added.
    let good = seller.to_json().expect("json");
    let outpoint = seller.funding_outpoint().expect("outpoint");
    let without_outpoint = good.replace(&format!("\"{outpoint}\""), "null");
    assert_ne!(without_outpoint, good);
    assert!(matches!(
        SellerRun::from_json(&without_outpoint),
        Err(StoreError::Corrupt(_))
    ));
    let with_extra = good.replacen('{', "{\"extra\":1,", 1);
    assert!(matches!(
        SellerRun::from_json(&with_extra),
        Err(StoreError::Corrupt(_))
    ));
    assert!(SellerRun::from_json(&good).is_ok());

    // A lease file whose preimage does not open the contract.
    let wrong = before.replace(
        before.lines().nth(2).expect("the preimage line"),
        &"5a".repeat(32),
    );
    std::fs::remove_file(&lease_file).expect("remove");
    std::fs::write(&lease_file, &wrong).expect("write");
    std::fs::set_permissions(&lease_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert!(matches!(
        BuyerLease::load(&lease_file),
        Err(StoreError::Corrupt(_))
    ));
    let (sellers, buyers, found) = problems();
    assert_eq!((sellers, buyers, found.len()), (1, 0, 1));

    // A buyer's record put next to another lease.
    let (_, other, _) = stage.taken().await;
    let other_lease = path.join("other.lease");
    other.save_lease(&other_lease).expect("save");
    let record = std::fs::read_to_string(&buyer_file).expect("read");
    let result = BuyerRun::from_json(&record, BuyerLease::load(&other_lease).expect("load"));
    assert!(matches!(result, Err(StoreError::Corrupt(_))));
}

#[tokio::test]
async fn finished_runs_are_put_away_once_they_are_old_and_nothing_is_deleted() {
    let stage = Stage::new();
    let path = dir("put-away");
    let store = LeaseStore::open(&path).expect("store");
    // A seller that withdrew its offer, a buyer that backed out, and an offer still waiting.
    let mut withdrawn = stage.offered().await;
    withdrawn
        .cancel(&stage.seller_node, 9)
        .await
        .expect("cancel");
    store.save_seller(&withdrawn).expect("save");
    let (mut backed_out, _) = stage.take(stage.take_request()).await;
    backed_out.cancel(9).expect("cancel");
    store.save_buyer(&backed_out).expect("save");
    let (waiting, _) = SellerRun::offer(stage.instance_key(), stage.offer.clone(), 5, 9, SELLER);
    store.save_seller(&waiting).expect("save");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    // Finished a moment ago: they stay where they are listed.
    assert!(
        store
            .put_away_finished(3_600, now)
            .expect("pass")
            .is_empty()
    );
    // An hour on, the two finished runs go; the offer that still waits does not.
    let mut moved = store.put_away_finished(3_600, now + 3_601).expect("pass");
    moved.sort();
    let seller = format!("seller-{}", withdrawn.store_key());
    let buyer = format!("buyer-{}", backed_out.store_key());
    assert_eq!(moved, vec![buyer.clone(), seller.clone()]);
    let loaded = store.load().expect("load");
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    assert_eq!((loaded.sellers.len(), loaded.buyers.len()), (1, 0));
    assert_eq!(loaded.sellers[0].state(), SellerState::Offering);
    // Nothing was deleted: the files are in `done`, owner-only, the buyer's lease with them.
    let done = path.join("done");
    assert_eq!(
        std::fs::metadata(&done).expect("meta").mode() & 0o777,
        0o700
    );
    for name in [
        format!("{seller}.json"),
        format!("{buyer}.json"),
        format!("{buyer}.lease"),
    ] {
        assert!(done.join(&name).exists(), "{name} is kept");
        assert!(!path.join(&name).exists(), "{name} is out of the way");
        assert_eq!(
            std::fs::metadata(done.join(&name)).expect("meta").mode() & 0o777,
            0o600
        );
    }
    // A second pass finds nothing more to do.
    assert!(
        store
            .put_away_finished(3_600, now + 3_601)
            .expect("pass")
            .is_empty()
    );
}
