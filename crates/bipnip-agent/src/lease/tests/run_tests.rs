//! The lease runs, seller and buyer, on the two-node simulator. Messages travel sealed, the way
//! an instance and a peer would send them; the test plays the instance.

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use bipnip_ln::{HoldInvoiceRequest, HoldState, LightningBackend, OpenRequest, PaymentState};
use lightning_invoice::Bolt11Invoice;
use mostro_core::bond::{MAX_BOND_CLTV_DELTA, payout_sat};
use mostro_core::lease::LeaseContract;
use mostro_core::lease_message::{LeaseInvoice, LeaseOffer, LeasePayload};
use mostro_core::node_binding::{
    Action as BindingAction, Network, SignedStatement, Statement, sign_ln_message,
};
use mostro_core::prelude::{Action, CantDoReason, Message, Payload, Peer};
use nostr_sdk::prelude::{FinalizeEvent, Keys, PublicKey};
use uuid::Uuid;

use super::{NOW, Node, World, confirm, pair, secret};
use crate::lease::run::{
    BondTerms, BuyerConfig, BuyerRun, BuyerState, Outgoing, RunError, SellerConfig, SellerRun,
    SellerState, TakeRequest,
};
use crate::lease::{DEFAULT_MAX_CLTV_DELTA, DEFAULT_SELLER_CLTV_DELTA, LeaseStepError};
use crate::lease_wire::{self, Incoming, WireError};

mod link_tests;
mod store_tests;

const SELLER: SellerConfig = SellerConfig {
    cltv_delta: DEFAULT_SELLER_CLTV_DELTA,
    feerate_sat_vb: 2,
};
const BUYER: BuyerConfig = BuyerConfig {
    max_fee_msat: 10_000,
    max_cltv_delta: DEFAULT_MAX_CLTV_DELTA,
};

/// A node runner's Nostr keys.
struct Agent {
    identity: Keys,
    trade: Keys,
}

impl Agent {
    fn new() -> Self {
        Agent {
            identity: Keys::generate(),
            trade: Keys::generate(),
        }
    }
}

/// `node`'s binding of `identity`.
fn binding(node: &Node, identity: &Keys) -> String {
    let secret = secp256k1::SecretKey::from_slice(&node.secret.secret_bytes()).expect("key");
    let node_key =
        secp256k1::PublicKey::from_secret_key(&secp256k1::Secp256k1::signing_only(), &secret);
    let statement = Statement::new(
        BindingAction::Bind,
        Network::Regtest,
        &identity.public_key().to_hex(),
        node_key,
        NOW - 60,
    )
    .expect("statement");
    let node_signature = sign_ln_message(statement.text().as_bytes(), &secret);
    SignedStatement {
        statement,
        node_signature,
    }
    .to_event_builder()
    .finalize(identity)
    .expect("event")
    .as_json()
}

/// The one message an answer holds, which must be for the instance.
fn to_instance(out: Vec<Outgoing>) -> Message {
    match out.as_slice() {
        [Outgoing::ToInstance(message)] => message.clone(),
        other => panic!("expected one message for the instance, got {other:?}"),
    }
}

/// Two nodes, their runners, and an instance played by the test.
struct Stage {
    seller_node: Node,
    buyer_node: Node,
    world: Arc<Mutex<World>>,
    instance: Keys,
    seller: Agent,
    buyer: Agent,
    offer: LeaseOffer,
    order_id: Uuid,
}

impl Stage {
    fn new() -> Self {
        let (seller_node, buyer_node, world) = pair();
        let seller = Agent::new();
        let offer = LeaseOffer {
            network: Network::Regtest,
            seller_node: seller_node.id(),
            binding: binding(&seller_node, &seller.identity),
            min_capacity_sat: 1_000_000,
            max_capacity_sat: 5_000_000,
            duration_blocks: 4_320,
            fee_base_sat: 1_000,
            fee_ppm: 2_500,
            upfront_percent: 50,
            min_confirmations: 3,
            announce: true,
            expires_at: NOW + 86_400,
            bond_sat: 0,
        };
        Stage {
            seller_node,
            buyer_node,
            world,
            instance: Keys::generate(),
            seller,
            buyer: Agent::new(),
            offer,
            order_id: Uuid::new_v4(),
        }
    }

    fn instance_key(&self) -> PublicKey {
        self.instance.public_key()
    }

    /// `message` as `from` sends it to the trade key `to`, and as `to` reads it.
    fn sent(message: &Message, from: &Keys, to: &Keys) -> (Incoming, PublicKey) {
        let event = lease_wire::seal_for_peer(message, from, to.public_key()).expect("seal");
        lease_wire::open(&event, to)
            .expect("open")
            .expect("addressed to the receiver")
    }

    /// The instance sends `message` to the seller.
    async fn to_seller(
        &self,
        run: &mut SellerRun,
        message: &Message,
    ) -> Result<Vec<Outgoing>, RunError> {
        let (incoming, sender) = Self::sent(message, &self.instance, &self.seller.trade);
        run.on_message(incoming, &sender, &self.seller_node, NOW)
            .await
    }

    /// The instance sends `message` to the buyer.
    async fn to_buyer(
        &self,
        run: &mut BuyerRun,
        message: &Message,
    ) -> Result<Vec<Outgoing>, RunError> {
        let (incoming, sender) = Self::sent(message, &self.instance, &self.buyer.trade);
        run.on_message(incoming, &sender, &self.buyer_node).await
    }

    fn peer_key(&self, of: &Keys) -> Message {
        Message::new_order(
            Some(self.order_id),
            None,
            None,
            Action::TradePubkey,
            Some(Payload::Peer(Peer::new(of.public_key().to_hex(), None))),
        )
    }

    fn canceled(&self) -> Message {
        Message::new_order(Some(self.order_id), None, None, Action::Canceled, None)
    }

