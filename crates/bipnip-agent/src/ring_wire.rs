//! Ring messages on the wire: what the agent sends to an instance and how it reads what comes
//! back (`docs/nips/liquidity-rings.md`).
//!
//! Pure: no relay and no node. Every ring message a member acts on comes from the instance the
//! ring runs on; members never write to each other. So the one sender rule here is that the
//! event's author is that instance ([`from_instance`]). What the instance says is then checked
//! by [`crate::ring::run`]: the roster's bindings, every commit, and the channel on our own
//! node.

use mostro_core::prelude::{Action, CantDoReason, Message, Payload, unwrap_message_nip44};
use mostro_core::ring::Ring;
use mostro_core::ring_message::{
    RingClosed, RingCommit, RingEdge, RingMessageError, RingOffer, RingOpen, RingPayload,
    RingRoster, RingSeat,
};
use nostr_sdk::prelude::{Event, Keys, PublicKey};
use uuid::Uuid;

/// Why a ring message was not built, opened or accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RingWireError {
    /// Not a ring message this agent understands, or put together wrongly.
    Shape(&'static str),
    /// A field, the roster, a binding or a commit failed its checks.
    Invalid(RingMessageError),
    /// The event could not be opened.
    Transport(String),
}

impl std::fmt::Display for RingWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RingWireError::Shape(what) => write!(f, "not a usable ring message: {what}"),
            RingWireError::Invalid(e) => write!(f, "{e}"),
            RingWireError::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

impl std::error::Error for RingWireError {}

impl From<RingMessageError> for RingWireError {
    fn from(e: RingMessageError) -> Self {
        RingWireError::Invalid(e)
    }
}

fn ring_message(
    order_id: Option<Uuid>,
    request_id: Option<u64>,
    action: Action,
    payload: RingPayload,
) -> Message {
    Message::new_order(
        order_id,
        request_id,
        None,
        action,
        Some(Payload::Ring(payload)),
    )
}

/// Creator → instance: a new ring. The instance echoes it with the order id, under
/// `request_id`.
pub fn offer(request_id: u64, offer: RingOffer) -> Message {
    ring_message(
        None,
        Some(request_id),
        Action::RingOffer,
        RingPayload::Offer(offer),
    )
}

/// Member → instance: take a slot of the ring `order_id` with `seat`.
pub fn take(order_id: Uuid, request_id: u64, seat: RingSeat) -> Message {
    ring_message(
        Some(order_id),
        Some(request_id),
        Action::TakeRingSlot,
        RingPayload::Take(seat),
    )
}

/// Member → instance: our signature of the ring id.
pub fn commit(order_id: Uuid, ring: &Ring, identity: &Keys) -> Message {
    ring_message(
        Some(order_id),
        None,
        Action::RingCommit,
        RingPayload::Commit(RingCommit::sign(ring, identity)),
    )
}

fn edge(ring: &Ring, funding_outpoint: &str) -> Result<RingEdge, RingWireError> {
    let (txid, index) = funding_outpoint
        .split_once(':')
        .ok_or(RingWireError::Shape("funding outpoint"))?;
    let edge = RingEdge {
        ring_id: ring.id(),
        funding_txid: txid.to_string(),
        output_index: index
            .parse()
            .map_err(|_| RingWireError::Shape("funding outpoint"))?,
    };
    edge.validate()?;
    Ok(edge)
}

/// Opener → instance → the member opened to: the funding outpoint of our channel, `<txid>:<index>`.
pub fn edge_opened(
    order_id: Uuid,
    ring: &Ring,
    funding_outpoint: &str,
) -> Result<Message, RingWireError> {
    Ok(ring_message(
        Some(order_id),
        None,
        Action::RingEdgeOpened,
        RingPayload::Edge(edge(ring, funding_outpoint)?),
    ))
}

/// Receiver → instance → the opener: our node sees the channel at this outpoint.
pub fn edge_verified(
    order_id: Uuid,
    ring: &Ring,
    funding_outpoint: &str,
) -> Result<Message, RingWireError> {
    Ok(ring_message(
        Some(order_id),
        None,
        Action::RingEdgeVerified,
        RingPayload::Edge(edge(ring, funding_outpoint)?),
    ))
}

/// Creator or member → instance, while slots are open: withdraw the ring, or give the slot back.
pub fn cancel(order_id: Uuid, request_id: u64) -> Message {
    Message::new_order(Some(order_id), Some(request_id), None, Action::Cancel, None)
}

