//! One member's part in a ring, driven by messages ([`RingRun`]).
//!
//! It takes the messages [`crate::ring_wire::open`] read, plus a periodic [`RingRun::tick`] to
//! look at its own node, and answers with the messages to send. There is no relay and no clock
//! in here: the caller delivers, sends, saves after every call, and passes the time.
//!
//! What this member does not take on the instance's word:
//!
//! - **The roster.** Every member's node binding is checked; our own node must be in it, bound to
//!   our own key; and it must be the ring we created or joined: same capacity, announce flag,
//!   window and size.
//! - **That everyone committed.** No channel is opened before there is one valid commit per
//!   member, by the key that member's node vouches for.
//! - **The channel opened to us.** The previous member's report only says where to look. We
//!   confirm it when our own node shows a channel at that outpoint that the other side opened,
//!   with the ring's capacity and announce flag, confirmed and active, and that was not there
//!   before the roster.
//!
//! What it does on the instance's word: wait. An instance that withholds messages stalls a ring;
//! the window then ends it.
//!
//! A run that stops after opening its channel and before it is saved finds that channel on its
//! node again (it was not there when the roster was checked) and reports it instead of opening
//! another. Not here yet: sending again what a relay did not take.

use bipnip_ln::{LightningBackend, OpenRequest};
use mostro_core::prelude::{CantDoReason, Message};
use mostro_core::ring::{Ring, RingTerms};
use mostro_core::ring_message::{RingOffer, RingRoster, RingSeat};
use nostr_sdk::prelude::{Keys, PublicKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::lease::store::StoreError;
use crate::ring_wire::{self, RingIncoming, RingWireError};

/// Why a message or a tick did nothing.
#[derive(Debug)]
pub enum RingRunError {
    /// The message does not come from the instance the ring runs on.
    Sender,
    /// The message is about another order.
    OtherOrder,
    /// The message does not fit where the ring stands.
    OutOfOrder(&'static str),
    /// The message failed its checks: a field, the roster, a binding or a commit.
    Wire(RingWireError),
    /// Well-formed and consistent, but not the ring we are in; the reason says how.
    NotOurRing(&'static str),
    /// Our node could not be asked, or refused.
    Node(String),
    /// The instance refused what we sent.
    Refused(Option<CantDoReason>),
}

impl std::fmt::Display for RingRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RingRunError::Sender => write!(f, "the message does not come from the instance"),
            RingRunError::OtherOrder => write!(f, "the message is about another order"),
            RingRunError::OutOfOrder(what) => write!(f, "out of order: {what}"),
            RingRunError::Wire(e) => write!(f, "{e}"),
            RingRunError::NotOurRing(what) => write!(f, "not our ring: {what}"),
            RingRunError::Node(e) => write!(f, "node: {e}"),
            RingRunError::Refused(Some(reason)) => write!(f, "the instance refused: {reason:?}"),
            RingRunError::Refused(None) => write!(f, "the instance refused"),
        }
    }
}

impl std::error::Error for RingRunError {}

impl From<RingWireError> for RingRunError {
    fn from(e: RingWireError) -> Self {
        RingRunError::Wire(e)
    }
}

impl From<mostro_core::ring_message::RingMessageError> for RingRunError {
    fn from(e: mostro_core::ring_message::RingMessageError) -> Self {
        RingRunError::Wire(RingWireError::Invalid(e))
    }
}

impl From<bipnip_ln::BackendError> for RingRunError {
    fn from(e: bipnip_ln::BackendError) -> Self {
        RingRunError::Node(e.to_string())
    }
}

/// Where a member's part in a ring stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RingState {
    /// Our ring is sent; the instance has not answered.
    Offering,
    /// Our take is sent; the instance has not answered.
    Joining,
    /// We hold a slot; the ring is filling.
    Seated,
    /// The roster passed our checks and our commit is sent.
    Committed,
    /// Every member committed: channels are being opened and confirmed.
    Open,
    /// The ring ended with every channel confirmed.
    Success,
    /// The ring ended with some channels missing.
    Partial,
    /// Over before the roster was fixed.
    Canceled,
}