    /// The seller's run with its offer open under the stage's order id.
    async fn offered(&self) -> SellerRun {
        let (mut run, out) =
            SellerRun::offer(self.instance_key(), self.offer.clone(), 1, 7, SELLER);
        assert!(to_instance(vec![out]).verify());
        assert_eq!(run.state(), SellerState::Offering);
        let echo = Message::new_order(
            Some(self.order_id),
            Some(1),
            None,
            Action::LeaseOffer,
            Some(Payload::Lease(LeasePayload::Offer(self.offer.clone()))),
        );
        let out = self.to_seller(&mut run, &echo).await.expect("echo");
        assert!(out.is_empty());
        assert_eq!(run.state(), SellerState::Open);
        assert_eq!(run.order_id(), Some(self.order_id));
        run
    }

    fn take_request(&self) -> TakeRequest {
        TakeRequest {
            order_id: self.order_id,
            request_id: 2,
            trade_index: 8,
            seller_node: self.offer.seller_node.clone(),
            network: Network::Regtest,
            terms: self.offer.terms_for(2_000_000).expect("terms"),
            binding: binding(&self.buyer_node, &self.buyer.identity),
            node_address: Some("203.0.113.7:9735".into()),
            bond: None,
        }
    }

    /// The buyer's run and its take message.
    async fn take(&self, request: TakeRequest) -> (BuyerRun, Message) {
        let (run, out) = BuyerRun::take(&self.buyer_node, self.instance_key(), request, BUYER, NOW)
            .await
            .expect("take");
        assert_eq!(run.state(), BuyerState::Taking);
        (run, to_instance(vec![out]))
    }

    /// Both runs with the take accepted and the trade keys exchanged; the seller's invoice
    /// message is not delivered yet.
    async fn taken(&self) -> (SellerRun, BuyerRun, Message) {
        let mut seller = self.offered().await;
        let (mut buyer, take) = self.take(self.take_request()).await;
        let invoice = to_instance(self.to_seller(&mut seller, &take).await.expect("take"));
        assert_eq!(seller.state(), SellerState::Invoiced);
        // Sent twice: a repeated key is fine, a changed one is not.
        for _ in 0..2 {
            let out = self
                .to_seller(&mut seller, &self.peer_key(&self.buyer.trade))
                .await;
            assert!(out.expect("peer key").is_empty());
        }
        let out = self
            .to_buyer(&mut buyer, &self.peer_key(&self.seller.trade))
            .await;
        assert!(out.expect("peer key").is_empty());
        (seller, buyer, invoice)
    }

    /// Both runs with the buyer's payment locked and the channel opening, unconfirmed.
    async fn opened(&self) -> (SellerRun, BuyerRun) {
        let (mut seller, mut buyer, invoice) = self.taken().await;
        self.to_buyer(&mut buyer, &invoice).await.expect("invoice");
        assert_eq!(buyer.state(), BuyerState::Paid);
        let report = self.open(&mut seller).await;
        assert_eq!(seller.state(), SellerState::Opened);
        self.to_buyer(&mut buyer, &report).await.expect("report");
        assert_eq!(buyer.state(), BuyerState::Verifying);
        (seller, buyer)
    }

    /// The seller notices the locked payment, then opens the channel: two looks at its node,
    /// with room for a save in between.
    async fn open(&self, seller: &mut SellerRun) -> Message {
        let noticed = seller.tick(&self.seller_node).await.expect("tick");
        assert!(noticed.is_empty());
        assert_eq!(seller.state(), SellerState::Opening);
        let report = to_instance(seller.tick(&self.seller_node).await.expect("tick"));
        assert_eq!(seller.state(), SellerState::Opened);
        report
    }

    /// A stage whose offer asks for `percent` of the fee upfront.
    fn with_upfront(percent: u8) -> Self {
        let mut stage = Self::new();
        stage.offer.upfront_percent = percent;
        stage
    }

    /// Both runs with the lease active and the channel three blocks deep.
    async fn active(&self) -> (SellerRun, BuyerRun) {
        let (mut seller, mut buyer) = self.opened().await;
        self.confirm(&seller);
        let out = buyer.tick(&self.buyer_node).await.expect("tick");
        let active = to_instance(
            self.peer_to_seller(&mut seller, &out[0])
                .await
                .expect("preimage"),
        );
        self.to_buyer(&mut buyer, &active).await.expect("active");
        assert_eq!(seller.state(), SellerState::Active);
        assert_eq!(buyer.state(), BuyerState::Active);
        (seller, buyer)
    }

    /// The leased channel now has this many confirmations.
    fn age(&self, seller: &SellerRun, confirmations: u32) {
        confirm(
            &self.world,
            seller.funding_outpoint().expect("an outpoint"),
            confirmations,
        );
    }

    fn close_channels(&self) {
        self.world.lock().expect("world").channels.clear();
    }

    fn payments(&self) -> usize {
        self.world.lock().expect("world").payments.len()
    }

    /// A completion invoice from `node` for `amount_msat`, as a message for `contract`.
    async fn bill(&self, node: &Node, contract: &LeaseContract, amount_msat: u64) -> Message {
        let bolt11 = node
            .add_invoice(amount_msat, 3_600, "completion")
            .await
            .expect("invoice");
        lease_wire::completion_invoice(self.order_id, contract, bolt11)
    }

    fn confirm(&self, seller: &SellerRun) {
        confirm(
            &self.world,
            seller.funding_outpoint().expect("an outpoint"),
            3,
        );
    }

    /// The seller reads what the buyer sent peer to peer.
    async fn peer_to_seller(
        &self,
        run: &mut SellerRun,
        out: &Outgoing,
    ) -> Result<Vec<Outgoing>, RunError> {
        let event = out
            .seal(
                &self.buyer.identity,
                &self.buyer.trade,
                self.instance_key(),
                0,
            )
            .expect("seal");
        assert_eq!(event.pubkey, self.buyer.trade.public_key());
        let (incoming, sender) = lease_wire::open(&event, &self.seller.trade)
            .expect("open")
            .expect("for the seller");
        run.on_message(incoming, &sender, &self.seller_node, NOW)
            .await
    }

    async fn hold_state(&self, run: &BuyerRun) -> HoldState {
        self.seller_node
            .hold_invoice_state(&run.contract().payment_hash)
            .await
            .expect("hold invoice")
    }

    async fn payment(&self, run: &BuyerRun) -> PaymentState {
        self.buyer_node
            .payment_state(&run.contract().payment_hash)
            .await
            .expect("payment")
    }

