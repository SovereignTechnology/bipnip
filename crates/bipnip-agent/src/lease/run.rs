//! One lease driven by messages: the seller's side ([`SellerRun`]) and the buyer's ([`BuyerRun`]).
//!
//! Each takes the messages [`crate::lease_wire::open`] read, plus a periodic [`SellerRun::tick`]
//! or [`BuyerRun::tick`] to look at its own node, and answers with the messages to send. There is
//! no relay and no clock in here: the caller delivers, sends and passes the time.
//!
//! What an instance can and cannot do to a lease run by these two:
//!
//! - It cannot move a step forward: every message is checked for its sender
//!   ([`crate::lease_wire::sender_ok`]), the seller checks the take itself, the buyer's node checks
//!   the invoice and the channel.
//! - Nor can a seller be paid for a channel it opened earlier: the buyer notes the channels its
//!   node has with the seller when it takes the order, and refuses a report that names one.
//! - It cannot end a lease in which money is locked. A `canceled` from the instance stops a lease
//!   before the buyer has paid. After that, each side asks its own node: the buyer goes on while
//!   its payment is still locked, and the seller, once it has opened the channel, keeps waiting
//!   for the preimage. Otherwise an instance could cost a seller the channel it opened, by telling
//!   it to give the payment back.
//!
//! Both runs can be written out and read back ([`SellerRun::to_json`], [`BuyerRun::to_json`],
//! [`super::store::LeaseStore`]). The caller saves after every message and every tick, and saves
//! a buyer's run before sending its take. The seller opens the channel in two ticks for that
//! reason: the first records that the payment is locked and which channels exist, the caller
//! saves, and only the second opens. A seller that restarts in between looks for a channel it
//! already opened before opening another.
//!
//! Two steps touch the node before the run can be saved, and are safe to repeat after a restart:
//! a buyer whose payment is already locked does not pay again, and a seller whose hold invoice is
//! already settled does not settle again. One is not: a seller that stopped after creating the
//! hold invoice and before saving cannot create it again, so that lease stalls until the
//! instance times it out. No money is at stake there, since the buyer never saw an invoice.
//!
//! An answer no relay took is not lost: each run can make the message its state implies again
//! ([`SellerRun::pending`], [`BuyerRun::pending`]), and remembers on disk that it still owes it.
//!
//! The end of a lease is judged the same way on both sides: by how many confirmations the
//! channel has on the party's own node. Once that reaches the lease's length the seller bills the
//! completion share, and the buyer pays it only if its node still has the channel. A channel
//! that is gone ends the lease with the completion share unpaid, whoever closed it.

use std::path::Path;