/// A ring to join: what the order said, and our seat.
#[derive(Debug, Clone)]
pub struct JoinRequest {
    /// The order to join.
    pub order_id: Uuid,
    /// Our request id for the take.
    pub request_id: u64,
    /// Which of the wallet's lease trade keys this run talks under; stored with the run.
    pub trade_index: u32,
    /// The terms the order showed. The roster must have exactly these.
    pub terms: RingTerms,
    /// The size the order showed. The roster must have exactly this many members.
    pub slots: u8,
    /// Our seat: node, binding, address.
    pub seat: RingSeat,
    /// Fee rate of our funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
}

/// One member's part in one ring.
#[derive(Debug)]
pub struct RingRun {
    instance: PublicKey,
    request_id: u64,
    trade_index: u32,
    feerate_sat_vb: u64,
    terms: RingTerms,
    slots: u8,
    /// Our own offer, when we created the ring.
    offer: Option<RingOffer>,
    seat: RingSeat,
    order_id: Option<Uuid>,
    state: RingState,
    roster: Option<RingRoster>,
    /// Channels our node had with the next member when the roster was checked.
    out_before: Vec<String>,
    out_outpoint: Option<String>,
    out_confirmed: bool,
    /// Channels our node had with the previous member when the roster was checked.
    in_before: Vec<String>,
    in_outpoint: Option<String>,
    in_confirmed: bool,
    /// Positions the closing report named as confirmed.
    verified: Vec<u8>,
    waiting_on: Option<String>,
}

/// A run as stored. Nothing in it is secret.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RingRecord {
    version: u8,
    instance: String,
    request_id: u64,
    trade_index: u32,
    feerate_sat_vb: u64,
    network: String,
    capacity_sat: u64,
    announce: bool,
    open_window_secs: u64,
    slots: u8,
    offer: Option<RingOffer>,
    seat: RingSeat,
    order_id: Option<String>,
    state: RingState,
    roster: Option<RingRoster>,
    out_before: Vec<String>,
    out_outpoint: Option<String>,
    out_confirmed: bool,
    in_before: Vec<String>,
    in_outpoint: Option<String>,
    in_confirmed: bool,
    verified: Vec<u8>,
}

const RECORD_VERSION: u8 = 1;

fn corrupt(what: &str) -> StoreError {
    StoreError::Corrupt(what.to_string())
}

/// The funding outpoints of the channels our node has with `node`.
async fn outpoints(
    backend: &dyn LightningBackend,
    node: &secp256k1::PublicKey,
) -> Result<Vec<String>, RingRunError> {
    Ok(backend
        .channels_with(&node.to_string())
        .await?
        .into_iter()
        .map(|channel| channel.funding_outpoint)
        .collect())
}

impl RingRun {
    fn new(
        instance: PublicKey,
        request_id: u64,
        trade_index: u32,
        feerate_sat_vb: u64,
        terms: RingTerms,
        slots: u8,
        seat: RingSeat,
    ) -> Self {
        RingRun {
            instance,
            request_id,
            trade_index,
            feerate_sat_vb,
            terms,
            slots,
            offer: None,
            seat,
            order_id: None,
            state: RingState::Offering,
            roster: None,
            out_before: Vec::new(),
            out_outpoint: None,
            out_confirmed: false,
            in_before: Vec::new(),
            in_outpoint: None,
            in_confirmed: false,
            verified: Vec::new(),
            waiting_on: None,
        }
    }