    fn channels(&self) -> usize {
        self.world.lock().expect("world").channels.len()
    }
}

#[tokio::test]
async fn a_lease_runs_from_offer_to_active_on_messages_alone() {
    let stage = Stage::new();
    let mut seller = stage.offered().await;
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );

    let (mut buyer, take) = stage.take(stage.take_request()).await;
    let invoice = to_instance(stage.to_seller(&mut seller, &take).await.expect("take"));
    assert_eq!(seller.state(), SellerState::Invoiced);
    assert_eq!(seller.contract(), Some(buyer.contract()));
    stage
        .to_seller(&mut seller, &stage.peer_key(&stage.buyer.trade))
        .await
        .expect("peer key");
    stage
        .to_buyer(&mut buyer, &stage.peer_key(&stage.seller.trade))
        .await
        .expect("peer key");

    // No channel on a promise: nothing opens until the buyer's payment is locked.
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.channels(), 0);

    assert!(
        stage
            .to_buyer(&mut buyer, &invoice)
            .await
            .expect("invoice")
            .is_empty()
    );
    assert_eq!(buyer.state(), BuyerState::Paid);
    assert_eq!(stage.hold_state(&buyer).await, HoldState::Accepted);

    let report = stage.open(&mut seller).await;
    assert_eq!(seller.state(), SellerState::Opened);
    // Opened once, however often the seller looks.
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.channels(), 1);

    stage.to_buyer(&mut buyer, &report).await.expect("report");
    assert_eq!(buyer.state(), BuyerState::Verifying);
    // Unconfirmed: the buyer's node says no, and nothing is released.
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert!(buyer.waiting_on().is_some());
    assert_eq!(buyer.state(), BuyerState::Verifying);

    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node).await.expect("tick");
    assert_eq!(buyer.state(), BuyerState::Released);
    assert!(buyer.waiting_on().is_none());
    let [Outgoing::ToPeer { to, .. }] = out.as_slice() else {
        panic!("expected the preimage for the peer, got {out:?}");
    };
    assert_eq!(*to, stage.seller.trade.public_key());
    // Released once.
    assert!(
        buyer
            .tick(&stage.buyer_node)
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
    assert_eq!(seller.state(), SellerState::Active);
    let PaymentState::Succeeded { preimage } = stage.payment(&buyer).await else {
        panic!("the payment did not settle");
    };
    assert!(
        !format!("{out:?}").contains(&preimage),
        "the preimage is never printed"
    );

    stage.to_buyer(&mut buyer, &active).await.expect("active");
    assert_eq!(buyer.state(), BuyerState::Active);
}

/// The messages as they travel, for comparing two answers.
fn wire(out: &[Outgoing]) -> Vec<String> {
    out.iter()
        .map(|outgoing| match outgoing {
            Outgoing::ToInstance(message) | Outgoing::ToPeer { message, .. } => {
                message.as_json().expect("json")
            }
        })
        .collect()
}

#[tokio::test]
async fn a_released_preimage_is_sent_again_after_a_restart_and_nothing_else_is() {
    let stage = Stage::new();
    let (seller, mut buyer) = stage.opened().await;
    // Taken up again while it waits for confirmations: nothing is owed.
    buyer.resumed();
    assert!(!buyer.unsent());

    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node).await.expect("tick");
    assert_eq!(buyer.state(), BuyerState::Released);
    assert_eq!(out.len(), 1);
    assert!(!buyer.unsent());
    // The process ends here, the release stored and the preimage perhaps never sent. The next
    // process owes it again, and makes it again from its own node's view of the channel.
    buyer.resumed();
    assert!(buyer.unsent());
    let again = buyer.pending(&stage.buyer_node).await.expect("pending");
    let [Outgoing::ToPeer { to, .. }] = again.as_slice() else {
        panic!("expected the preimage for the peer, got {again:?}");
    };
    assert_eq!(*to, stage.seller.trade.public_key());
    // If the channel is gone by then, nothing is released.
    stage.close_channels();
    assert!(buyer.pending(&stage.buyer_node).await.is_err());
}

#[tokio::test]
async fn each_state_can_make_its_message_again() {
    let stage = Stage::new();

    // The seller: its offer, its invoice, its channel report, its active notice.
    let (mut seller, offer) =
        SellerRun::offer(stage.instance_key(), stage.offer.clone(), 1, 7, SELLER);
    assert_eq!(wire(&seller.pending().expect("pending")), wire(&[offer]));
    let echo = Message::new_order(
        Some(stage.order_id),
        Some(1),
        None,
        Action::LeaseOffer,
        Some(Payload::Lease(LeasePayload::Offer(stage.offer.clone()))),
    );
    stage.to_seller(&mut seller, &echo).await.expect("echo");
    assert!(
        seller.pending().expect("pending").is_empty(),
        "open says nothing"
    );

    let (mut buyer, take) = stage.take(stage.take_request()).await;
    let again = buyer.pending(&stage.buyer_node).await.expect("pending");
    assert_eq!(wire(&again), vec![take.as_json().expect("json")]);

    let invoice = stage.to_seller(&mut seller, &take).await.expect("take");
    assert_eq!(wire(&seller.pending().expect("pending")), wire(&invoice));
    stage
        .to_seller(&mut seller, &stage.peer_key(&stage.buyer.trade))
        .await
        .expect("peer key");
    stage
        .to_buyer(&mut buyer, &stage.peer_key(&stage.seller.trade))
        .await
        .expect("peer key");
    stage
        .to_buyer(&mut buyer, &to_instance(invoice))
        .await
        .expect("invoice");
    let paid = buyer.pending(&stage.buyer_node).await.expect("pending");
    assert!(paid.is_empty(), "paid says nothing");

    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert!(
        seller.pending().expect("pending").is_empty(),
        "opening says nothing"
    );
    let report = seller.tick(&stage.seller_node).await.expect("tick");
    assert_eq!(wire(&seller.pending().expect("pending")), wire(&report));

    stage
        .to_buyer(&mut buyer, &to_instance(report))
        .await
        .expect("report");
    stage.confirm(&seller);
    let released = buyer.tick(&stage.buyer_node).await.expect("tick");
    assert_eq!(buyer.state(), BuyerState::Released);
    // The preimage again, to the same key, through the same check of the channel.
    let again = buyer.pending(&stage.buyer_node).await.expect("pending");
    assert_eq!(wire(&again), wire(&released));
    assert!(matches!(
        again.as_slice(),
        [Outgoing::ToPeer { to, .. }] if *to == stage.seller.trade.public_key()
    ));
    // If the channel no longer passes the check, nothing is made.
    confirm(
        &stage.world,
        seller.funding_outpoint().expect("an outpoint"),
        0,
    );
    let gone = buyer.pending(&stage.buyer_node).await;
    assert!(
        matches!(gone, Err(RunError::Step(LeaseStepError::Channel(_)))),
        "{gone:?}"
    );
    stage.confirm(&seller);

    let active = stage
        .peer_to_seller(&mut seller, &released[0])
        .await
        .expect("preimage");
    assert_eq!(wire(&seller.pending().expect("pending")), wire(&active));

    // What is owed is part of the stored run.
    seller.set_unsent(true);
    let stored = SellerRun::from_json(&seller.to_json().expect("json")).expect("stored");
    assert!(stored.unsent());
    assert_eq!(wire(&stored.pending().expect("pending")), wire(&active));
}