use bipnip_ln::{HoldState, LightningBackend, PaymentState, hexutil};
use mostro_core::lease::{LeaseContract, LeaseTerms};
use mostro_core::lease_message::{LeaseInvoice, LeaseOffer};
use mostro_core::node_binding::Network;
use mostro_core::prelude::{CantDoReason, Message};
use nostr_sdk::prelude::{Event, Keys, PublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::store::StoreError;
use super::{BuyerLease, LeaseStepError, SellerLease};
use crate::lease_wire::{self, Incoming, WireError};

/// How long the buyer's node may take over the completion payment before the run looks again.
const COMPLETION_PAY_WAIT_SECS: u32 = 30;

/// A message to send.
#[derive(Debug, Clone)]
pub enum Outgoing {
    /// To the instance the lease runs on.
    ToInstance(Message),
    /// To the other party's trade key, peer to peer. Only the preimage travels this way.
    ToPeer {
        /// The other party's trade key.
        to: PublicKey,
        /// The message.
        message: Message,
    },
}

impl Outgoing {
    /// Wraps the message for its receiver: for the instance with proof of `identity` and `pow`
    /// bits of work, for the peer from the trade key alone.
    pub fn seal(
        &self,
        identity: &Keys,
        trade: &Keys,
        instance: PublicKey,
        pow: u8,
    ) -> Result<Event, WireError> {
        match self {
            Outgoing::ToInstance(message) => {
                lease_wire::seal_for_instance(message, identity, trade, instance, pow)
            }
            Outgoing::ToPeer { to, message } => lease_wire::seal_for_peer(message, trade, *to),
        }
    }
}

/// Why a message or a tick did nothing.
#[derive(Debug)]
pub enum RunError {
    /// The key that sent the message may not send it.
    Sender,
    /// The message is about another order or another lease.
    OtherOrder,
    /// The message does not fit where the lease stands.
    OutOfOrder(&'static str),
    /// The message failed its checks.
    Wire(WireError),
    /// The lease step was refused, by our own checks or by our node.
    Step(LeaseStepError),
    /// The instance refused what we sent.
    Refused(Option<CantDoReason>),
    /// Our node could not pay the upfront invoice; the reason is the node's.
    PaymentFailed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Sender => write!(f, "the sender may not send this message"),
            RunError::OtherOrder => write!(f, "the message is about another order"),
            RunError::OutOfOrder(what) => write!(f, "out of order: {what}"),
            RunError::Wire(e) => write!(f, "{e}"),
            RunError::Step(e) => write!(f, "{e}"),
            RunError::Refused(Some(reason)) => write!(f, "the instance refused: {reason:?}"),
            RunError::Refused(None) => write!(f, "the instance refused"),
            RunError::PaymentFailed(reason) => write!(f, "the upfront payment failed: {reason}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<WireError> for RunError {
    fn from(e: WireError) -> Self {
        RunError::Wire(e)
    }
}

impl From<LeaseStepError> for RunError {
    fn from(e: LeaseStepError) -> Self {
        RunError::Step(e)
    }
}

/// Stores the other party's trade key, or checks it has not changed.
fn learn_peer(known: &mut Option<PublicKey>, trade_key: PublicKey) -> Result<(), RunError> {
    match known {
        None => {
            *known = Some(trade_key);
            Ok(())
        }
        Some(key) if *key == trade_key => Ok(()),
        Some(_) => Err(RunError::OutOfOrder("the peer's trade key changed")),
    }
}

/// Where the seller's side stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SellerState {
    /// The offer is sent; the instance has not answered.
    Offering,
    /// The offer is open.
    Open,
    /// A buyer took it and has our hold invoice.
    Invoiced,
    /// The buyer's payment is locked; the channel is about to be opened, or was opened and the
    /// run stopped before recording it.
    Opening,
    /// The buyer's payment is locked and the channel is opening; waiting for the preimage.
    Opened,
    /// The hold invoice is settled. The end of the lease when all of the fee was upfront.
    Active,
    /// The lease has run its length and the completion invoice is out.
    Completing,
    /// The completion share is paid.
    Complete,
    /// The channel closed before the completion share could be billed.
    Closed,
    /// Over before it was active.
    Canceled,
}

/// The seller's choices for a lease.
#[derive(Debug, Clone, Copy)]
pub struct SellerConfig {
    /// `min_final_cltv_expiry_delta` of the hold invoice.
    pub cltv_delta: u32,
    /// Fee rate of the funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
}

/// The seller's side of one offer and the lease taken from it.
#[derive(Debug)]
pub struct SellerRun {
    instance: PublicKey,
    offer: LeaseOffer,
    request_id: u64,
    trade_index: u32,
    config: SellerConfig,
    state: SellerState,
    order_id: Option<Uuid>,
    buyer_trade: Option<PublicKey>,
    buyer_addr: Option<String>,
    lease: Option<SellerLease>,
    /// Funding outpoints of the channels our node had with the buyer before we opened one.
    channels_before: Vec<String>,
    outpoint: Option<String>,
    canceled_by_instance: bool,
    /// The upfront hold invoice, kept so it can be sent again.
    invoice: Option<String>,
    /// The completion invoice, once the lease has run its length.
    completion_invoice: Option<String>,
    /// True when the answer the current state implies has not reached a relay.
    unsent: bool,
}

/// A seller's run as stored. Nothing in it is secret.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SellerRecord {
    version: u8,
    instance: String,
    offer: LeaseOffer,
    request_id: u64,
    trade_index: u32,
    cltv_delta: u32,
    feerate_sat_vb: u64,
    state: SellerState,
    order_id: Option<String>,
    buyer_trade: Option<String>,
    buyer_addr: Option<String>,
    contract: Option<String>,
    channels_before: Vec<String>,
    outpoint: Option<String>,
    canceled_by_instance: bool,
    invoice: Option<String>,
    completion_invoice: Option<String>,
    unsent: bool,
}

/// The version of the stored records.
const RECORD_VERSION: u8 = 1;

fn corrupt(what: &str) -> StoreError {
    StoreError::Corrupt(what.to_string())
}

fn stored_key(hex: &str) -> Result<PublicKey, StoreError> {
    PublicKey::parse(hex).map_err(|_| corrupt("a public key"))
}

fn stored_json<T: Serialize>(record: &T) -> Result<String, StoreError> {
    serde_json::to_string(record).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn parsed_json<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|e| StoreError::Corrupt(e.to_string()))
}

impl SellerRun {
    /// Starts by offering `offer` on `instance`. Returns the run and the offer message to send.
    /// `trade_index` says which of the wallet's lease trade keys this run talks under; it is
    /// stored with the run so a restart finds the key again.
    pub fn offer(
        instance: PublicKey,
        offer: LeaseOffer,
        request_id: u64,
        trade_index: u32,
        config: SellerConfig,
    ) -> (Self, Outgoing) {
        let message = lease_wire::offer(request_id, offer.clone());
        let run = SellerRun {
            instance,
            offer,
            request_id,
            trade_index,
            config,
            state: SellerState::Offering,
            order_id: None,
            buyer_trade: None,
            buyer_addr: None,
            lease: None,
            channels_before: Vec::new(),
            outpoint: None,
            canceled_by_instance: false,
            invoice: None,
            completion_invoice: None,
            unsent: false,
        };
        (run, Outgoing::ToInstance(message))
    }

    /// A stable name for this run's stored state: the same offer, sent to the same instance
    /// under the same request id, has the same key (32 hex).
    pub fn store_key(&self) -> String {
        let offer = serde_json::to_string(&self.offer).unwrap_or_default();
        let text = format!("{}:{}:{offer}", self.instance.to_hex(), self.request_id);
        let mut key = hexutil::encode(&Sha256::digest(text.as_bytes()));
        key.truncate(32);
        key
    }

    /// The run as text, to store. Nothing in it is secret.
    pub fn to_json(&self) -> Result<String, StoreError> {
        stored_json(&SellerRecord {
            version: RECORD_VERSION,
            instance: self.instance.to_hex(),
            offer: self.offer.clone(),
            request_id: self.request_id,
            trade_index: self.trade_index,
            cltv_delta: self.config.cltv_delta,
            feerate_sat_vb: self.config.feerate_sat_vb,
            state: self.state,
            order_id: self.order_id.map(|id| id.to_string()),
            buyer_trade: self.buyer_trade.map(|key| key.to_hex()),
            buyer_addr: self.buyer_addr.clone(),
            contract: self.contract().map(LeaseContract::text),
            channels_before: self.channels_before.clone(),
            outpoint: self.outpoint.clone(),
            canceled_by_instance: self.canceled_by_instance,
            invoice: self.invoice.clone(),
            completion_invoice: self.completion_invoice.clone(),
            unsent: self.unsent,
        })
    }

