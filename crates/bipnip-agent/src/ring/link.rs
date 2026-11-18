//! A ring run on relays: the same order of work as for leases ([`crate::lease::link`]). A
//! message is handed to the run, the run is saved, and only then are its answers sent; a failed
//! save stops the step. Every answer goes to the instance.
//!
//! An answer no relay took is owed, not lost. It is kept with the run, in order, and the next
//! step starts by sending what is owed. Nothing newer is sent past something still owed, so the
//! instance gets a member's messages in the order the member made them. What is owed was, by
//! definition, never seen by the instance, so sending it later is not sending it twice.

use bipnip_ln::LightningBackend;
use mostro_core::prelude::Message;

use super::run::{RingRun, RingRunError};
use super::store::RingStore;
use crate::lease::link::{Link, StepReport, publish_outcome};
use crate::lease::store::StoreError;

/// Sends the messages in order and stops at the first that no relay takes. Returns that one and
/// everything after it: what is still owed.
async fn send_in_order(link: &Link, messages: &[Message], report: &mut StepReport) -> Vec<Message> {
    for (sent, message) in messages.iter().enumerate() {
        if let Err(e) = link.send_to_instance(message).await {
            report.problems.push(format!("not sent: {e}"));
            return messages[sent..].to_vec();
        }
        report.sent += 1;
    }
    Vec::new()
}

/// Sends a saved run's new answers, behind whatever it still owes, and saves the run again if
/// anything is left owed.
async fn send_or_owe(
    run: &mut RingRun,
    messages: Vec<Message>,
    link: &Link,
    store: &RingStore,
    report: &mut StepReport,
) -> Result<(), StoreError> {
    if messages.is_empty() {
        return Ok(());
    }
    let left = if run.unsent().is_empty() {
        send_in_order(link, &messages, report).await
    } else {
        messages
    };
    if !left.is_empty() {
        let mut owed = run.unsent().to_vec();
        owed.extend(left);
        run.set_unsent(owed);
        store.save(run)?;
    }
    Ok(())
}

/// Saves a new run, then sends its first message (the offer or the take). If no relay takes
/// it, the run is saved again as owing it, and the next step sends it.
pub async fn launch(
    run: &mut RingRun,
    first: &Message,
    link: &Link,
    store: &RingStore,
) -> Result<StepReport, StoreError> {
    store.save(run)?;
    let mut report = StepReport::default();
    send_or_owe(run, vec![first.clone()], link, store, &mut report).await?;
    Ok(report)
}

/// One step of a ring run at `now` (unix seconds): what is still owed, then the messages
/// waiting on the relays, then a look at the node. Fails only when the run cannot be saved.
pub async fn step(
    run: &mut RingRun,
    link: &mut Link,
    backend: &dyn LightningBackend,
    store: &RingStore,
    now: u64,
) -> Result<StepReport, StoreError> {
    let mut report = StepReport::default();
    if !run.unsent().is_empty() {
        let owed = run.unsent().to_vec();
        let left = send_in_order(link, &owed, &mut report).await;
        if left.len() != owed.len() {
            run.set_unsent(left);
            store.save(run)?;
        }
    }
    match link.poll_ring().await {
        Err(e) => report.problems.push(e),
        Ok(received) => {
            for (id, incoming, sender) in received {
                let identity = link.identity().clone();
                match run
                    .on_message(incoming, &sender, &identity, backend, now)
                    .await
                {
                    Ok(messages) => {
                        store.save(run)?;
                        link.mark_done(id);
                        report.handled += 1;
                        send_or_owe(run, messages, link, store, &mut report).await?;
                    }
                    Err(e) => {
                        // A refusal can still change the run (a refused take ends it).
                        store.save(run)?;
                        // Only a message that came too early may fit later.
                        if !matches!(e, RingRunError::OutOfOrder(_)) {
                            link.mark_done(id);
                            report.problems.push(format!("refused a message: {e}"));
                        }
                    }
                }
            }
        }
    }
    match run.tick(backend, now).await {
        Ok(messages) => {
            store.save(run)?;
            send_or_owe(run, messages, link, store, &mut report).await?;
        }
        Err(e) => report.problems.push(format!("node: {e}")),
    }
    if run.outcome_owed() {
        let done = match (link.outcomes().allows(run.announced()), run.outcome_event()) {
            (true, Some(Ok(builder))) => publish_outcome(link, builder, &mut report).await,
            (true, Some(Err(e))) => {
                report.problems.push(format!("outcome: {e}"));
                true
            }
            _ => true,
        };
        if done {
            run.set_outcome_sent();
            store.save(run)?;
        }
    }
    Ok(report)
}
