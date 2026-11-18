//! Whole leases over a relay: both agents poll, save and send through [`Link`], and a small
//! stand-in instance relays between them the way `bipnipd` does.

use std::collections::HashSet;
use std::path::PathBuf;

use bipnip_ln::{HoldState, LightningBackend, PaymentState};
use mostro_core::prelude::{Action, Message, Payload, Peer, unwrap_message_nip44};
use nostr_sdk::local_relay::MockRelay;
use nostr_sdk::prelude::{Client, EventId, Filter, Keys, Kind, PublicKey};

use super::{BUYER, SELLER, Stage};
use crate::lease::link::{Link, launch_buyer, launch_seller, step_buyer, step_seller};
use crate::lease::run::{BuyerRun, BuyerState, Outgoing, SellerRun, SellerState};
use crate::lease::store::LeaseStore;
use crate::lease_wire;
use crate::relay::connected;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bipnip-link-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// The part of an instance a lease needs: it gives the offer an id and relays.
struct Relayer {
    keys: Keys,
    client: Client,
    done: HashSet<EventId>,
    order_id: uuid::Uuid,
    seller: Option<PublicKey>,
    buyer: Option<PublicKey>,
    /// Every action that reached the instance.
    saw: Vec<Action>,
}

impl Relayer {
    async fn send(&self, to: PublicKey, message: Message) {
        let event = lease_wire::seal_for_peer(&message, &self.keys, to).expect("seal");
        self.client.send_event(&event).await.expect("send");
    }

    fn peer_key(&self, of: PublicKey) -> Message {
        Message::new_order(
            Some(self.order_id),
            None,
            None,
            Action::TradePubkey,
            Some(Payload::Peer(Peer::new(of.to_hex(), None))),
        )
    }

    async fn step(&mut self) {
        let filter = Filter::new()
            .kind(Kind::PrivateDirectMessage)
            .pubkey(self.keys.public_key());
        let events = self
            .client
            .fetch_events(filter)
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("fetch");
        let mut fresh: Vec<_> = events
            .iter()
            .filter(|event| !self.done.contains(&event.id))
            .cloned()
            .collect();
        fresh.sort_by_key(|event| (event.created_at, event.id));
        for event in fresh {
            self.done.insert(event.id);
            let opened = unwrap_message_nip44(&event, &self.keys)
                .expect("unwrap")
                .expect("for the instance");
            let kind = opened.message.get_inner_message_kind().clone();
            self.saw.push(kind.action.clone());
            let relayed = Message::new_order(
                Some(self.order_id),
                None,
                None,
                kind.action.clone(),
                kind.payload.clone(),
            );
            match kind.action {
                Action::LeaseOffer => {
                    self.seller = Some(opened.sender);
                    let echo = Message::new_order(
                        Some(self.order_id),
                        kind.request_id,
                        None,
                        Action::LeaseOffer,
                        kind.payload.clone(),
                    );
                    self.send(opened.sender, echo).await;
                }
                Action::TakeLease => {
                    let seller = self.seller.expect("an offer first");
                    self.buyer = Some(opened.sender);
                    self.send(seller, relayed).await;
                    self.send(seller, self.peer_key(opened.sender)).await;
                    self.send(opened.sender, self.peer_key(seller)).await;
                }
                Action::LeaseInvoice | Action::LeaseChannelOpened | Action::LeaseActive => {
                    assert_eq!(Some(opened.sender), self.seller, "only the seller");
                    self.send(self.buyer.expect("a take first"), relayed).await;
                }
                other => panic!("the instance was sent {other:?}"),
            }
        }
    }
}

/// A stage on a relay: the instance and both agents' links and stores.
struct Wired {
    stage: Stage,
    relays: Vec<String>,
    _relay: MockRelay,
    instance: Relayer,
    seller_store: LeaseStore,
    buyer_store: LeaseStore,
    started: u64,
}

impl Wired {
    async fn new(name: &str) -> Self {
        let relay = MockRelay::run().await.expect("relay");
        let relays = vec![relay.url().await.to_string()];
        let stage = Stage::new();
        let instance = Relayer {
            keys: stage.instance.clone(),
            client: connected(&relays).await.expect("connect"),
            done: HashSet::new(),
            order_id: stage.order_id,
            seller: None,
            buyer: None,
            saw: Vec::new(),
        };
        Wired {
            seller_store: LeaseStore::open(&dir(&format!("{name}-seller"))).expect("store"),
            buyer_store: LeaseStore::open(&dir(&format!("{name}-buyer"))).expect("store"),
            stage,
            relays,
            _relay: relay,
            instance,
            started: now() - 60,
        }
    }

