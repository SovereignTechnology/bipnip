//! A lease run on relays: brings a run the messages the relays hold for it, saves the run, and
//! sends what it answers.
//!
//! The order within a step is the point of this module:
//!
//! 1. a message is handed to the run, or the run looks at its node (`tick`);
//! 2. the run is saved;
//! 3. only then are its answers sent.
//!
//! So a crash never leaves a message out that the stored run does not know it sent, and a
//! buyer's take ([`launch_buyer`]) never leaves before its preimage is on disk. A failed save
//! stops the step: a run that cannot be saved must not go on.
//!
//! Relays are polled, not subscribed to. A lease moves at the pace of payments and block
//! confirmations, so a few seconds between looks cost nothing, and a poll needs no connection
//! state to get wrong.
//!
//! An answer no relay took is owed, not lost. The run is marked and saved, and the next step
//! starts by making the message its state implies again and sending that. What is sent twice
//! does no harm: the instance refuses a step it already recorded, and a run refuses a message
//! it already handled.

use std::collections::HashSet;

use bipnip_ln::LightningBackend;
use mostro_core::prelude::Message;
use nostr_sdk::prelude::{Client, Event, EventId, Filter, Keys, Kind, PublicKey, Timestamp};

use super::run::{BuyerRun, Outgoing, RunError, SellerRun};
use super::store::{LeaseStore, StoreError};
use crate::lease_wire::{self, Incoming};
use crate::relay::{RELAY_TIMEOUT, connected};
use crate::ring_wire::{self, RingIncoming};

/// A message a poll found, not handled yet.
#[derive(Debug)]
pub struct Received {
    /// The event that carried it.
    pub id: EventId,
    /// The message, with its own fields checked.
    pub incoming: Incoming,
    /// The key that authored the event.
    pub sender: PublicKey,
}

/// One run's connection to its instance's relays, under one trade key.
pub struct Link {
    client: Client,
    identity: Keys,
    trade: Keys,
    instance: PublicKey,
    pow: u8,
    since: Timestamp,
    done: HashSet<EventId>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Link(trade key {})", self.trade.public_key())
    }
}

impl Link {
    /// Connects to `relays`. Messages are read for the `trade` key from `since` (unix seconds)
    /// on; after a restart, pass a time before the run's last save, and the run itself refuses
    /// what it already handled. `pow` is the proof of work the instance asks for.
    pub async fn connect(
        relays: &[String],
        identity: Keys,
        trade: Keys,
        instance: PublicKey,
        pow: u8,
        since: u64,
    ) -> Result<Self, String> {
        Ok(Link {
            client: connected(relays).await?,
            identity,
            trade,
            instance,
            pow,
            since: Timestamp::from(since),
            done: HashSet::new(),
        })
    }

    /// Our liquidity identity, the key our node is bound to.
    pub fn identity(&self) -> &Keys {
        &self.identity
    }

    async fn publish(&self, event: &Event) -> Result<(), String> {
        match tokio::time::timeout(RELAY_TIMEOUT, self.client.send_event(event)).await {
            Err(_) => Err("no relay answered in time".into()),
            Ok(Err(e)) => Err(format!("publish: {e}")),
            Ok(Ok(output)) if output.success.is_empty() => {
                Err(format!("no relay accepted the event: {:?}", output.failed))
            }
            Ok(Ok(_)) => Ok(()),
        }
    }

    /// Wraps a message for its receiver and sends it. `Ok` means at least one relay took it.
    /// The error names relays and their reasons, never the message.
    pub async fn send(&self, outgoing: &Outgoing) -> Result<(), String> {
        let event = outgoing
            .seal(&self.identity, &self.trade, self.instance, self.pow)
            .map_err(|e| e.to_string())?;
        self.publish(&event).await
    }

    /// Wraps a message for the instance and sends it. For runs whose every message goes to the
    /// instance (rings). A lease preimage is refused here as everywhere.
    pub async fn send_to_instance(&self, message: &Message) -> Result<(), String> {
        let event = lease_wire::seal_for_instance(
            message,
            &self.identity,
            &self.trade,
            self.instance,
            self.pow,
        )
        .map_err(|e| e.to_string())?;
        self.publish(&event).await
    }

