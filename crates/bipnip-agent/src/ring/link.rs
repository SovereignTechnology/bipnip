//! A ring run on relays: the same order of work as for leases ([`crate::lease::link`]). A
//! message is handed to the run, the run is saved, and only then are its answers sent; a failed
//! save stops the step. Every answer goes to the instance.
//!
//! Not here yet: sending again what a relay did not take. A failed send is reported in the
//! [`StepReport`].

use bipnip_ln::LightningBackend;
use mostro_core::prelude::Message;

use super::run::{RingRun, RingRunError};
use super::store::RingStore;
use crate::lease::link::{Link, StepReport};
use crate::lease::store::StoreError;

async fn send_all(link: &Link, messages: &[Message], report: &mut StepReport) {
    for message in messages {
        match link.send_to_instance(message).await {
            Ok(()) => report.sent += 1,
            Err(e) => report.problems.push(format!("not sent: {e}")),
        }
    }
}

/// Saves a new run, then sends its first message (the offer or the take).
pub async fn launch(
    run: &RingRun,
    first: &Message,
    link: &Link,
    store: &RingStore,
) -> Result<StepReport, StoreError> {
    store.save(run)?;
    let mut report = StepReport::default();
    send_all(link, std::slice::from_ref(first), &mut report).await;
    Ok(report)
}

/// One step of a ring run at `now` (unix seconds): the messages waiting on the relays, then a
/// look at the node. Fails only when the run cannot be saved.
pub async fn step(
    run: &mut RingRun,
    link: &mut Link,
    backend: &dyn LightningBackend,
    store: &RingStore,
    now: u64,
) -> Result<StepReport, StoreError> {
    let mut report = StepReport::default();
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
                        send_all(link, &messages, &mut report).await;
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
            send_all(link, &messages, &mut report).await;
        }
        Err(e) => report.problems.push(format!("node: {e}")),
    }
    Ok(report)
}