    /// Reads a stored run, refusing one whose parts do not fit together: a state that needs an
    /// order id, a contract or an outpoint it does not have, or a contract that is not the
    /// offer's.
    pub fn from_json(json: &str) -> Result<Self, StoreError> {
        let record: SellerRecord = parsed_json(json)?;
        if record.version != RECORD_VERSION {
            return Err(corrupt("an unknown record version"));
        }
        record.offer.validate().map_err(|_| corrupt("the offer"))?;
        let order_id = record
            .order_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| corrupt("the order id"))?;
        let lease = record
            .contract
            .as_deref()
            .map(SellerLease::restore)
            .transpose()
            .map_err(|_| corrupt("the contract"))?;
        if let Some(lease) = &lease {
            record
                .offer
                .check_contract(lease.contract())
                .map_err(|_| corrupt("the contract is not the offer's"))?;
        }
        let state = record.state;
        let needs_order = state != SellerState::Offering && state != SellerState::Canceled;
        let after_opening = matches!(
            state,
            SellerState::Opened
                | SellerState::Active
                | SellerState::Completing
                | SellerState::Complete
                | SellerState::Closed
        );
        let needs_lease =
            after_opening || matches!(state, SellerState::Invoiced | SellerState::Opening);
        let needs_outpoint = after_opening;
        if (needs_order && order_id.is_none())
            || (needs_lease && lease.is_none())
            || (needs_outpoint && record.outpoint.is_none())
            || (state == SellerState::Invoiced && record.invoice.is_none())
            || (state == SellerState::Completing && record.completion_invoice.is_none())
        {
            return Err(corrupt("the state does not fit the rest of the record"));
        }
        Ok(SellerRun {
            instance: stored_key(&record.instance)?,
            offer: record.offer,
            request_id: record.request_id,
            trade_index: record.trade_index,
            config: SellerConfig {
                cltv_delta: record.cltv_delta,
                feerate_sat_vb: record.feerate_sat_vb,
            },
            state,
            order_id,
            buyer_trade: record.buyer_trade.as_deref().map(stored_key).transpose()?,
            buyer_addr: record.buyer_addr,
            lease,
            channels_before: record.channels_before,
            outpoint: record.outpoint,
            canceled_by_instance: record.canceled_by_instance,
            invoice: record.invoice,
            completion_invoice: record.completion_invoice,
            unsent: record.unsent,
        })
    }

    /// The channel this lease already opened, if our node has one: to the buyer, funded by us,
    /// of the contract's capacity, and not among the channels noted before opening.
    async fn already_opened(
        &self,
        lease: &SellerLease,
        backend: &dyn LightningBackend,
    ) -> Result<Option<String>, RunError> {
        let contract = lease.contract();
        let channels = backend
            .channels_with(&contract.buyer_node.to_string())
            .await
            .map_err(LeaseStepError::from)?;
        Ok(channels
            .into_iter()
            .find(|channel| {
                channel.local_initiator
                    && channel.capacity_sat == contract.terms.capacity_sat
                    && !self.channels_before.contains(&channel.funding_outpoint)
            })
            .map(|channel| channel.funding_outpoint))
    }

    /// Where the run stands.
    pub fn state(&self) -> SellerState {
        self.state
    }

    /// Which of the wallet's lease trade keys this run talks under.
    pub fn trade_index(&self) -> u32 {
        self.trade_index
    }

    /// The instance the run is on.
    pub fn instance(&self) -> PublicKey {
        self.instance
    }

    /// The order id, once the instance has given one.
    pub fn order_id(&self) -> Option<Uuid> {
        self.order_id
    }

    /// The contract, once a buyer's take was accepted.
    pub fn contract(&self) -> Option<&LeaseContract> {
        self.lease.as_ref().map(SellerLease::contract)
    }

    /// The funding outpoint, once the channel is opening.
    pub fn funding_outpoint(&self) -> Option<&str> {
        self.outpoint.as_deref()
    }

    /// True when the instance said `canceled` after the channel was opened. The run keeps
    /// waiting for the preimage regardless.
    pub fn canceled_by_instance(&self) -> bool {
        self.canceled_by_instance
    }

    /// True when the answer the current state implies has not reached a relay yet.
    pub fn unsent(&self) -> bool {
        self.unsent
    }

    /// Records whether the current state's answer still has to be sent. Stored with the run.
    pub fn set_unsent(&mut self, unsent: bool) {
        self.unsent = unsent;
    }

    /// True when nothing more will happen in this run: it was canceled, the completion share
    /// is paid, the channel closed first, or the lease is active and had no completion share.
    pub fn finished(&self) -> bool {
        match self.state {
            SellerState::Canceled | SellerState::Complete | SellerState::Closed => true,
            SellerState::Active => self
                .contract()
                .is_some_and(|contract| contract.terms.completion_sat() == Ok(0)),
            _ => false,
        }
    }

    /// The message the current state implies, made again: the offer while it is unanswered, the
    /// hold invoice once a take was accepted, the channel report once the channel is opening,
    /// the active notice once settled, and the completion invoice once the lease has run. Empty for states that say nothing. The instance
    /// refuses a message it already acted on, so sending one twice changes nothing.
    pub fn pending(&self) -> Result<Vec<Outgoing>, RunError> {
        let message = match (self.state, self.order_id, self.contract()) {
            (SellerState::Offering, _, _) => {
                Some(lease_wire::offer(self.request_id, self.offer.clone()))
            }
            (SellerState::Invoiced, Some(order_id), Some(contract)) => self
                .invoice
                .clone()
                .map(|bolt11| lease_wire::upfront_invoice(order_id, contract, bolt11)),
            (SellerState::Opened, Some(order_id), Some(contract)) => match &self.outpoint {
                Some(outpoint) => Some(lease_wire::channel_opened(order_id, contract, outpoint)?),
                None => None,
            },
            (SellerState::Active, Some(order_id), _) => Some(lease_wire::active(order_id)),
            (SellerState::Completing, Some(order_id), Some(contract)) => self
                .completion_invoice
                .clone()
                .map(|bolt11| lease_wire::completion_invoice(order_id, contract, bolt11)),
            _ => None,
        };
        Ok(message.into_iter().map(Outgoing::ToInstance).collect())
    }

    /// Handles one message that `sender` authored, at `now` (unix seconds).
    pub async fn on_message(
        &mut self,
        incoming: Incoming,
        sender: &PublicKey,
        backend: &dyn LightningBackend,
        now: u64,
    ) -> Result<Vec<Outgoing>, RunError> {
        if !lease_wire::sender_ok(&incoming, sender, &self.instance, self.buyer_trade.as_ref()) {
            return Err(RunError::Sender);
        }
        match &incoming {
            Incoming::Offered {
                order_id,
                request_id,
                offer,
            } => {
                if *request_id != Some(self.request_id) || *offer != self.offer {
                    return Err(RunError::OtherOrder);
                }
                if self.state != SellerState::Offering {
                    return Err(RunError::OutOfOrder("the offer was already answered"));
                }
                self.order_id = Some(*order_id);
                self.state = SellerState::Open;
                return Ok(Vec::new());
            }
            Incoming::Refused {
                order_id,
                request_id,
                reason,
            } => {
                let ours = match (order_id, self.order_id) {
                    (Some(theirs), Some(ours)) => *theirs == ours,
                    _ => *request_id == Some(self.request_id),
                };
                if !ours {
                    return Err(RunError::OtherOrder);
                }
                if self.state == SellerState::Offering {
                    self.state = SellerState::Canceled;
                }
                return Err(RunError::Refused(reason.clone()));
            }
            _ => {}
        }
        let order_id = self
            .order_id
            .ok_or(RunError::OutOfOrder("the offer has no order id yet"))?;
        if incoming.order_id() != Some(order_id) {
            return Err(RunError::OtherOrder);
        }

        match (incoming, self.state) {
            (Incoming::Take { take, .. }, SellerState::Open) => {
                let (contract, _vouched_for) = lease_wire::check_take(&self.offer, &take, now)?;
                let lease = SellerLease::accept(backend, &take.contract).await?;
                let bolt11 = lease
                    .upfront_invoice(backend, self.config.cltv_delta)
                    .await?;
                self.buyer_addr = take.node_address;
                self.lease = Some(lease);
                self.invoice = Some(bolt11.clone());
                self.state = SellerState::Invoiced;
                Ok(vec![Outgoing::ToInstance(lease_wire::upfront_invoice(
                    order_id, &contract, bolt11,
                ))])
            }
            (
                Incoming::PeerKey { trade_key, .. },
                SellerState::Open
                | SellerState::Invoiced
                | SellerState::Opening
                | SellerState::Opened,
            ) => {
                learn_peer(&mut self.buyer_trade, trade_key)?;
                Ok(Vec::new())
            }
            (Incoming::Preimage { preimage, .. }, SellerState::Opened) => {
                let lease = self
                    .lease
                    .as_ref()
                    .ok_or(RunError::OutOfOrder("no lease"))?;
                let secret = lease_wire::check_preimage(lease.contract(), &preimage)?;
                // A run that stopped after settling and before it was saved has nothing left
                // to settle.
                let settled = backend
                    .hold_invoice_state(&lease.contract().payment_hash)
                    .await
                    .map_err(LeaseStepError::from)?
                    == HoldState::Settled;
                if !settled {
                    lease.settle(backend, secret).await?;
                }
                self.state = SellerState::Active;
                Ok(vec![Outgoing::ToInstance(lease_wire::active(order_id))])
            }
            (Incoming::Canceled { .. }, SellerState::Open) => {
                self.state = SellerState::Canceled;
                Ok(Vec::new())
            }
            (Incoming::Canceled { .. }, SellerState::Invoiced) => {
                // No channel yet: give the buyer its payment back at once.
                if let Some(lease) = &self.lease {
                    lease.cancel(backend).await?;
                }
                self.state = SellerState::Canceled;
                Ok(Vec::new())
            }
            (Incoming::Canceled { .. }, SellerState::Opening) => {
                // Between noticing the payment and recording the channel. If our node already
                // has the channel, this is the case below; if not, nothing was opened and the
                // buyer gets its payment back.
                let lease = self.lease.clone().ok_or(RunError::OutOfOrder("no lease"))?;
                match self.already_opened(&lease, backend).await? {
                    Some(outpoint) => {
                        self.outpoint = Some(outpoint);
                        self.state = SellerState::Opened;
                        self.canceled_by_instance = true;
                    }
                    None => {
                        lease.cancel(backend).await?;
                        self.state = SellerState::Canceled;
                    }
                }
                Ok(Vec::new())
            }
            (Incoming::Canceled { .. }, SellerState::Opened) => {
                // The channel is open and the buyer's payment is locked at our node. The buyer
                // can still release the preimage; if it does not, the payment times out. Giving
                // it back on an instance's word would hand over the channel for nothing.
                self.canceled_by_instance = true;
                Ok(Vec::new())
            }
            _ => Err(RunError::OutOfOrder("the message does not fit this lease")),
        }
    }

    /// Looks at our node. Opening the channel takes two ticks, with a save by the caller in
    /// between: the first notices the buyer's locked payment and notes the channels our node has
    /// with the buyer; the second opens the channel (or finds the one a run that stopped mid-way
    /// already opened) and reports it.
    pub async fn tick(
        &mut self,
        backend: &dyn LightningBackend,
    ) -> Result<Vec<Outgoing>, RunError> {
        let (Some(lease), Some(order_id)) = (self.lease.clone(), self.order_id) else {
            return Ok(Vec::new());
        };
        match self.state {
            SellerState::Invoiced => {
                if lease.payment_locked(backend).await? {
                    self.channels_before = backend
                        .channels_with(&lease.contract().buyer_node.to_string())
                        .await
                        .map_err(LeaseStepError::from)?
                        .into_iter()
                        .map(|channel| channel.funding_outpoint)
                        .collect();
                    self.state = SellerState::Opening;
                }
                Ok(Vec::new())
            }
            SellerState::Opening => {
                let outpoint = match self.already_opened(&lease, backend).await? {
                    Some(outpoint) => outpoint,
                    None => {
                        lease
                            .open(backend, self.buyer_addr.clone(), self.config.feerate_sat_vb)
                            .await?
                    }
                };
                self.state = SellerState::Opened;
                self.outpoint = Some(outpoint.clone());
                Ok(vec![Outgoing::ToInstance(lease_wire::channel_opened(
                    order_id,
                    lease.contract(),
                    &outpoint,
                )?)])
            }
            SellerState::Active => self.bill(&lease, order_id, backend).await,
            SellerState::Completing => {
                let Some(bolt11) = &self.completion_invoice else {
                    return Ok(Vec::new());
                };
                let hash = super::check_completion_invoice(bolt11, lease.contract())?;
                let paid = backend
                    .hold_invoice_state(&hash)
                    .await
                    .map_err(LeaseStepError::from)?
                    == HoldState::Settled;
                if paid {
                    self.state = SellerState::Complete;
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Once the leased channel has as many confirmations as the lease is long, bills the
    /// completion share. A channel our node no longer has ends the lease unbilled. Nothing
    /// happens for a lease whose fee was all upfront.
    async fn bill(
        &mut self,
        lease: &SellerLease,
        order_id: Uuid,
        backend: &dyn LightningBackend,
    ) -> Result<Vec<Outgoing>, RunError> {
        let contract = lease.contract();
        if contract
            .terms
            .completion_sat()
            .map_err(LeaseStepError::from)?
            == 0
        {
            return Ok(Vec::new());
        }
        let Some(outpoint) = self.outpoint.clone() else {
            return Ok(Vec::new());
        };
        let channels = backend
            .channels_with(&contract.buyer_node.to_string())
            .await
            .map_err(LeaseStepError::from)?;
        let Some(channel) = channels
            .iter()
            .find(|channel| channel.funding_outpoint == outpoint)
        else {
            self.state = SellerState::Closed;
            return Ok(Vec::new());
        };
        if channel.confirmations < contract.terms.duration_blocks {
            return Ok(Vec::new());
        }
        let bolt11 = lease.completion_invoice(backend).await?;
        self.completion_invoice = Some(bolt11.clone());
        self.state = SellerState::Completing;
        Ok(vec![Outgoing::ToInstance(lease_wire::completion_invoice(
            order_id, contract, bolt11,
        ))])
    }

    /// Withdraws the offer, or backs out of a lease before the channel is opened: the hold
    /// invoice is canceled, so a buyer that already paid has its payment back at once.
    pub async fn cancel(
        &mut self,
        backend: &dyn LightningBackend,
        request_id: u64,
    ) -> Result<Vec<Outgoing>, RunError> {
        let order_id = match (self.state, self.order_id) {
            (SellerState::Open | SellerState::Invoiced, Some(order_id)) => order_id,
            _ => return Err(RunError::OutOfOrder("nothing to cancel at this point")),
        };
        if let Some(lease) = &self.lease {
            lease.cancel(backend).await?;
        }
        self.state = SellerState::Canceled;
        Ok(vec![Outgoing::ToInstance(lease_wire::cancel(
            order_id, request_id,
        ))])
    }
}

/// Where the buyer's side stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuyerState {
    /// The take is sent; waiting for the seller's invoice.
    Taking,
    /// The upfront payment is locked at the seller's node.
    Paid,
    /// The seller reported a channel; our node is checking it.
    Verifying,
    /// The channel passed the check and the preimage was released.
    Released,
    /// The seller settled. The end of the lease when all of the fee was upfront.
    Active,
    /// The completion share is paid.
    Complete,
    /// The channel was gone when the completion share was billed; it is not paid.
    Closed,
    /// Over before the preimage was released.
    Canceled,
}

/// The buyer's limits for a lease.
#[derive(Debug, Clone, Copy)]
pub struct BuyerConfig {
    /// Most routing fee to pay on the upfront payment, in millisatoshi.
    pub max_fee_msat: u64,
    /// Largest `min_final_cltv_expiry_delta` accepted on the seller's invoice.
    pub max_cltv_delta: u32,
}

/// What a buyer takes: an order and the terms it wants from it.
#[derive(Debug, Clone)]
pub struct TakeRequest {
    /// The order to take.
    pub order_id: Uuid,
    /// Our request id for the take.
    pub request_id: u64,
    /// Which of the wallet's lease trade keys this run talks under; stored with the run.
    pub trade_index: u32,
    /// The seller's node key (66 hex), from the order. The caller has checked the seller's
    /// binding.
    pub seller_node: String,
    /// The network, from the order.
    pub network: Network,
    /// The terms, from the order at the capacity we want.
    pub terms: LeaseTerms,
    /// Our node binding event (JSON).
    pub binding: String,
    /// Where the seller can reach our node.
    pub node_address: Option<String>,
}

/// The buyer's side of one lease.
#[derive(Debug)]
pub struct BuyerRun {
    instance: PublicKey,
    order_id: Uuid,
    request_id: u64,
    trade_index: u32,
    config: BuyerConfig,
    lease: BuyerLease,
    state: BuyerState,
    seller_trade: Option<PublicKey>,
    /// Funding outpoints of the channels our node already had with the seller at the take.
    channels_before: Vec<String>,
    outpoint: Option<String>,
    waiting_on: Option<String>,
    /// Our node binding event and node address, kept so the take can be sent again.
    binding: String,
    node_address: Option<String>,
    /// True when the answer the current state implies has not reached a relay.
    unsent: bool,
}

/// A buyer's run as stored. The preimage is not in it: that lives in the lease's own file.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyerRecord {
    version: u8,
    instance: String,
    order_id: String,
    request_id: u64,
    trade_index: u32,
    lease_id: String,
    max_fee_msat: u64,
    max_cltv_delta: u32,
    state: BuyerState,
    seller_trade: Option<String>,
    channels_before: Vec<String>,
    outpoint: Option<String>,
    binding: String,
    node_address: Option<String>,
    unsent: bool,
}

impl BuyerRun {
    /// A stable name for this run's stored state: the lease id (64 hex). It commits to the
    /// payment hash, so two different preimages can never be stored under one key.
    pub fn store_key(&self) -> String {
        self.lease.contract().id()
    }

    /// Writes the contract and the preimage to a new owner-only file
    /// ([`BuyerLease::save_new`]). Before the take is sent.
    pub fn save_lease(&self, path: &Path) -> Result<(), StoreError> {
        self.lease.save_new(path)
    }

    /// The run as text, to store. The preimage is not in it.
    pub fn to_json(&self) -> Result<String, StoreError> {
        stored_json(&BuyerRecord {
            version: RECORD_VERSION,
            instance: self.instance.to_hex(),
            order_id: self.order_id.to_string(),
            request_id: self.request_id,
            trade_index: self.trade_index,
            lease_id: self.lease.contract().id(),
            max_fee_msat: self.config.max_fee_msat,
            max_cltv_delta: self.config.max_cltv_delta,
            state: self.state,
            seller_trade: self.seller_trade.map(|key| key.to_hex()),
            channels_before: self.channels_before.clone(),
            outpoint: self.outpoint.clone(),
            binding: self.binding.clone(),
            node_address: self.node_address.clone(),
            unsent: self.unsent,
        })
    }

    /// Reads a stored run and joins it with its lease, refusing a record that is for another
    /// lease or whose state needs an outpoint or a peer key it does not have.
    pub fn from_json(json: &str, lease: BuyerLease) -> Result<Self, StoreError> {
        let record: BuyerRecord = parsed_json(json)?;
        if record.version != RECORD_VERSION {
            return Err(corrupt("an unknown record version"));
        }
        if record.lease_id != lease.contract().id() {
            return Err(corrupt("the record is for another lease"));
        }
        let seller_trade = record.seller_trade.as_deref().map(stored_key).transpose()?;
        let state = record.state;
        let needs_peer = matches!(
            state,
            BuyerState::Released | BuyerState::Active | BuyerState::Complete | BuyerState::Closed
        );
        let needs_outpoint = needs_peer || state == BuyerState::Verifying;
        if (needs_outpoint && record.outpoint.is_none()) || (needs_peer && seller_trade.is_none()) {
            return Err(corrupt("the state does not fit the rest of the record"));
        }
        Ok(BuyerRun {
            instance: stored_key(&record.instance)?,
            order_id: Uuid::parse_str(&record.order_id).map_err(|_| corrupt("the order id"))?,
            request_id: record.request_id,
            trade_index: record.trade_index,
            config: BuyerConfig {
                max_fee_msat: record.max_fee_msat,
                max_cltv_delta: record.max_cltv_delta,
            },
            lease,
            state,
            seller_trade,
            channels_before: record.channels_before,
            outpoint: record.outpoint,
            waiting_on: None,
            binding: record.binding,
            node_address: record.node_address,
            unsent: record.unsent,
        })
    }

    /// Starts by taking an order on `instance`: draws the preimage, writes the contract dated
    /// `now`, and returns the run and the take message to send. The channels our node already
    /// has with the seller are noted, so none of them can be passed off as the leased one.
    pub async fn take(
        backend: &dyn LightningBackend,
        instance: PublicKey,
        request: TakeRequest,
        config: BuyerConfig,
        now: u64,
    ) -> Result<(Self, Outgoing), RunError> {
        let lease = BuyerLease::start(
            backend,
            &request.seller_node,
            request.network,
            request.terms,
            now,
        )
        .await?;
        let channels_before = backend
            .channels_with(&request.seller_node)
            .await
            .map_err(LeaseStepError::from)?
            .into_iter()
            .map(|channel| channel.funding_outpoint)
            .collect();
        let message = lease_wire::take(
            request.order_id,
            request.request_id,
            lease.contract(),
            request.binding.clone(),
            request.node_address.clone(),
        );
        let run = BuyerRun {
            instance,
            order_id: request.order_id,
            request_id: request.request_id,
            trade_index: request.trade_index,
            config,
            lease,
            state: BuyerState::Taking,
            seller_trade: None,
            channels_before,
            outpoint: None,
            waiting_on: None,
            binding: request.binding,
            node_address: request.node_address,
            unsent: false,
        };
        Ok((run, Outgoing::ToInstance(message)))
    }

    /// Where the run stands.
    pub fn state(&self) -> BuyerState {
        self.state
    }

    /// Which of the wallet's lease trade keys this run talks under.
    pub fn trade_index(&self) -> u32 {
        self.trade_index
    }

    /// The instance the run is on.
    pub fn instance(&self) -> PublicKey {
        self.instance
    }

    /// The order taken.
    pub fn order_id(&self) -> Uuid {
        self.order_id
    }

    /// The contract.
    pub fn contract(&self) -> &LeaseContract {
        self.lease.contract()
    }

    /// Why the channel the seller reported has not passed our node's check yet, if it has not.
    pub fn waiting_on(&self) -> Option<&str> {
        self.waiting_on.as_deref()
    }

    /// True when the answer the current state implies has not reached a relay yet.
    pub fn unsent(&self) -> bool {
        self.unsent
    }

    /// Records whether the current state's answer still has to be sent. Stored with the run.
    pub fn set_unsent(&mut self, unsent: bool) {
        self.unsent = unsent;
    }

    /// The message the current state implies, made again: the take while it is unanswered, and
    /// the preimage once released. Empty for states that say nothing.
    ///
    /// The preimage is made again the way it was made the first time: our node checks the
    /// channel, and only a passed check lets it out. A channel that no longer passes is an
    /// error, and nothing is sent.
    pub async fn pending(&self, backend: &dyn LightningBackend) -> Result<Vec<Outgoing>, RunError> {
        match (self.state, &self.outpoint, self.seller_trade) {
            (BuyerState::Taking, _, _) => Ok(vec![Outgoing::ToInstance(lease_wire::take(
                self.order_id,
                self.request_id,
                self.lease.contract(),
                self.binding.clone(),
                self.node_address.clone(),
            ))]),
            (BuyerState::Released, Some(outpoint), Some(seller_trade)) => {
                let proof = self.lease.verify_channel(backend, outpoint).await?;
                let secret = self.lease.release(&proof)?;
                Ok(vec![Outgoing::ToPeer {
                    to: seller_trade,
                    message: lease_wire::preimage(self.order_id, self.lease.contract(), secret),
                }])
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Handles one message that `sender` authored.
    pub async fn on_message(
        &mut self,
        incoming: Incoming,
        sender: &PublicKey,
        backend: &dyn LightningBackend,
    ) -> Result<Vec<Outgoing>, RunError> {
        if !lease_wire::sender_ok(
            &incoming,
            sender,
            &self.instance,
            self.seller_trade.as_ref(),
        ) {
            return Err(RunError::Sender);
        }
        if let Incoming::Refused {
            order_id,
            request_id,
            reason,
        } = &incoming
        {
            let ours = match order_id {
                Some(order_id) => *order_id == self.order_id,
                None => *request_id == Some(self.request_id),
            };
            if !ours {
                return Err(RunError::OtherOrder);
            }
            if self.state == BuyerState::Taking {
                self.state = BuyerState::Canceled;
            }
            return Err(RunError::Refused(reason.clone()));
        }
        if incoming.order_id() != Some(self.order_id) {
            return Err(RunError::OtherOrder);
        }

        match (incoming, self.state) {
            (
                Incoming::PeerKey { trade_key, .. },
                BuyerState::Taking | BuyerState::Paid | BuyerState::Verifying,
            ) => {
                learn_peer(&mut self.seller_trade, trade_key)?;
                Ok(Vec::new())
            }
            (Incoming::UpfrontInvoice { invoice, .. }, BuyerState::Taking) => {
                lease_wire::check_names(self.lease.contract(), &invoice.lease_id)?;
                // A run that stopped after paying and before it was saved: the payment is
                // already locked, and paying again would only be refused by our node.
                if matches!(
                    self.lease.payment_state(backend).await,
                    Ok(PaymentState::InFlight)
                ) {
                    self.state = BuyerState::Paid;
                    return Ok(Vec::new());
                }
                let paid = self
                    .lease
                    .pay_upfront(
                        backend,
                        &invoice.invoice,
                        self.config.max_fee_msat,
                        self.config.max_cltv_delta,
                    )
                    .await?;
                if let PaymentState::Failed { reason } = paid {
                    return Err(RunError::PaymentFailed(reason));
                }
                self.state = BuyerState::Paid;
                Ok(Vec::new())
            }
            (Incoming::ChannelOpened { channel, .. }, BuyerState::Paid) => {
                lease_wire::check_names(self.lease.contract(), &channel.lease_id)?;
                let outpoint = channel.channel_point();
                // A channel we already had is not the one this lease pays for, however well it
                // matches the terms.
                if self.channels_before.contains(&outpoint) {
                    return Err(RunError::Step(LeaseStepError::Channel(
                        "the channel existed before this lease".into(),
                    )));
                }
                self.outpoint = Some(outpoint);
                self.state = BuyerState::Verifying;
                Ok(Vec::new())
            }
            (Incoming::CompletionInvoice { invoice, .. }, BuyerState::Active) => {
                self.pay_completion(&invoice, backend).await
            }
            (Incoming::Active { .. }, BuyerState::Released) => {
                self.state = BuyerState::Active;
                Ok(Vec::new())
            }
            (Incoming::Canceled { .. }, BuyerState::Taking) => {
                self.state = BuyerState::Canceled;
                Ok(Vec::new())
            }
            (Incoming::Canceled { .. }, BuyerState::Paid | BuyerState::Verifying) => {
                // Our node decides, not the instance: while the payment is still locked at the
                // seller the lease goes on, and a channel that passes the check is paid for.
                if !matches!(
                    self.lease.payment_state(backend).await?,
                    PaymentState::InFlight
                ) {
                    self.state = BuyerState::Canceled;
                }
                Ok(Vec::new())
            }
            _ => Err(RunError::OutOfOrder("the message does not fit this lease")),
        }
    }

    /// Looks at our node. Once the channel the seller reported passes the check, and only while
    /// our payment is still locked, releases the preimage to the seller's trade key.
    pub async fn tick(
        &mut self,
        backend: &dyn LightningBackend,
    ) -> Result<Vec<Outgoing>, RunError> {
        if self.state != BuyerState::Verifying {
            return Ok(Vec::new());
        }
        let (Some(outpoint), Some(seller_trade)) = (self.outpoint.clone(), self.seller_trade)
        else {
            return Ok(Vec::new());
        };
        match self.lease.payment_state(backend).await? {
            PaymentState::InFlight => {}
            // The seller gave the payment back, or it timed out: there is nothing to release for.
            PaymentState::Failed { .. } => {
                self.state = BuyerState::Canceled;
                return Ok(Vec::new());
            }
            PaymentState::Succeeded { .. } => {
                return Err(RunError::OutOfOrder(
                    "the payment settled before the preimage was released",
                ));
            }
        }
        match self.lease.verify_channel(backend, &outpoint).await {
            Ok(proof) => {
                let secret = self.lease.release(&proof)?;
                self.state = BuyerState::Released;
                self.waiting_on = None;
                Ok(vec![Outgoing::ToPeer {
                    to: seller_trade,
                    message: lease_wire::preimage(self.order_id, self.lease.contract(), secret),
                }])
            }
            Err(LeaseStepError::Channel(why)) => {
                self.waiting_on = Some(why);
                Ok(Vec::new())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// True when nothing more will happen in this run: it was canceled, the completion share
    /// is paid, the channel was gone when it was billed, or the lease is active and had no
    /// completion share.
    pub fn finished(&self) -> bool {
        match self.state {
            BuyerState::Canceled | BuyerState::Complete | BuyerState::Closed => true,
            BuyerState::Active => self.lease.contract().terms.completion_sat() == Ok(0),
            _ => false,
        }
    }

    /// Pays the completion share, if our own node says the lease earned it: the channel is still
    /// there and has as many confirmations as the lease is long. Too early is an out-of-order
    /// message, looked at again later; a channel that is gone ends the lease unpaid.
    async fn pay_completion(
        &mut self,
        invoice: &LeaseInvoice,
        backend: &dyn LightningBackend,
    ) -> Result<Vec<Outgoing>, RunError> {
        let contract = self.lease.contract().clone();
        lease_wire::check_names(&contract, &invoice.lease_id)?;
        let hash = super::check_completion_invoice(&invoice.invoice, &contract)?;
        let Some(outpoint) = self.outpoint.clone() else {
            return Err(RunError::OutOfOrder(
                "no channel is recorded for this lease",
            ));
        };
        let channels = backend
            .channels_with(&contract.seller_node.to_string())
            .await
            .map_err(LeaseStepError::from)?;
        let Some(channel) = channels
            .iter()
            .find(|channel| channel.funding_outpoint == outpoint)
        else {
            self.state = BuyerState::Closed;
            return Ok(Vec::new());
        };
        if channel.confirmations < contract.terms.duration_blocks {
            return Err(RunError::OutOfOrder("the lease has not run its length yet"));
        }
        // A run that stopped after paying and before it was saved does not pay again.
        match backend.payment_state(&hash).await {
            Ok(PaymentState::Succeeded { .. }) => {
                self.state = BuyerState::Complete;
                return Ok(Vec::new());
            }
            Ok(PaymentState::InFlight) => {
                return Err(RunError::OutOfOrder(
                    "the completion payment is still under way",
                ));
            }
            _ => {}
        }
        let paid = backend
            .pay_invoice(
                &invoice.invoice,
                self.config.max_fee_msat,
                COMPLETION_PAY_WAIT_SECS,
            )
            .await
            .map_err(LeaseStepError::from)?;
        match paid {
            PaymentState::Succeeded { .. } => {
                self.state = BuyerState::Complete;
                Ok(Vec::new())
            }
            PaymentState::InFlight => Err(RunError::OutOfOrder(
                "the completion payment is still under way",
            )),
            PaymentState::Failed { reason } => Err(RunError::PaymentFailed(reason)),
        }
    }

    /// Backs out before paying. After the payment is locked there is nothing for the buyer to
    /// cancel: the seller gives the payment back, or it times out.
    pub fn cancel(&mut self, request_id: u64) -> Result<Vec<Outgoing>, RunError> {
        if self.state != BuyerState::Taking {
            return Err(RunError::OutOfOrder("nothing to cancel at this point"));
        }
        self.state = BuyerState::Canceled;
        Ok(vec![Outgoing::ToInstance(lease_wire::cancel(
            self.order_id,
            request_id,
        ))])
    }
}