/// A ring message as received, with its own fields checked. Whether it is about a ring this
/// agent is in, and whether what it claims holds, is for [`crate::ring::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RingIncoming {
    /// The instance stored our ring under `order_id`.
    Offered {
        /// The order id the instance gave the ring.
        order_id: Uuid,
        /// The request id we sent the offer under.
        request_id: Option<u64>,
        /// The offer as the instance stored it.
        offer: RingOffer,
    },
    /// The instance gave us the slot we asked for.
    SeatTaken {
        /// The ring's order id.
        order_id: Uuid,
        /// The request id we sent the take under.
        request_id: Option<u64>,
        /// The seat as the instance stored it.
        seat: RingSeat,
    },
    /// The ring is full: its members in order.
    Roster {
        /// The ring's order id.
        order_id: Uuid,
        /// The canonical text and every member's seat.
        roster: RingRoster,
    },
    /// Every member has committed.
    Open {
        /// The ring's order id.
        order_id: Uuid,
        /// Every member's commit.
        open: RingOpen,
    },
    /// The previous member says it opened a channel to us.
    EdgeOpened {
        /// The ring's order id.
        order_id: Uuid,
        /// Where to look; our own node is the judge.
        edge: RingEdge,
    },
    /// The next member confirmed the channel we opened to it.
    EdgeVerified {
        /// The ring's order id.
        order_id: Uuid,
        /// The channel confirmed.
        edge: RingEdge,
    },
    /// The ring is over.
    Closed {
        /// The ring's order id.
        order_id: Uuid,
        /// Which members' channels were confirmed.
        closed: RingClosed,
    },
    /// The ring was withdrawn or expired before it filled, or our slot was given back.
    Canceled {
        /// The ring's order id.
        order_id: Uuid,
    },
    /// The instance refused something we sent.
    Refused {
        /// The order the refusal is about, if any.
        order_id: Option<Uuid>,
        /// The request id of what was refused, if we gave one.
        request_id: Option<u64>,
        /// Why, if the instance said.
        reason: Option<CantDoReason>,
    },
}

impl RingIncoming {
    /// The order the message is about.
    pub fn order_id(&self) -> Option<Uuid> {
        match self {
            RingIncoming::Offered { order_id, .. }
            | RingIncoming::SeatTaken { order_id, .. }
            | RingIncoming::Roster { order_id, .. }
            | RingIncoming::Open { order_id, .. }
            | RingIncoming::EdgeOpened { order_id, .. }
            | RingIncoming::EdgeVerified { order_id, .. }
            | RingIncoming::Closed { order_id, .. }
            | RingIncoming::Canceled { order_id } => Some(*order_id),
            RingIncoming::Refused { order_id, .. } => *order_id,
        }
    }
}

/// Reads a ring message and checks its own fields.
pub fn read(message: &Message) -> Result<RingIncoming, RingWireError> {
    let kind = message.get_inner_message_kind();
    if let Message::CantDo(_) = message {
        let reason = match &kind.payload {
            Some(Payload::CantDo(reason)) => reason.clone(),
            _ => None,
        };
        return Ok(RingIncoming::Refused {
            order_id: kind.id,
            request_id: kind.request_id,
            reason,
        });
    }
    let Message::Order(_) = message else {
        return Err(RingWireError::Shape("not an order message"));
    };
    let order_id = kind.id.ok_or(RingWireError::Shape("no order id"))?;
    let ring = match &kind.payload {
        Some(Payload::Ring(payload)) => Some(payload),
        _ => None,
    };
    if let Some(payload) = ring {
        payload.validate()?;
    }
    Ok(match (&kind.action, ring, &kind.payload) {
        (Action::RingOffer, Some(RingPayload::Offer(offer)), _) => RingIncoming::Offered {
            order_id,
            request_id: kind.request_id,
            offer: offer.clone(),
        },
        (Action::TakeRingSlot, Some(RingPayload::Take(seat)), _) => RingIncoming::SeatTaken {
            order_id,
            request_id: kind.request_id,
            seat: seat.clone(),
        },
        (Action::RingRoster, Some(RingPayload::Roster(roster)), _) => RingIncoming::Roster {
            order_id,
            roster: roster.clone(),
        },
        (Action::RingOpen, Some(RingPayload::Open(open)), _) => RingIncoming::Open {
            order_id,
            open: open.clone(),
        },
        (Action::RingEdgeOpened, Some(RingPayload::Edge(edge)), _) => RingIncoming::EdgeOpened {
            order_id,
            edge: edge.clone(),
        },
        (Action::RingEdgeVerified, Some(RingPayload::Edge(edge)), _) => {
            RingIncoming::EdgeVerified {
                order_id,
                edge: edge.clone(),
            }
        }
        (Action::RingClosed, Some(RingPayload::Closed(closed)), _) => RingIncoming::Closed {
            order_id,
            closed: closed.clone(),
        },
        (Action::Canceled, None, None) => RingIncoming::Canceled { order_id },
        _ => return Err(RingWireError::Shape("not a ring message")),
    })
}

/// True when `sender`, the key that authored the event, is the instance the ring runs on. No
/// ring message is acted on from anyone else.
pub fn from_instance(sender: &PublicKey, instance: &PublicKey) -> bool {
    sender == instance
}

/// Opens an event addressed to our trade key. `Ok(None)` when it is not for us. On success, the
/// message with its own fields checked, and the key that authored the event: pass it to
/// [`from_instance`] before acting on anything.
pub fn open(
    event: &Event,
    trade: &Keys,
) -> Result<Option<(RingIncoming, PublicKey)>, RingWireError> {
    let Some(opened) =
        unwrap_message_nip44(event, trade).map_err(|e| RingWireError::Transport(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(Some((read(&opened.message)?, opened.sender)))
}