    /// The events the relays hold for our trade key that are not marked done, oldest first.
    async fn fresh(&self) -> Result<Vec<Event>, String> {
        let filter = Filter::new()
            .kind(Kind::PrivateDirectMessage)
            .pubkey(self.trade.public_key())
            .since(self.since);
        let events = self
            .client
            .fetch_events(filter)
            .timeout(RELAY_TIMEOUT)
            .await
            .map_err(|e| format!("fetch: {e}"))?;
        let mut fresh: Vec<Event> = events
            .iter()
            .filter(|event| !self.done.contains(&event.id))
            .cloned()
            .collect();
        fresh.sort_by_key(|event| (event.created_at, event.id));
        Ok(fresh)
    }

    /// The messages the relays hold for our trade key that are not marked done, oldest first.
    /// Events that are not for us, or do not open as a lease message, are marked done and
    /// skipped.
    pub async fn poll(&mut self) -> Result<Vec<Received>, String> {
        let mut received = Vec::new();
        for event in self.fresh().await? {
            match lease_wire::open(&event, &self.trade) {
                Ok(Some((incoming, sender))) => received.push(Received {
                    id: event.id,
                    incoming,
                    sender,
                }),
                Ok(None) | Err(_) => {
                    self.done.insert(event.id);
                }
            }
        }
        Ok(received)
    }

    /// As [`Link::poll`], for a ring run: the ring messages waiting for our trade key, each with
    /// its event id and the key that authored it.
    pub async fn poll_ring(&mut self) -> Result<Vec<(EventId, RingIncoming, PublicKey)>, String> {
        let mut received = Vec::new();
        for event in self.fresh().await? {
            match ring_wire::open(&event, &self.trade) {
                Ok(Some((incoming, sender))) => received.push((event.id, incoming, sender)),
                Ok(None) | Err(_) => {
                    self.done.insert(event.id);
                }
            }
        }
        Ok(received)
    }

    /// Marks an event as handled: later polls skip it.
    pub fn mark_done(&mut self, id: EventId) {
        self.done.insert(id);
    }

    /// Disconnects from the relays.
    pub async fn close(self) {
        self.client.shutdown().await;
    }
}

/// What one step did.
#[derive(Debug, Default)]
pub struct StepReport {
    /// Messages the run acted on.
    pub handled: usize,
    /// Answers sent.
    pub sent: usize,
    /// What did not work: a poll that failed, a message the run refused, an answer no relay
    /// took. Never holds message contents.
    pub problems: Vec<String>,
}

/// Sends each message; true when every one reached a relay.
async fn send_all(link: &Link, outgoing: &[Outgoing], report: &mut StepReport) -> bool {
    let mut all_sent = true;
    for message in outgoing {
        match link.send(message).await {
            Ok(()) => report.sent += 1,
            Err(e) => {
                all_sent = false;
                report.problems.push(format!("not sent: {e}"));
            }
        }
    }
    all_sent
}

/// Whether a refused message should be looked at again: one that came too early may fit later
/// (relays do not promise an order, and a completion invoice can arrive before our node agrees
/// the lease is over), and a payment our node could not make now may go through later.
fn retry_later(error: &RunError) -> bool {
    matches!(error, RunError::OutOfOrder(_) | RunError::PaymentFailed(_))
}

/// Saves a seller's new run, then sends its offer. If no relay takes it, the run is saved again
/// as owing it, and the next step sends it.
pub async fn launch_seller(
    run: &mut SellerRun,
    offer: &Outgoing,
    link: &Link,
    store: &LeaseStore,
) -> Result<StepReport, StoreError> {
    store.save_seller(run)?;
    let mut report = StepReport::default();
    if !send_all(link, std::slice::from_ref(offer), &mut report).await {
        run.set_unsent(true);
        store.save_seller(run)?;
    }
    Ok(report)
}