    /// Starts by proposing `offer` on `instance`; we take its first slot. Returns the run and
    /// the offer message to send.
    pub fn create(
        instance: PublicKey,
        offer: RingOffer,
        request_id: u64,
        trade_index: u32,
        feerate_sat_vb: u64,
    ) -> (Self, Message) {
        let mut run = RingRun::new(
            instance,
            request_id,
            trade_index,
            feerate_sat_vb,
            offer.terms(),
            offer.slots,
            offer.seat.clone(),
        );
        let message = ring_wire::offer(request_id, offer.clone());
        run.offer = Some(offer);
        (run, message)
    }

    /// Starts by taking a slot of a ring on `instance`. Returns the run and the take message to
    /// send.
    pub fn join(instance: PublicKey, request: JoinRequest) -> (Self, Message) {
        let message = ring_wire::take(request.order_id, request.request_id, request.seat.clone());
        let mut run = RingRun::new(
            instance,
            request.request_id,
            request.trade_index,
            request.feerate_sat_vb,
            request.terms,
            request.slots,
            request.seat,
        );
        run.order_id = Some(request.order_id);
        run.state = RingState::Joining;
        (run, message)
    }

    /// Where the run stands.
    pub fn state(&self) -> RingState {
        self.state
    }

    /// True when nothing more will happen in this run.
    pub fn finished(&self) -> bool {
        matches!(
            self.state,
            RingState::Success | RingState::Partial | RingState::Canceled
        )
    }

    /// The order id, once known.
    pub fn order_id(&self) -> Option<Uuid> {
        self.order_id
    }

    /// The instance the run is on.
    pub fn instance(&self) -> PublicKey {
        self.instance
    }

    /// Which of the wallet's lease trade keys this run talks under.
    pub fn trade_index(&self) -> u32 {
        self.trade_index
    }

    /// The ring, once its roster passed our checks.
    pub fn ring(&self) -> Option<Ring> {
        self.roster
            .as_ref()
            .and_then(|roster| roster.validate().ok())
    }

    /// The channel we opened to the next member, once opened.
    pub fn opened_outpoint(&self) -> Option<&str> {
        self.out_outpoint.as_deref()
    }

    /// True once the next member confirmed the channel we opened.
    pub fn our_channel_confirmed(&self) -> bool {
        self.out_confirmed
    }

    /// True once our node saw the channel opened to us and we confirmed it.
    pub fn incoming_confirmed(&self) -> bool {
        self.in_confirmed
    }

    /// The positions the instance's closing report named as confirmed.
    pub fn reported_confirmed(&self) -> &[u8] {
        &self.verified
    }

    /// Why the channel reported to us has not passed our node's check yet, if it has not.
    pub fn waiting_on(&self) -> Option<&str> {
        self.waiting_on.as_deref()
    }

