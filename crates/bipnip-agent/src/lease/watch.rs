//! Watching a leased channel on our own node, from the moment the lease is active until it has
//! run its length or the channel is gone.
//!
//! A lease's end is counted the way the rest of the lease counts: in confirmations of the funding
//! transaction on our own node. The lease has run its length once our node showed the channel
//! open with as many confirmations as the lease is long. When the channel is gone, how it ended
//! is decided from what our node saw before and what it says of the close; when it cannot be
//! told, the answer is [`Ending::Unknown`], never a guess.

use bipnip_ln::{BackendError, ClosedChannel, Closer, LightningBackend};
use serde::{Deserialize, Serialize};

/// Looks in a row that must find the channel neither open nor known to be closing before it is
/// taken as gone (a node that keeps no record of closed channels says nothing more).
const MISSING_LOOKS: u8 = 2;

/// Who closed the channel, as our node says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClosedBy {
    /// Our node.
    Us,
    /// The other party's node.
    Peer,
    /// Our node does not say.
    Unknown,
}

impl From<Closer> for ClosedBy {
    fn from(closer: Closer) -> Self {
        match closer {
            Closer::Local => ClosedBy::Us,
            Closer::Remote => ClosedBy::Peer,
            Closer::Unknown => ClosedBy::Unknown,
        }
    }
}

/// How the leased channel ended, as far as our node can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ending {
    /// The lease ran its length: the channel was open at the lease's last block.
    Ran,
    /// The channel closed before the lease's last block.
    Early,
    /// The channel is gone and our node cannot say when (nobody looked in time, and the node
    /// gives no close height).
    Unknown,
}

/// The channel is gone: how, and who closed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gone {
    /// How the lease ended.
    pub ending: Ending,
    /// Who closed the channel.
    pub closed_by: ClosedBy,
    /// Height of the block that confirmed the close, where our node gives it.
    pub close_height: Option<u32>,
}

/// What our node has shown of the leased channel. Stored with the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    /// Height of the block that confirmed the funding transaction, once our node counted a
    /// confirmation.
    #[serde(default)]
    pub funding_height: Option<u32>,
    /// Most confirmations our node showed for the channel.
    #[serde(default)]
    pub seen_confirmations: u32,
    /// Looks in a row that found the channel neither open nor known to be closing.
    #[serde(default)]
    pub missing_looks: u8,
    /// Set once the channel is gone.
    #[serde(default)]
    pub gone: Option<Gone>,
}

impl Watch {
    /// True once the lease has run its length: our node showed the channel open with
    /// `duration_blocks` confirmations, or it closed after the lease's last block.
    pub fn ran(&self, duration_blocks: u32) -> bool {
        self.seen_confirmations >= duration_blocks
            || self.gone.is_some_and(|gone| gone.ending == Ending::Ran)
    }

    /// The lease's last block: the one at which the funding transaction has as many
    /// confirmations as the lease is long. Known once a confirmation was seen.
    pub fn last_block(&self, duration_blocks: u32) -> Option<u32> {
        self.funding_height
            .map(|height| height.saturating_add(duration_blocks.saturating_sub(1)))
    }

    /// Notes a look that found the channel open with `confirmations` while our node's best block
    /// is `tip`.
    pub fn saw_open(&mut self, confirmations: u32, tip: u32) {
        self.missing_looks = 0;
        self.seen_confirmations = self.seen_confirmations.max(confirmations);
        if confirmations > 0 && self.funding_height.is_none() {
            self.funding_height = Some(tip.saturating_add(1).saturating_sub(confirmations));
        }
    }

    /// Notes a look that did not find the channel open. `closed` is what our node says of a close:
    /// `Some` when it knows the channel is closing or closed, `None` when it does not know or
    /// cannot say. Our node's best block is `tip`. Settles [`Watch::gone`] at once when the node
    /// knows of the close, else after [`MISSING_LOOKS`] such looks in a row.
    pub fn saw_missing(&mut self, closed: Option<ClosedChannel>, tip: u32, duration_blocks: u32) {
        if self.gone.is_some() {
            return;
        }
        let (closed_by, close_height) = match closed {
            Some(closed) => (closed.closer.into(), closed.close_height),
            None => {
                self.missing_looks = self.missing_looks.saturating_add(1);
                if self.missing_looks < MISSING_LOOKS {
                    return;
                }
                (ClosedBy::Unknown, None)
            }
        };
        let ending = self.ending(close_height, tip, duration_blocks);
        self.gone = Some(Gone {
            ending,
            closed_by,
            close_height,
        });
    }

    fn ending(&self, close_height: Option<u32>, tip: u32, duration_blocks: u32) -> Ending {
        if self.seen_confirmations >= duration_blocks {
            return Ending::Ran;
        }
        let Some(last) = self.last_block(duration_blocks) else {
            return Ending::Unknown;
        };
        // The channel is not open now, and the lease's last block has not even been reached.
        if tip < last {
            return Ending::Early;
        }
        match close_height {
            // Closed in a block after the last one: at the last block it was still open.
            Some(height) if height > last => Ending::Ran,
            Some(_) => Ending::Early,
            None => Ending::Unknown,
        }
    }