/// Saves a buyer's new run, preimage included, then sends its take. If no relay takes it, the
/// run is saved again as owing it, and the next step sends it.
pub async fn launch_buyer(
    run: &mut BuyerRun,
    take: &Outgoing,
    link: &Link,
    store: &LeaseStore,
) -> Result<StepReport, StoreError> {
    store.save_buyer(run)?;
    let mut report = StepReport::default();
    if !send_all(link, std::slice::from_ref(take), &mut report).await {
        run.set_unsent(true);
        store.save_buyer(run)?;
    }
    Ok(report)
}

/// One step of a seller's run at `now` (unix seconds): the messages waiting on the relays, then
/// a look at the node. Fails only when the run cannot be saved.
pub async fn step_seller(
    run: &mut SellerRun,
    link: &mut Link,
    backend: &dyn LightningBackend,
    store: &LeaseStore,
    now: u64,
) -> Result<StepReport, StoreError> {
    let mut report = StepReport::default();
    if run.unsent() {
        match run.pending() {
            Ok(owed) => {
                if send_all(link, &owed, &mut report).await {
                    run.set_unsent(false);
                    store.save_seller(run)?;
                }
            }
            Err(e) => report.problems.push(format!("cannot send again: {e}")),
        }
    }
    match link.poll().await {
        Err(e) => report.problems.push(e),
        Ok(received) => {
            for message in received {
                match run
                    .on_message(message.incoming, &message.sender, backend, now)
                    .await
                {
                    Ok(outgoing) => {
                        // A new answer replaces whatever was owed before it.
                        run.set_unsent(false);
                        store.save_seller(run)?;
                        link.mark_done(message.id);
                        report.handled += 1;
                        if !send_all(link, &outgoing, &mut report).await {
                            run.set_unsent(true);
                            store.save_seller(run)?;
                        }
                    }
                    Err(e) => {
                        // A refusal can still change the run (a refused offer ends it).
                        store.save_seller(run)?;
                        if !retry_later(&e) {
                            link.mark_done(message.id);
                            report.problems.push(format!("refused a message: {e}"));
                        }
                    }
                }
            }
        }
    }
    match run.tick(backend).await {
        Ok(outgoing) => {
            store.save_seller(run)?;
            if !send_all(link, &outgoing, &mut report).await {
                run.set_unsent(true);
                store.save_seller(run)?;
            }
        }
        Err(e) => report.problems.push(format!("node: {e}")),
    }
    Ok(report)
}

/// One step of a buyer's run: the messages waiting on the relays, then a look at the node.
/// Fails only when the run cannot be saved.
pub async fn step_buyer(
    run: &mut BuyerRun,
    link: &mut Link,
    backend: &dyn LightningBackend,
    store: &LeaseStore,
) -> Result<StepReport, StoreError> {
    let mut report = StepReport::default();
    if run.unsent() {
        match run.pending(backend).await {
            Ok(owed) => {
                if send_all(link, &owed, &mut report).await {
                    run.set_unsent(false);
                    store.save_buyer(run)?;
                }
            }
            Err(e) => report.problems.push(format!("cannot send again: {e}")),
        }
    }
    match link.poll().await {
        Err(e) => report.problems.push(e),
        Ok(received) => {
            for message in received {
                match run
                    .on_message(message.incoming, &message.sender, backend)
                    .await
                {
                    Ok(outgoing) => {
                        run.set_unsent(false);
                        store.save_buyer(run)?;
                        link.mark_done(message.id);
                        report.handled += 1;
                        if !send_all(link, &outgoing, &mut report).await {
                            run.set_unsent(true);
                            store.save_buyer(run)?;
                        }
                    }
                    Err(e) => {
                        store.save_buyer(run)?;
                        if !retry_later(&e) {
                            link.mark_done(message.id);
                            report.problems.push(format!("refused a message: {e}"));
                        }
                    }
                }
            }
        }
    }
    match run.tick(backend).await {
        Ok(outgoing) => {
            store.save_buyer(run)?;
            if !send_all(link, &outgoing, &mut report).await {
                run.set_unsent(true);
                store.save_buyer(run)?;
            }
        }
        Err(e) => report.problems.push(format!("node: {e}")),
    }
    Ok(report)
}