    /// A stable name for this run's stored state: the instance, the request id and our node.
    pub fn store_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let text = format!(
            "{}:{}:{}",
            self.instance.to_hex(),
            self.request_id,
            self.seat.node
        );
        let mut key = bipnip_ln::hexutil::encode(&Sha256::digest(text.as_bytes()));
        key.truncate(32);
        key
    }

    /// The run as text, to store. Nothing in it is secret.
    pub fn to_json(&self) -> Result<String, StoreError> {
        let record = RingRecord {
            version: RECORD_VERSION,
            instance: self.instance.to_hex(),
            request_id: self.request_id,
            trade_index: self.trade_index,
            feerate_sat_vb: self.feerate_sat_vb,
            network: self.terms.network.as_str().to_string(),
            capacity_sat: self.terms.capacity_sat,
            announce: self.terms.announce,
            open_window_secs: self.terms.open_window_secs,
            slots: self.slots,
            offer: self.offer.clone(),
            seat: self.seat.clone(),
            order_id: self.order_id.map(|id| id.to_string()),
            state: self.state,
            roster: self.roster.clone(),
            out_before: self.out_before.clone(),
            out_outpoint: self.out_outpoint.clone(),
            out_confirmed: self.out_confirmed,
            in_before: self.in_before.clone(),
            in_outpoint: self.in_outpoint.clone(),
            in_confirmed: self.in_confirmed,
            verified: self.verified.clone(),
        };
        serde_json::to_string(&record).map_err(|e| StoreError::Corrupt(e.to_string()))
    }

    /// Reads a stored run, refusing one whose parts do not fit together.
    pub fn from_json(json: &str) -> Result<Self, StoreError> {
        let record: RingRecord =
            serde_json::from_str(json).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        if record.version != RECORD_VERSION {
            return Err(corrupt("an unknown record version"));
        }
        let terms = RingTerms {
            network: record.network.parse().map_err(|_| corrupt("the network"))?,
            capacity_sat: record.capacity_sat,
            announce: record.announce,
            open_window_secs: record.open_window_secs,
        };
        terms.validate().map_err(|_| corrupt("the terms"))?;
        record.seat.validate().map_err(|_| corrupt("the seat"))?;
        let order_id = record
            .order_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| corrupt("the order id"))?;
        if let Some(roster) = &record.roster {
            roster.validate().map_err(|_| corrupt("the roster"))?;
        }
        let state = record.state;
        let needs_order = state != RingState::Offering && state != RingState::Canceled;
        let needs_roster = matches!(
            state,
            RingState::Committed | RingState::Open | RingState::Success | RingState::Partial
        );
        if (needs_order && order_id.is_none()) || (needs_roster && record.roster.is_none()) {
            return Err(corrupt("the state does not fit the rest of the record"));
        }
        Ok(RingRun {
            instance: PublicKey::parse(&record.instance).map_err(|_| corrupt("a public key"))?,
            request_id: record.request_id,
            trade_index: record.trade_index,
            feerate_sat_vb: record.feerate_sat_vb,
            terms,
            slots: record.slots,
            offer: record.offer,
            seat: record.seat,
            order_id,
            state,
            roster: record.roster,
            out_before: record.out_before,
            out_outpoint: record.out_outpoint,
            out_confirmed: record.out_confirmed,
            in_before: record.in_before,
            in_outpoint: record.in_outpoint,
            in_confirmed: record.in_confirmed,
            verified: record.verified,
            waiting_on: None,
        })
    }

    /// The stored roster, checked again, with the Nostr key each member's node vouches for.
    fn checked_roster(&self, now: u64) -> Result<(Ring, Vec<PublicKey>), RingRunError> {
        let roster = self
            .roster
            .as_ref()
            .ok_or(RingRunError::OutOfOrder("there is no roster yet"))?;
        Ok(roster.check(now)?)
    }

    /// Handles one message that `sender` authored, at `now` (unix seconds). `identity` is our
    /// liquidity identity, the key our node is bound to; the commit is signed with it.
    pub async fn on_message(
        &mut self,
        incoming: RingIncoming,
        sender: &PublicKey,
        identity: &Keys,
        backend: &dyn LightningBackend,
        now: u64,
    ) -> Result<Vec<Message>, RingRunError> {
        if !ring_wire::from_instance(sender, &self.instance) {
            return Err(RingRunError::Sender);
        }
        match &incoming {
            RingIncoming::Offered {
                order_id,
                request_id,
                offer,
            } => {
                if *request_id != Some(self.request_id) || self.offer.as_ref() != Some(offer) {
                    return Err(RingRunError::OtherOrder);
                }
                if self.state != RingState::Offering {
                    return Err(RingRunError::OutOfOrder("the offer was already answered"));
                }
                self.order_id = Some(*order_id);
                self.state = RingState::Seated;
                return Ok(Vec::new());
            }
            RingIncoming::Refused {
                order_id,
                request_id,
                reason,
            } => {
                let ours = match (order_id, self.order_id) {
                    (Some(theirs), Some(ours)) => *theirs == ours,
                    _ => *request_id == Some(self.request_id),
                };
                if !ours {
                    return Err(RingRunError::OtherOrder);
                }
                if matches!(self.state, RingState::Offering | RingState::Joining) {
                    self.state = RingState::Canceled;
                }
                return Err(RingRunError::Refused(reason.clone()));
            }
            _ => {}
        }
        let order_id = self
            .order_id
            .ok_or(RingRunError::OutOfOrder("the ring has no order id yet"))?;
        if incoming.order_id() != Some(order_id) {
            return Err(RingRunError::OtherOrder);
        }

        match (incoming, self.state) {
            (RingIncoming::SeatTaken { seat, .. }, RingState::Joining) => {
                if seat != self.seat {
                    return Err(RingRunError::NotOurRing(
                        "the seat is not the one we asked for",
                    ));
                }
                self.state = RingState::Seated;
                Ok(Vec::new())
            }
            (RingIncoming::Roster { roster, .. }, RingState::Seated) => {
                let (ring, keys) = roster.check(now)?;
                let own = self.seat.node_key()?;
                let position = ring
                    .position(&own)
                    .ok_or(RingRunError::NotOurRing("our node is not in the roster"))?;
                if keys[position] != identity.public_key() {
                    return Err(RingRunError::NotOurRing(
                        "our seat is bound to someone else's key",
                    ));
                }
                match &self.offer {
                    Some(offer) => offer.check_ring(&ring)?,
                    None => {
                        if ring.terms != self.terms || ring.members.len() != usize::from(self.slots)
                        {
                            return Err(RingRunError::NotOurRing(
                                "the roster is not the ring we joined",
                            ));
                        }
                    }
                }
                let (Some(next), Some(previous)) = (ring.opens_to(&own), ring.receives_from(&own))
                else {
                    return Err(RingRunError::NotOurRing("our node is not in the roster"));
                };
                // What our node has with its two neighbours now is not part of this ring.
                self.out_before = outpoints(backend, &next).await?;
                self.in_before = outpoints(backend, &previous).await?;
                self.roster = Some(roster);
                self.state = RingState::Committed;
                Ok(vec![ring_wire::commit(order_id, &ring, identity)])
            }
            (RingIncoming::Open { open, .. }, RingState::Committed) => {
                let (ring, keys) = self.checked_roster(now)?;
                open.check(&ring, &keys)?;
                self.state = RingState::Open;
                Ok(Vec::new())
            }
            (RingIncoming::EdgeOpened { edge, .. }, RingState::Open) => {
                let (ring, _) = self.checked_roster(now)?;
                if edge.ring_id != ring.id() {
                    return Err(RingRunError::NotOurRing("another ring id"));
                }
                let outpoint = edge.channel_point();
                if self.in_before.contains(&outpoint) {
                    return Err(RingRunError::NotOurRing(
                        "the channel existed before the ring",
                    ));
                }
                match &self.in_outpoint {
                    Some(known) if *known != outpoint => {
                        return Err(RingRunError::OutOfOrder(
                            "another channel was already reported to us",
                        ));
                    }
                    _ => self.in_outpoint = Some(outpoint),
                }
                Ok(Vec::new())
            }
            (RingIncoming::EdgeVerified { edge, .. }, RingState::Open) => {
                let (ring, _) = self.checked_roster(now)?;
                if edge.ring_id != ring.id() {
                    return Err(RingRunError::NotOurRing("another ring id"));
                }
                if self.out_outpoint.as_deref() != Some(edge.channel_point().as_str()) {
                    return Err(RingRunError::NotOurRing(
                        "that is not the channel we opened",
                    ));
                }
                self.out_confirmed = true;
                Ok(Vec::new())
            }
            (RingIncoming::Closed { closed, .. }, RingState::Committed | RingState::Open) => {
                let (ring, _) = self.checked_roster(now)?;
                closed.check(&ring)?;
                self.state = if closed.is_complete(&ring) {
                    RingState::Success
                } else {
                    RingState::Partial
                };
                self.verified = closed.verified;
                Ok(Vec::new())
            }
            (
                RingIncoming::Canceled { .. },
                RingState::Offering | RingState::Joining | RingState::Seated,
            ) => {
                self.state = RingState::Canceled;
                Ok(Vec::new())
            }
            _ => Err(RingRunError::OutOfOrder(
                "the message does not fit this ring",
            )),
        }
    }

    /// Looks at our node, at `now`. While the ring is open and its window has not ended, opens
    /// our channel to the next member once and reports it; and confirms the channel reported to
    /// us once our node shows it as the ring describes.
    pub async fn tick(
        &mut self,
        backend: &dyn LightningBackend,
        now: u64,
    ) -> Result<Vec<Message>, RingRunError> {
        if self.state != RingState::Open {
            return Ok(Vec::new());
        }
        let (Some(order_id), Some(roster)) = (self.order_id, self.roster.clone()) else {
            return Ok(Vec::new());
        };
        let ring = roster.validate()?;
        let own = self.seat.node_key()?;
        let (Some(next), Some(previous)) = (ring.opens_to(&own), ring.receives_from(&own)) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();

        if self.out_outpoint.is_none() && now < ring.window_ends_at() {
            let capacity = ring.terms.capacity_sat;
            // A run that stopped after opening and before it was saved finds its channel here.
            let already = backend
                .channels_with(&next.to_string())
                .await?
                .into_iter()
                .find(|channel| {
                    channel.local_initiator
                        && channel.capacity_sat == capacity
                        && !self.out_before.contains(&channel.funding_outpoint)
                })
                .map(|channel| channel.funding_outpoint);
            let outpoint = match already {
                Some(outpoint) => outpoint,
                None => {
                    let address = ring
                        .position(&next)
                        .and_then(|position| roster.seats.get(position))
                        .and_then(|seat| seat.node_address.clone());
                    backend
                        .open_channel(&OpenRequest {
                            remote_pubkey: next.to_string(),
                            remote_addr: address,
                            capacity_sat: capacity,
                            announce: ring.terms.announce,
                            feerate_sat_vb: self.feerate_sat_vb,
                        })
                        .await?
                }
            };
            self.out_outpoint = Some(outpoint.clone());
            out.push(ring_wire::edge_opened(order_id, &ring, &outpoint)?);
        }

        if let (Some(outpoint), false) = (self.in_outpoint.clone(), self.in_confirmed) {
            let channels = backend.channels_with(&previous.to_string()).await?;
            let problem = match channels.iter().find(|c| c.funding_outpoint == outpoint) {
                None => Some("our node has no channel at that outpoint"),
                Some(c) if c.local_initiator => Some("our node opened that channel"),
                Some(c) if c.capacity_sat != ring.terms.capacity_sat => {
                    Some("its capacity is not the ring's")
                }
                Some(c) if c.announced != ring.terms.announce => {
                    Some("its announce flag is not the ring's")
                }
                Some(c) if c.confirmations == 0 => Some("it is not confirmed yet"),
                Some(c) if !c.active => Some("it is not active yet"),
                Some(_) => None,
            };
            match problem {
                None => {
                    self.in_confirmed = true;
                    self.waiting_on = None;
                    out.push(ring_wire::edge_verified(order_id, &ring, &outpoint)?);
                }
                Some(why) => self.waiting_on = Some(why.to_string()),
            }
        }
        Ok(out)
    }

    /// While the ring is still filling: withdraws it if we created it, or gives our slot back.
    pub fn cancel(&mut self, request_id: u64) -> Result<Vec<Message>, RingRunError> {
        match (self.state, self.order_id) {
            (RingState::Seated, Some(order_id)) => {
                self.state = RingState::Canceled;
                Ok(vec![ring_wire::cancel(order_id, request_id)])
            }
            _ => Err(RingRunError::OutOfOrder("nothing to cancel at this point")),
        }
    }
}
