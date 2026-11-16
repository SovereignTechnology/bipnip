//! The lease runs, seller and buyer, on the two-node simulator. Messages travel sealed, the way
//! an instance and a peer would send them; the test plays the instance.

use std::sync::{Arc, Mutex};

use bipnip_ln::{HoldState, LightningBackend, OpenRequest, PaymentState};
use mostro_core::lease_message::{LeaseOffer, LeasePayload};
use mostro_core::node_binding::{
    Action as BindingAction, Network, SignedStatement, Statement, sign_ln_message,
};
use mostro_core::prelude::{Action, CantDoReason, Message, Payload, Peer};
use nostr_sdk::prelude::{FinalizeEvent, Keys, PublicKey};
use uuid::Uuid;

use super::{NOW, Node, World, confirm, pair};
use crate::lease::run::{
    BuyerConfig, BuyerRun, BuyerState, Outgoing, RunError, SellerConfig, SellerRun, SellerState,
    TakeRequest,
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