#[tokio::test]
async fn the_completion_share_is_billed_once_the_lease_has_run_and_paid_while_the_channel_stands() {
    let stage = Stage::new(); // half the fee upfront, 4 320 blocks
    let (mut seller, mut buyer) = stage.active().await;
    assert!(!seller.finished() && !buyer.finished());
    let completion_msat = buyer.contract().terms.completion_sat().expect("share") * 1000;
    assert!(completion_msat > 0);

    // One block short of the lease's length: no bill.
    stage.age(&seller, 4_319);
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Active);

    stage.age(&seller, 4_320);
    let bill = seller.tick(&stage.seller_node).await.expect("tick");
    assert_eq!(seller.state(), SellerState::Completing);
    assert_eq!(wire(&seller.pending().expect("pending")), wire(&bill));
    // Billed once, and stored.
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    let stored = SellerRun::from_json(&seller.to_json().expect("json")).expect("stored");
    assert_eq!(stored.state(), SellerState::Completing);

    let bill = to_instance(bill);
    let Incoming::CompletionInvoice { invoice, .. } = lease_wire::read(&bill).expect("bill") else {
        panic!("not a completion invoice");
    };
    let parsed = Bolt11Invoice::from_str(&invoice.invoice).expect("bolt11");
    assert_eq!(parsed.amount_milli_satoshis(), Some(completion_msat));

    let before = buyer.to_json().expect("json");
    let paid_before = stage.payments();
    stage.to_buyer(&mut buyer, &bill).await.expect("bill");
    assert_eq!(buyer.state(), BuyerState::Complete);
    assert!(buyer.finished());
    assert_eq!(stage.payments(), paid_before + 1);

    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Complete);
    assert!(seller.finished());

    // A buyer that stopped after paying and before it was saved does not pay again.
    let lease_file = std::env::temp_dir().join(format!(
        "bipnip-completion-{}-{}",
        std::process::id(),
        buyer.store_key()
    ));
    let _ = std::fs::remove_file(&lease_file);
    buyer.save_lease(&lease_file).expect("save");
    let lease = crate::lease::BuyerLease::load(&lease_file).expect("load");
    let mut buyer = BuyerRun::from_json(&before, lease).expect("the run as it was saved");
    assert_eq!(buyer.state(), BuyerState::Active);
    stage.to_buyer(&mut buyer, &bill).await.expect("bill");
    assert_eq!(buyer.state(), BuyerState::Complete);
    assert_eq!(stage.payments(), paid_before + 1);
}

#[tokio::test]
async fn a_bill_that_is_early_or_wrong_is_not_paid() {
    let stage = Stage::new();
    let (seller, mut buyer) = stage.active().await;
    let contract = buyer.contract().clone();
    let completion_msat = contract.terms.completion_sat().expect("share") * 1000;
    let paid_before = stage.payments();
    let good = stage
        .bill(&stage.seller_node, &contract, completion_msat)
        .await;

    // Before the channel is as old as the lease is long. It may fit later.
    let result = stage.to_buyer(&mut buyer, &good).await;
    assert!(matches!(result, Err(RunError::OutOfOrder(_))), "{result:?}");
    stage.age(&seller, 4_320);

    // More than the agreed share; an invoice that pays another node; another lease's id.
    let too_much = stage
        .bill(&stage.seller_node, &contract, completion_msat + 1_000)
        .await;
    let elsewhere = stage
        .bill(&stage.buyer_node, &contract, completion_msat)
        .await;
    for wrong in [too_much, elsewhere] {
        let result = stage.to_buyer(&mut buyer, &wrong).await;
        assert!(
            matches!(result, Err(RunError::Step(LeaseStepError::Invoice(_)))),
            "{result:?}"
        );
    }
    let mut other = contract.clone();
    other.created_at += 1;
    let other_lease = stage
        .bill(&stage.seller_node, &other, completion_msat)
        .await;
    let result = stage.to_buyer(&mut buyer, &other_lease).await;
    assert!(
        matches!(result, Err(RunError::Wire(WireError::NotOurs(_)))),
        "{result:?}"
    );
    // A bill that does not come from the instance.
    let (incoming, sender) = Stage::sent(&good, &Keys::generate(), &stage.buyer.trade);
    let result = buyer.on_message(incoming, &sender, &stage.buyer_node).await;
    assert!(matches!(result, Err(RunError::Sender)), "{result:?}");

    assert_eq!(buyer.state(), BuyerState::Active);
    assert_eq!(stage.payments(), paid_before, "nothing was paid");

    stage
        .to_buyer(&mut buyer, &good)
        .await
        .expect("the real bill");
    assert_eq!(buyer.state(), BuyerState::Complete);
}