    /// Looks at our node: notes the channel at `outpoint` with `peer` as open, or as missing with
    /// what the node says of its close. A node that keeps no record of closed channels counts as
    /// saying nothing; any other failure is passed on and nothing is noted.
    pub async fn look(
        &mut self,
        backend: &dyn LightningBackend,
        peer: &str,
        outpoint: &str,
        duration_blocks: u32,
    ) -> Result<(), BackendError> {
        if self.gone.is_some() {
            return Ok(());
        }
        let channels = backend.channels_with(peer).await?;
        let tip = backend.node_info().await?.block_height;
        if let Some(channel) = channels
            .iter()
            .find(|channel| channel.funding_outpoint == outpoint)
        {
            self.saw_open(channel.confirmations, tip);
            return Ok(());
        }
        let closed = match backend.closed_channel(peer, outpoint).await {
            Ok(closed) => closed,
            Err(BackendError::Unsupported(_)) => None,
            Err(e) => return Err(e),
        };
        self.saw_missing(closed, tip, duration_blocks);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DURATION: u32 = 100;

    fn closed(closer: Closer, close_height: Option<u32>) -> Option<ClosedChannel> {
        Some(ClosedChannel {
            closer,
            close_height,
        })
    }

    /// A watch that saw the channel open with `confirmations` at `tip`.
    fn seen(confirmations: u32, tip: u32) -> Watch {
        let mut watch = Watch::default();
        watch.saw_open(confirmations, tip);
        watch
    }

    #[test]
    fn the_funding_height_and_the_last_block_come_from_the_first_confirmed_look() {
        let mut watch = Watch::default();
        watch.saw_open(0, 200);
        assert_eq!(watch.funding_height, None);
        watch.saw_open(3, 202);
        assert_eq!(watch.funding_height, Some(200));
        // A later look does not move it.
        watch.saw_open(10, 209);
        assert_eq!(watch.funding_height, Some(200));
        assert_eq!(watch.last_block(DURATION), Some(299));
        assert_eq!(watch.seen_confirmations, 10);
        assert!(!watch.ran(DURATION));
        watch.saw_open(DURATION, 299);
        assert!(watch.ran(DURATION));
    }

    #[test]
    fn a_close_the_node_reports_settles_at_once() {
        // Closed by the peer at block 250, before the last block (299).
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Remote, Some(250)), 255, DURATION);
        assert_eq!(
            watch.gone,
            Some(Gone {
                ending: Ending::Early,
                closed_by: ClosedBy::Peer,
                close_height: Some(250),
            })
        );
        assert!(!watch.ran(DURATION));
        // Nothing moves it afterwards.
        watch.saw_missing(closed(Closer::Local, Some(400)), 400, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Early));
    }

    #[test]
    fn a_close_after_the_last_block_means_the_lease_ran_even_unseen() {
        // Our agent was away from block 209 until after the close at 300.
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Local, Some(300)), 310, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Ran));
        assert!(watch.ran(DURATION));
        // Closed in the last block itself: at that block it was no longer open.
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Local, Some(299)), 310, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Early));
    }

    #[test]
    fn a_close_with_no_height_is_early_only_before_the_last_block() {
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Remote, None), 250, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Early));
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Remote, None), 310, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Unknown));
        // Seen through to the end: it ran, whatever the node says of the close.
        let mut watch = seen(DURATION, 299);
        watch.saw_missing(closed(Closer::Remote, None), 310, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Ran));
    }

    #[test]
    fn a_channel_missing_with_nothing_said_is_gone_only_on_the_second_look() {
        let mut watch = seen(10, 209);
        watch.saw_missing(None, 220, DURATION);
        assert_eq!(watch.gone, None);
        // Open again on the next look (a node that briefly did not list it): the count restarts.
        watch.saw_open(12, 211);
        watch.saw_missing(None, 220, DURATION);
        assert_eq!(watch.gone, None);
        watch.saw_missing(None, 221, DURATION);
        assert_eq!(
            watch.gone,
            Some(Gone {
                ending: Ending::Early,
                closed_by: ClosedBy::Unknown,
                close_height: None,
            })
        );
    }

    #[test]
    fn with_no_confirmation_ever_seen_nothing_can_be_told() {
        let mut watch = Watch::default();
        watch.saw_missing(closed(Closer::Remote, Some(250)), 255, DURATION);
        assert_eq!(watch.gone.map(|gone| gone.ending), Some(Ending::Unknown));
    }

    #[test]
    fn a_stored_watch_reads_back_and_an_empty_one_is_the_default() {
        let mut watch = seen(10, 209);
        watch.saw_missing(closed(Closer::Remote, Some(250)), 255, DURATION);
        let text = serde_json::to_string(&watch).expect("json");
        assert_eq!(serde_json::from_str::<Watch>(&text).expect("read"), watch);
        assert_eq!(
            serde_json::from_str::<Watch>("{}").expect("read"),
            Watch::default()
        );
        assert!(serde_json::from_str::<Watch>(r#"{"other":1}"#).is_err());
    }
}