    async fn seller_link(&self) -> Link {
        Link::connect(
            &self.relays,
            self.stage.seller.identity.clone(),
            self.stage.seller.trade.clone(),
            self.stage.instance_key(),
            0,
            self.started,
        )
        .await
        .expect("link")
    }

    async fn buyer_link(&self) -> Link {
        Link::connect(
            &self.relays,
            self.stage.buyer.identity.clone(),
            self.stage.buyer.trade.clone(),
            self.stage.instance_key(),
            0,
            self.started,
        )
        .await
        .expect("link")
    }

    /// Confirms whatever channel the seller has opened.
    fn confirm_channels(&self) {
        let mut world = self.stage.world.lock().expect("world");
        for channel in world.channels.iter_mut() {
            channel.confirmations = 3;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_runs_over_a_relay_and_survives_both_agents_restarting() {
    let mut wired = Wired::new("restart").await;
    let stage_now = super::super::NOW;

    let (mut seller, offer) = SellerRun::offer(
        wired.stage.instance_key(),
        wired.stage.offer.clone(),
        1,
        7,
        SELLER,
    );
    let mut seller_link = wired.seller_link().await;
    let report = launch_seller(&mut seller, &offer, &seller_link, &wired.seller_store)
        .await
        .expect("launch");
    assert_eq!((report.sent, report.problems.len()), (1, 0));

    // The seller waits for its order id.
    for _ in 0..20 {
        wired.instance.step().await;
        step_seller(
            &mut seller,
            &mut seller_link,
            &wired.stage.seller_node,
            &wired.seller_store,
            stage_now,
        )
        .await
        .expect("step");
        if seller.state() == SellerState::Open {
            break;
        }
    }
    assert_eq!(seller.state(), SellerState::Open);

    let (mut buyer, take) = BuyerRun::take(
        &wired.stage.buyer_node,
        wired.stage.instance_key(),
        wired.stage.take_request(),
        BUYER,
        stage_now,
    )
    .await
    .expect("take");
    let mut buyer_link = wired.buyer_link().await;
    launch_buyer(&mut buyer, &take, &buyer_link, &wired.buyer_store)
        .await
        .expect("launch");
    // On disk before anything else happens, preimage included.
    assert_eq!(wired.buyer_store.load().expect("load").buyers.len(), 1);

    let mut restarted = (false, false);
    for round in 0..60 {
        wired.instance.step().await;
        let report = step_seller(
            &mut seller,
            &mut seller_link,
            &wired.stage.seller_node,
            &wired.seller_store,
            stage_now,
        )
        .await
        .expect("step");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        wired.instance.step().await;
        let report = step_buyer(
            &mut buyer,
            &mut buyer_link,
            &wired.stage.buyer_node,
            &wired.buyer_store,
            stage_now,
        )
        .await
        .expect("step");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        wired.confirm_channels();

        // Each agent dies once in the middle: its run comes back from disk, on a new link
        // that sees every message again and acts on none of them twice.
        if !restarted.0 && seller.state() == SellerState::Opening {
            seller_link.close().await;
            seller = wired
                .seller_store
                .load()
                .expect("load")
                .sellers
                .pop()
                .expect("the seller's run");
            seller_link = wired.seller_link().await;
            restarted.0 = true;
        }
        if !restarted.1 && buyer.state() == BuyerState::Verifying {
            buyer_link.close().await;
            buyer = wired
                .buyer_store
                .load()
                .expect("load")
                .buyers
                .pop()
                .expect("the buyer's run");
            buyer_link = wired.buyer_link().await;
            restarted.1 = true;
        }
        if seller.state() == SellerState::Active && buyer.state() == BuyerState::Active {
            break;
        }
        assert!(round < 59, "the lease did not finish: {seller:?} {buyer:?}");
    }
    assert_eq!(restarted, (true, true));
    assert_eq!(wired.stage.channels(), 1);
    assert!(matches!(
        wired.stage.payment(&buyer).await,
        PaymentState::Succeeded { .. }
    ));
    assert_eq!(wired.stage.hold_state(&buyer).await, HoldState::Settled);

    // The instance relayed everything but the preimage, which it never saw.
    assert_eq!(
        wired.instance.saw,
        vec![
            Action::LeaseOffer,
            Action::TakeLease,
            Action::LeaseInvoice,
            Action::LeaseChannelOpened,
            Action::LeaseActive,
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_is_owed_is_sent_at_the_next_step() {
    let mut wired = Wired::new("owed").await;
    let (mut seller, offer) = SellerRun::offer(
        wired.stage.instance_key(),
        wired.stage.offer.clone(),
        1,
        7,
        SELLER,
    );
    let mut seller_link = wired.seller_link().await;
    launch_seller(&mut seller, &offer, &seller_link, &wired.seller_store)
        .await
        .expect("launch");
    assert!(!seller.unsent());

    // As if no relay had taken the offer: the run is stored as owing it.
    seller.set_unsent(true);
    wired.seller_store.save_seller(&seller).expect("save");
    let mut seller = wired
        .seller_store
        .load()
        .expect("load")
        .sellers
        .pop()
        .expect("the seller's run");
    assert!(seller.unsent(), "what is owed survives a restart");

    let report = step_seller(
        &mut seller,
        &mut seller_link,
        &wired.stage.seller_node,
        &wired.seller_store,
        super::super::NOW,
    )
    .await
    .expect("step");
    assert_eq!(report.sent, 1);
    assert!(!seller.unsent());
    let stored = wired.seller_store.load().expect("load").sellers;
    assert!(!stored[0].unsent(), "and the store knows it was sent");

    // The instance got the offer twice and answered both; the run takes one answer.
    wired.instance.step().await;
    assert_eq!(
        wired.instance.saw,
        vec![Action::LeaseOffer, Action::LeaseOffer]
    );
    for _ in 0..3 {
        step_seller(
            &mut seller,
            &mut seller_link,
            &wired.stage.seller_node,
            &wired.seller_store,
            super::super::NOW,
        )
        .await
        .expect("step");
    }
    assert_eq!(seller.state(), SellerState::Open);
    assert_eq!(seller.order_id(), Some(wired.stage.order_id));
}

#[tokio::test(flavor = "multi_thread")]
async fn steps_that_reached_the_node_before_a_crash_are_not_done_twice() {
    let stage = Stage::new();

    // The buyer paid, and died before its run was saved: the invoice arrives again.
    let (mut seller, mut buyer, invoice) = stage.taken().await;
    let before = buyer.to_json().expect("json");
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    let lease_file = dir("twice").join("lease");
    std::fs::create_dir_all(lease_file.parent().expect("dir")).expect("dir");
    buyer.save_lease(&lease_file).expect("save");
    let lease = crate::lease::BuyerLease::load(&lease_file).expect("load");
    let mut buyer = BuyerRun::from_json(&before, lease).expect("the run as it was saved");
    assert_eq!(buyer.state(), BuyerState::Taking);
    stage
        .to_buyer(&mut buyer, &stage.peer_key(&stage.seller.trade))
        .await
        .expect("peer key");
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert_eq!(buyer.state(), BuyerState::Paid);
    assert_eq!(stage.payment(&buyer).await, PaymentState::InFlight);

    // The seller settled, and died before its run was saved: the preimage arrives again.
    let report = stage.open(&mut seller).await;
    stage.to_buyer(&mut buyer, &report).await.expect("report");
    stage.confirm(&seller);
    let out = buyer
        .tick(&stage.buyer_node, super::super::NOW)
        .await
        .expect("tick");
    let before = seller.to_json().expect("json");
    stage
        .peer_to_seller(&mut seller, &out[0])
        .await
        .expect("preimage");
    let mut seller = SellerRun::from_json(&before).expect("the run as it was saved");
    assert_eq!(seller.state(), SellerState::Opened);
    let again = stage
        .peer_to_seller(&mut seller, &out[0])
        .await
        .expect("preimage");
    assert_eq!(seller.state(), SellerState::Active);
    assert!(matches!(again.as_slice(), [Outgoing::ToInstance(_)]));
    assert_eq!(
        stage
            .seller_node
            .hold_invoice_state(&buyer.contract().payment_hash)
            .await
            .expect("state"),
        HoldState::Settled
    );
}