#[tokio::test]
async fn a_channel_that_is_gone_ends_the_lease_unbilled_and_unpaid() {
    let stage = Stage::new();
    let (mut seller, mut buyer) = stage.active().await;
    let contract = buyer.contract().clone();
    let completion_msat = contract.terms.completion_sat().expect("share") * 1000;
    let bill = stage
        .bill(&stage.seller_node, &contract, completion_msat)
        .await;
    let paid_before = stage.payments();
    stage.age(&seller, 4_320);
    stage.close_channels();

    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Closed);
    assert!(seller.finished());

    // A seller that bills anyway is not paid.
    let out = stage.to_buyer(&mut buyer, &bill).await.expect("handled");
    assert!(out.is_empty());
    assert_eq!(buyer.state(), BuyerState::Closed);
    assert!(buyer.finished());
    assert_eq!(stage.payments(), paid_before);
}

#[tokio::test]
async fn a_lease_paid_all_upfront_ends_when_it_is_active() {
    let stage = Stage::with_upfront(100);
    let (mut seller, mut buyer) = stage.active().await;
    assert!(seller.finished() && buyer.finished());
    let contract = buyer.contract().clone();
    assert_eq!(contract.terms.completion_sat(), Ok(0));

    stage.age(&seller, 4_320);
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(seller.state(), SellerState::Active);
    // Nothing more is owed, whatever is billed.
    let extra = stage.bill(&stage.seller_node, &contract, 1_000).await;
    let result = stage.to_buyer(&mut buyer, &extra).await;
    assert!(
        matches!(result, Err(RunError::Step(LeaseStepError::Invoice(_)))),
        "{result:?}"
    );
    assert_eq!(buyer.state(), BuyerState::Active);
}

#[tokio::test]
async fn messages_from_the_wrong_sender_do_nothing() {
    let stage = Stage::new();
    let stranger = Keys::generate();
    let (mut seller, mut buyer, invoice) = stage.taken().await;

    // An invoice that does not come from the instance is not paid.
    let (incoming, sender) = Stage::sent(&invoice, &stranger, &stage.buyer.trade);
    let result = buyer.on_message(incoming, &sender, &stage.buyer_node).await;
    assert!(matches!(result, Err(RunError::Sender)), "{result:?}");
    assert_eq!(buyer.state(), BuyerState::Taking);
    assert!(stage.world.lock().expect("world").payments.is_empty());

    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    let report = stage.open(&mut seller).await;
    stage.to_buyer(&mut buyer, &report).await.expect("report");
    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node).await.expect("tick");
    let [Outgoing::ToPeer { message, .. }] = out.as_slice() else {
        panic!("expected the preimage for the peer, got {out:?}");
    };

    // A preimage the instance relays, or a stranger sends, settles nothing.
    for from in [&stage.instance, &stranger] {
        let (incoming, sender) = Stage::sent(message, from, &stage.seller.trade);
        let result = seller
            .on_message(incoming, &sender, &stage.seller_node, NOW)
            .await;
        assert!(matches!(result, Err(RunError::Sender)), "{result:?}");
    }
    assert_eq!(seller.state(), SellerState::Opened);
    assert_eq!(stage.hold_state(&buyer).await, HoldState::Accepted);

    // And it is never wrapped for the instance.
    let misdirected = Outgoing::ToInstance(message.clone()).seal(
        &stage.buyer.identity,
        &stage.buyer.trade,
        stage.instance_key(),
        0,
    );
    assert!(matches!(misdirected, Err(WireError::Shape(_))));

    // A changed peer key is refused.
    let result = stage
        .to_seller(&mut seller, &stage.peer_key(&stranger))
        .await;
    assert!(matches!(result, Err(RunError::OutOfOrder(_))), "{result:?}");
    stage
        .peer_to_seller(&mut seller, &out[0])
        .await
        .expect("the buyer's own preimage still settles");
    assert_eq!(seller.state(), SellerState::Active);
}

#[tokio::test]
async fn a_take_that_is_not_the_offer_gets_no_invoice() {
    let stage = Stage::new();
    let mut seller = stage.offered().await;

    let mut cheaper = stage.take_request();
    cheaper.terms.fee_ppm = 1;
    let (_, take) = stage.take(cheaper).await;
    let result = stage.to_seller(&mut seller, &take).await;
    assert!(
        matches!(result, Err(RunError::Wire(WireError::Invalid(_)))),
        "{result:?}"
    );
    assert_eq!(seller.state(), SellerState::Open);
    assert!(stage.world.lock().expect("world").invoices.is_empty());

    // A message about another order is not ours.
    let other = Message::new_order(
        Some(Uuid::new_v4()),
        None,
        None,
        Action::TradePubkey,
        Some(Payload::Peer(Peer::new(
            stage.buyer.trade.public_key().to_hex(),
            None,
        ))),
    );
    let result = stage.to_seller(&mut seller, &other).await;
    assert!(matches!(result, Err(RunError::OtherOrder)), "{result:?}");

    // The offer is still open for a proper take.
    let (_, take) = stage.take(stage.take_request()).await;
    stage.to_seller(&mut seller, &take).await.expect("take");
    assert_eq!(seller.state(), SellerState::Invoiced);
}

#[tokio::test]
async fn an_invoice_for_another_lease_is_not_paid() {
    let stage = Stage::new();
    let (_seller, mut buyer, invoice) = stage.taken().await;
    let Incoming::UpfrontInvoice { invoice, .. } = lease_wire::read(&invoice).expect("invoice")
    else {
        panic!("not an invoice");
    };
    let mut other = buyer.contract().clone();
    other.created_at += 1;
    let wrong = lease_wire::upfront_invoice(stage.order_id, &other, invoice.invoice);
    let result = stage.to_buyer(&mut buyer, &wrong).await;
    assert!(
        matches!(result, Err(RunError::Wire(WireError::NotOurs(_)))),
        "{result:?}"
    );
    assert_eq!(buyer.state(), BuyerState::Taking);
    assert!(stage.world.lock().expect("world").payments.is_empty());
}

#[tokio::test]
async fn a_channel_that_existed_before_the_lease_is_not_paid_for() {
    let stage = Stage::new();
    // The seller opened a channel to the buyer some time ago, with the very terms on offer.
    let old = stage
        .seller_node
        .open_channel(&OpenRequest {
            remote_pubkey: stage.buyer_node.id(),
            remote_addr: None,
            capacity_sat: 2_000_000,
            announce: true,
            feerate_sat_vb: 2,
        })
        .await
        .expect("an earlier channel");
    confirm(&stage.world, &old, 500);

    let (seller, mut buyer, invoice) = stage.taken().await;
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    // It reports the old channel instead of opening one.
    let report =
        lease_wire::channel_opened(stage.order_id, buyer.contract(), &old).expect("report");
    let result = stage.to_buyer(&mut buyer, &report).await;
    assert!(
        matches!(result, Err(RunError::Step(LeaseStepError::Channel(_)))),
        "{result:?}"
    );
    assert_eq!(buyer.state(), BuyerState::Paid);
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.payment(&buyer).await, PaymentState::InFlight);
    assert_eq!(seller.state(), SellerState::Invoiced);
}

#[tokio::test]
async fn a_cancel_before_the_channel_gives_the_payment_back() {
    let stage = Stage::new();
    let (mut seller, mut buyer, invoice) = stage.taken().await;
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert_eq!(stage.payment(&buyer).await, PaymentState::InFlight);

    stage
        .to_seller(&mut seller, &stage.canceled())
        .await
        .expect("canceled");
    assert_eq!(seller.state(), SellerState::Canceled);
    assert_eq!(stage.hold_state(&buyer).await, HoldState::Canceled);
    stage
        .to_buyer(&mut buyer, &stage.canceled())
        .await
        .expect("canceled");
    assert_eq!(buyer.state(), BuyerState::Canceled);
    assert!(matches!(
        stage.payment(&buyer).await,
        PaymentState::Failed { .. }
    ));

    // Nothing more happens on either side.
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.channels(), 0);
}

#[tokio::test]
async fn an_instance_cannot_cancel_a_lease_whose_payment_is_locked_and_channel_open() {
    let stage = Stage::new();
    let (mut seller, mut buyer) = stage.opened().await;

    stage
        .to_seller(&mut seller, &stage.canceled())
        .await
        .expect("canceled");
    stage
        .to_buyer(&mut buyer, &stage.canceled())
        .await
        .expect("canceled");
    // The seller keeps the payment locked and waits; the buyer's node still sees its payment
    // locked, so the buyer goes on checking the channel.
    assert_eq!(seller.state(), SellerState::Opened);
    assert!(seller.canceled_by_instance());
    assert_eq!(stage.hold_state(&buyer).await, HoldState::Accepted);
    assert_eq!(buyer.state(), BuyerState::Verifying);

    stage.confirm(&seller);
    let out = buyer.tick(&stage.buyer_node).await.expect("tick");
    stage
        .peer_to_seller(&mut seller, &out[0])
        .await
        .expect("preimage");
    assert_eq!(seller.state(), SellerState::Active);
    assert!(matches!(
        stage.payment(&buyer).await,
        PaymentState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn no_preimage_is_released_for_a_payment_that_came_back() {
    let stage = Stage::new();
    let (seller, mut buyer) = stage.opened().await;

    // The seller's node gives the payment back; the channel then confirms anyway.
    stage
        .seller_node
        .cancel_hold_invoice(&buyer.contract().payment_hash)
        .await
        .expect("cancel");
    stage.confirm(&seller);
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(buyer.state(), BuyerState::Canceled);
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
}

#[tokio::test]
async fn either_side_can_back_out_only_before_money_or_a_channel_is_at_stake() {
    let stage = Stage::new();

    // The seller withdraws an open offer.
    let mut seller = stage.offered().await;
    let cancel = to_instance(seller.cancel(&stage.seller_node, 9).await.expect("cancel"));
    assert_eq!(cancel.get_inner_message_kind().action, Action::Cancel);
    assert_eq!(seller.state(), SellerState::Canceled);
    assert!(seller.cancel(&stage.seller_node, 9).await.is_err());

    // The buyer backs out before paying.
    let (mut buyer, _) = stage.take(stage.take_request()).await;
    let cancel = to_instance(buyer.cancel(9).expect("cancel"));
    assert_eq!(cancel.get_inner_message_kind().id, Some(stage.order_id));
    assert_eq!(buyer.state(), BuyerState::Canceled);

    // The seller backs out after the buyer paid and before opening: the payment goes back.
    let (mut seller, mut buyer, invoice) = stage.taken().await;
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert!(buyer.cancel(9).is_err(), "nothing for the buyer to cancel");
    seller.cancel(&stage.seller_node, 9).await.expect("cancel");
    assert!(matches!(
        stage.payment(&buyer).await,
        PaymentState::Failed { .. }
    ));

    // Once the channel is opening the seller cannot back out.
    let (mut seller, _buyer) = stage.opened().await;
    assert!(seller.cancel(&stage.seller_node, 9).await.is_err());
    assert_eq!(seller.state(), SellerState::Opened);
}

#[tokio::test]
async fn a_refusal_from_the_instance_ends_what_it_refused() {
    let stage = Stage::new();
    let refusal = |order_id: Option<Uuid>, request_id: u64| {
        Message::cant_do(
            order_id,
            Some(request_id),
            Some(Payload::CantDo(Some(CantDoReason::TooManyRequests))),
        )
    };

    let (mut seller, _) = SellerRun::offer(stage.instance_key(), stage.offer.clone(), 1, 7, SELLER);
    // A refusal of someone else's request is not ours.
    let result = stage.to_seller(&mut seller, &refusal(None, 77)).await;
    assert!(matches!(result, Err(RunError::OtherOrder)), "{result:?}");
    assert_eq!(seller.state(), SellerState::Offering);
    let result = stage.to_seller(&mut seller, &refusal(None, 1)).await;
    assert!(
        matches!(
            result,
            Err(RunError::Refused(Some(CantDoReason::TooManyRequests)))
        ),
        "{result:?}"
    );
    assert_eq!(seller.state(), SellerState::Canceled);

    let (mut buyer, _) = stage.take(stage.take_request()).await;
    let result = stage
        .to_buyer(&mut buyer, &refusal(Some(stage.order_id), 2))
        .await;
    assert!(matches!(result, Err(RunError::Refused(_))), "{result:?}");
    assert_eq!(buyer.state(), BuyerState::Canceled);
}

// ---- the buyer's bond ----------------------------------------------------------------------

/// What the buyer of a bonded test lease locks.
const BOND: u64 = 50_000;

/// A stage whose offer asks its buyer for a bond, and the instance's own node.
fn bonded_stage() -> (Stage, Node) {
    let mut stage = Stage::new();
    stage.offer.bond_sat = BOND;
    let house = Node {
        world: stage.world.clone(),
        secret: secret(99),
        network: "regtest",
    };
    (stage, house)
}

/// A bond invoice as the instance would send it: a hold invoice from `node` for `amount_sat`.
async fn bond_invoice(
    stage: &Stage,
    node: &Node,
    lease_id: String,
    amount_sat: u64,
    cltv_delta: u32,
    n: u8,
) -> Message {
    let request = HoldInvoiceRequest {
        payment_hash: format!("{n:02x}").repeat(32),
        amount_msat: amount_sat * 1_000,
        expiry_secs: 3_600,
        cltv_delta,
        description: "bond".into(),
    };
    let invoice = node.add_hold_invoice(&request).await.expect("invoice");
    Message::new_order(
        Some(stage.order_id),
        None,
        None,
        Action::LeaseBondInvoice,
        Some(Payload::Lease(LeasePayload::Invoice(LeaseInvoice {
            lease_id,
            invoice,
        }))),
    )
}

#[tokio::test]
async fn the_buyer_pays_its_bond_once_and_only_to_the_instances_node_for_the_orders_amount() {
    let (stage, house) = bonded_stage();
    let mut request = stage.take_request();
    request.bond = Some(BondTerms {
        instance_node: house.id(),
        bond_sat: BOND,
    });
    let (mut buyer, take) = stage.take(request).await;
    let lease_id = buyer.contract().id();
    assert_eq!(buyer.bond_status(), "awaited");
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.payments(), 0);

    // Invoices that are not the order's bond to the instance's node are never kept.
    let stranger = Node {
        world: stage.world.clone(),
        secret: secret(42),
        network: "regtest",
    };
    let wrong = [
        (
            "another node",
            bond_invoice(&stage, &stranger, lease_id.clone(), BOND, 300, 1).await,
        ),
        (
            "the seller's node",
            bond_invoice(&stage, &stage.seller_node, lease_id.clone(), BOND, 300, 2).await,
        ),
        (
            "a larger amount",
            bond_invoice(&stage, &house, lease_id.clone(), BOND + 1, 300, 3).await,
        ),
        (
            "too long a lock",
            bond_invoice(
                &stage,
                &house,
                lease_id.clone(),
                BOND,
                MAX_BOND_CLTV_DELTA + 1,
                4,
            )
            .await,
        ),
        (
            "another lease",
            bond_invoice(&stage, &house, "ef".repeat(32), BOND, 300, 5).await,
        ),
    ];
    for (what, message) in wrong {
        let refused = stage.to_buyer(&mut buyer, &message).await;
        assert!(refused.is_err(), "{what}: {refused:?}");
        assert!(buyer.bond().is_none(), "{what}");
    }
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.payments(), 0);

    // The real one is kept, and paid only on the next look at the node, once.
    let good = bond_invoice(&stage, &house, lease_id.clone(), BOND, 300, 6).await;
    assert!(
        stage
            .to_buyer(&mut buyer, &good)
            .await
            .expect("bond")
            .is_empty()
    );
    assert_eq!(buyer.bond_status(), "checked");
    assert_eq!(stage.payments(), 0);
    let hash = "06".repeat(32);
    assert_eq!(buyer.bond().expect("bond").payment_hash, hash);
    for _ in 0..3 {
        assert!(
            buyer
                .tick(&stage.buyer_node)
                .await
                .expect("tick")
                .is_empty()
        );
        assert_eq!(buyer.bond_status(), "sent");
        assert_eq!(stage.payments(), 1);
    }
    {
        let world = stage.world.lock().expect("world");
        assert_eq!(world.invoices.get(&hash), Some(&HoldState::Accepted));
        assert_eq!(world.payments.get(&hash), Some(&PaymentState::InFlight));
    }
    // The same invoice again is fine; another one is not taken.
    assert!(
        stage
            .to_buyer(&mut buyer, &good)
            .await
            .expect("again")
            .is_empty()
    );
    let second = bond_invoice(&stage, &house, lease_id, BOND, 300, 7).await;
    let refused = stage.to_buyer(&mut buyer, &second).await;
    assert!(
        matches!(refused, Err(RunError::OutOfOrder(_))),
        "{refused:?}"
    );
    assert_eq!(stage.payments(), 1);

    // The instance accepted the take (it sent the bond invoice), so a later refusal, of a take
    // that went out twice, does not end the run.
    let refusal = Message::cant_do(
        Some(stage.order_id),
        Some(2),
        Some(Payload::CantDo(Some(CantDoReason::NotAllowedByStatus))),
    );
    let refused = stage.to_buyer(&mut buyer, &refusal).await;
    assert!(matches!(refused, Err(RunError::Refused(_))), "{refused:?}");
    assert_eq!(buyer.state(), BuyerState::Taking);

    // With the bond locked the take reaches the seller, and the lease runs as any other.
    let mut seller = stage.offered().await;
    let invoice = to_instance(stage.to_seller(&mut seller, &take).await.expect("take"));
    stage
        .to_buyer(&mut buyer, &stage.peer_key(&stage.seller.trade))
        .await
        .expect("peer key");
    stage.to_buyer(&mut buyer, &invoice).await.expect("invoice");
    assert_eq!(buyer.state(), BuyerState::Paid);
    assert_eq!(stage.payments(), 2);
}

#[tokio::test]
async fn a_lease_without_a_bond_takes_no_bond_invoice() {
    let stage = Stage::new();
    let house = Node {
        world: stage.world.clone(),
        secret: secret(99),
        network: "regtest",
    };
    let (mut buyer, _) = stage.take(stage.take_request()).await;
    let lease_id = buyer.contract().id();
    let message = bond_invoice(&stage, &house, lease_id, BOND, 300, 1).await;
    let refused = stage.to_buyer(&mut buyer, &message).await;
    assert!(
        matches!(refused, Err(RunError::OutOfOrder(_))),
        "{refused:?}"
    );
    assert_eq!(buyer.bond_status(), "-");
    assert!(
        buyer
            .tick(&stage.buyer_node)
            .await
            .expect("tick")
            .is_empty()
    );
    assert_eq!(stage.payments(), 0);
}

/// The invoice in a bond claim the seller sent.
fn claimed(out: Vec<Outgoing>, lease_id: &str) -> String {
    let message = to_instance(out);
    assert!(message.verify());
    let kind = message.get_inner_message_kind();
    assert_eq!(kind.action, Action::LeaseBondClaim);
    let Some(Payload::Lease(LeasePayload::Invoice(claim))) = &kind.payload else {
        panic!("not a claim");
    };
    assert_eq!(claim.lease_id, lease_id);
    claim.invoice.clone()
}

#[tokio::test]
async fn a_seller_owed_the_buyers_bond_claims_it_and_then_gives_the_upfront_payment_back() {
    let (stage, house) = bonded_stage();
    let (mut seller, _buyer) = stage.opened().await;
    let contract = seller.contract().expect("contract").clone();
    let node = &stage.seller_node;
    let owed = Message::new_order(
        Some(stage.order_id),
        None,
        None,
        Action::LeaseBondClaim,
        None,
    );

    // The buyer never releases the preimage. The instance says the lease is canceled; the
    // seller keeps the payment locked and claims nothing it was not told it is owed.
    stage
        .to_seller(&mut seller, &stage.canceled())
        .await
        .expect("canceled");
    assert_eq!(seller.state(), SellerState::Opened);
    assert_eq!(seller.bond_status(), "-");
    assert!(seller.tick(node).await.expect("tick").is_empty());

    // The instance took the buyer's bond for us.
    assert!(
        stage
            .to_seller(&mut seller, &owed)
            .await
            .expect("owed")
            .is_empty()
    );
    assert_eq!(seller.bond_status(), "owed");
    let first = claimed(seller.tick(node).await.expect("tick"), &contract.id());
    let parsed = Bolt11Invoice::from_str(&first).expect("an invoice");
    assert_eq!(
        parsed.amount_milli_satoshis(),
        Some(payout_sat(BOND) * 1_000)
    );
    assert_eq!(parsed.get_payee_pub_key().to_string(), node.id());
    assert_eq!(seller.bond_status(), "claimed");
    // Not paid yet: nothing more is sent, also after a restart.
    let mut seller = SellerRun::from_json(&seller.to_json().expect("json")).expect("stored");
    assert!(seller.tick(node).await.expect("tick").is_empty());
    assert_eq!(seller.bond_status(), "claimed");

    // The instance refuses the claim (the bond is not settled yet): a new one goes out, but
    // only after a while.
    let refusal = Message::cant_do(
        Some(stage.order_id),
        None,
        Some(Payload::CantDo(Some(CantDoReason::NotAllowedByStatus))),
    );
    let refused = stage.to_seller(&mut seller, &refusal).await;
    assert!(matches!(refused, Err(RunError::Refused(_))), "{refused:?}");
    assert_eq!(seller.bond_status(), "owed");
    let mut waited = 0;
    let second = loop {
        let out = seller.tick(node).await.expect("tick");
        if !out.is_empty() {
            break claimed(out, &contract.id());
        }
        waited += 1;
        assert!(waited < 100, "the claim was never made again");
    };
    assert!(waited >= 10, "claimed again after only {waited} looks");
    assert_ne!(second, first);

    // Until it is paid, the buyer's upfront payment stays locked at our node.
    let locked = |world: &Arc<Mutex<World>>| {
        world
            .lock()
            .expect("world")
            .invoices
            .get(&contract.payment_hash)
            .copied()
    };
    assert_eq!(locked(&stage.world), Some(HoldState::Accepted));
    house.pay_invoice(&second, 0, 0).await.expect("the payout");
    assert!(seller.tick(node).await.expect("tick").is_empty());
    assert_eq!(seller.bond_status(), "paid");
    // Paid the bond: the upfront payment goes back, and the lease is over.
    assert_eq!(locked(&stage.world), Some(HoldState::Canceled));
    assert_eq!(seller.state(), SellerState::Canceled);
    assert!(seller.finished());
    let again = SellerRun::from_json(&seller.to_json().expect("json")).expect("stored");
    assert_eq!(again.bond_status(), "paid");
}

#[tokio::test]
async fn a_seller_takes_no_word_that_it_is_owed_a_bond_it_never_asked_for() {
    let owed = |stage: &Stage| {
        Message::new_order(
            Some(stage.order_id),
            None,
            None,
            Action::LeaseBondClaim,
            None,
        )
    };
    // An offer without a bond.
    let stage = Stage::new();
    let (mut seller, _) = stage.opened().await;
    let refused = stage.to_seller(&mut seller, &owed(&stage)).await;
    assert!(
        matches!(refused, Err(RunError::OutOfOrder(_))),
        "{refused:?}"
    );
    assert_eq!(seller.bond_status(), "-");
    assert!(
        seller
            .tick(&stage.seller_node)
            .await
            .expect("tick")
            .is_empty()
    );

    // An offer with a bond, before any channel was opened.
    let (stage, _) = bonded_stage();
    let mut seller = stage.offered().await;
    let refused = stage.to_seller(&mut seller, &owed(&stage)).await;
    assert!(
        matches!(refused, Err(RunError::OutOfOrder(_))),
        "{refused:?}"
    );
    assert_eq!(seller.bond_status(), "-");

    // And not from anyone but the instance.
    let (stage, _) = bonded_stage();
    let (mut seller, _) = stage.opened().await;
    let (incoming, _) = Stage::sent(&owed(&stage), &stage.instance, &stage.seller.trade);
    let forged = seller
        .on_message(
            incoming,
            &Keys::generate().public_key(),
            &stage.seller_node,
            NOW,
        )
        .await;
    assert!(matches!(forged, Err(RunError::Sender)), "{forged:?}");
    assert_eq!(seller.bond_status(), "-");
}
