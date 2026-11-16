//! Lease messages on the wire: what the agent sends to an instance and to its lease peer, and how
//! it reads what comes back (`docs/nips/liquidity-leases.md`).
//!
//! Pure: no relay and no node. Three rules live here because nothing downstream may forget them:
//!
//! - The preimage goes from the buyer to the seller's trade key only. [`seal_for_peer`] is the
//!   only way this module wraps it, and [`Incoming::must_come_from`] accepts it from the peer
//!   only, so a preimage an instance relays (it never should) is not acted on.
//! - Everything else must come from the instance the lease runs on. A message that claims to be
//!   the seller's invoice but was sent by some other key is dropped by [`sender_ok`].
//! - The seller checks a take itself ([`check_take`]): the instance already did, but an instance
//!   is not trusted with the price.

use mostro_core::lease::LeaseContract;
use mostro_core::lease_message::{
    LeaseChannel, LeaseInvoice, LeaseMessageError, LeaseOffer, LeasePayload, LeasePreimage,
    LeaseTake,
};
use mostro_core::prelude::{
    Action, CantDoReason, Message, Payload, WrapOptions, unwrap_message_nip44, wrap_message_nip44,
};
use nostr_sdk::prelude::{Event, Keys, PublicKey};
use uuid::Uuid;

/// Oldest contract a seller accepts in a take: one hour.
pub const MAX_CONTRACT_AGE_SECS: u64 = 3600;
/// How far ahead of the seller's clock a contract may be dated.
pub const MAX_CONTRACT_FUTURE_SECS: u64 = 600;

/// Why a lease message was not built, opened or accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Not a lease message this agent understands, or put together wrongly.
    Shape(&'static str),
    /// A field, the contract or a binding failed its checks.
    Invalid(LeaseMessageError),
    /// Well-formed, but not for the offer or lease at hand.
    NotOurs(&'static str),
    /// The event could not be wrapped or opened.
    Transport(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Shape(what) => write!(f, "not a usable lease message: {what}"),
            WireError::Invalid(e) => write!(f, "{e}"),
            WireError::NotOurs(what) => write!(f, "not for this lease: {what}"),
            WireError::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<LeaseMessageError> for WireError {
    fn from(e: LeaseMessageError) -> Self {
        WireError::Invalid(e)
    }
}

fn lease_message(
    order_id: Option<Uuid>,
    request_id: Option<u64>,
    action: Action,
    payload: Option<LeasePayload>,
) -> Message {
    Message::new_order(
        order_id,
        request_id,
        None,
        action,
        payload.map(Payload::Lease),
    )
}

/// Seller → instance: a new offer. The instance echoes it with the order id, under `request_id`.
pub fn offer(request_id: u64, offer: LeaseOffer) -> Message {
    lease_message(
        None,
        Some(request_id),
        Action::LeaseOffer,
        Some(LeasePayload::Offer(offer)),
    )
}

/// Buyer → instance: take the offer `order_id` with `contract`. `binding` is the buyer's node
/// binding event (JSON); `node_address` is where the seller can reach the buyer's node.
pub fn take(
    order_id: Uuid,
    request_id: u64,
    contract: &LeaseContract,
    binding: String,
    node_address: Option<String>,
) -> Message {
    lease_message(
        Some(order_id),
        Some(request_id),
        Action::TakeLease,
        Some(LeasePayload::Take(LeaseTake {
            contract: contract.text(),
            binding,
            node_address,
        })),
    )
}

/// Seller → instance → buyer: the upfront hold invoice.
pub fn upfront_invoice(order_id: Uuid, contract: &LeaseContract, bolt11: String) -> Message {
    lease_message(
        Some(order_id),
        None,
        Action::LeaseInvoice,
        Some(LeasePayload::Invoice(LeaseInvoice {
            lease_id: contract.id(),
            invoice: bolt11,
        })),
    )
}

/// Seller → instance → buyer: the channel's funding outpoint, `<txid>:<index>` as nodes print it.
pub fn channel_opened(
    order_id: Uuid,
    contract: &LeaseContract,
    funding_outpoint: &str,
) -> Result<Message, WireError> {
    let (txid, index) = funding_outpoint
        .split_once(':')
        .ok_or(WireError::Shape("funding outpoint"))?;
    let channel = LeaseChannel {
        lease_id: contract.id(),
        funding_txid: txid.to_string(),
        output_index: index
            .parse()
            .map_err(|_| WireError::Shape("funding outpoint"))?,
    };
    channel.validate()?;
    Ok(lease_message(
        Some(order_id),
        None,
        Action::LeaseChannelOpened,
        Some(LeasePayload::Channel(channel)),
    ))
}

/// Seller → instance → buyer: the hold invoice is settled; the lease is active.
pub fn active(order_id: Uuid) -> Message {
    lease_message(Some(order_id), None, Action::LeaseActive, None)
}

/// Seller → instance → buyer: the invoice for the completion share, at the end of the lease.
pub fn completion_invoice(order_id: Uuid, contract: &LeaseContract, bolt11: String) -> Message {
    lease_message(
        Some(order_id),
        None,
        Action::LeaseCompletionInvoice,
        Some(LeasePayload::Invoice(LeaseInvoice {
            lease_id: contract.id(),
            invoice: bolt11,
        })),
    )
}

/// Buyer → seller, peer to peer: the preimage. Wrap it with [`seal_for_peer`] only.
pub fn preimage(order_id: Uuid, contract: &LeaseContract, preimage_hex: String) -> Message {
    lease_message(
        Some(order_id),
        None,
        Action::LeasePreimage,
        Some(LeasePayload::Preimage(LeasePreimage {
            lease_id: contract.id(),
            preimage: preimage_hex,
        })),
    )
}

/// Either party → instance: cancel the offer or the lease, before it is active.
pub fn cancel(order_id: Uuid, request_id: u64) -> Message {
    Message::new_order(Some(order_id), Some(request_id), None, Action::Cancel, None)
}

/// Who a message must come from to be acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The instance the lease runs on.
    Instance,
    /// The other party's trade key, peer to peer.
    Peer,
}

/// A lease message as received, with its own fields checked. Whether it belongs to a lease this
/// agent runs, and whether its sender may send it, is for the caller ([`sender_ok`],
/// [`check_take`], [`check_names`], [`check_preimage`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// The instance stored our offer under `order_id`.
    Offered {
        /// The order id the instance gave the offer.
        order_id: Uuid,
        /// The request id we sent the offer under.
        request_id: Option<u64>,
        /// The offer as the instance stored it.
        offer: LeaseOffer,
    },
    /// A buyer took our offer.
    Take {
        /// The offer taken.
        order_id: Uuid,
        /// The buyer's contract, binding and node address.
        take: LeaseTake,
    },
    /// The other party's trade key, for the peer-to-peer preimage.
    PeerKey {
        /// The lease's order id.
        order_id: Uuid,
        /// The other party's trade key.
        trade_key: PublicKey,
    },
    /// The seller's upfront hold invoice.
    UpfrontInvoice {
        /// The lease's order id.
        order_id: Uuid,
        /// The invoice and the lease it is for.
        invoice: LeaseInvoice,
    },
    /// The seller says the channel is opening.
    ChannelOpened {
        /// The lease's order id.
        order_id: Uuid,
        /// The funding outpoint the seller reports; the buyer's own node is the judge of it.
        channel: LeaseChannel,
    },
    /// The seller settled the hold invoice.
    Active {
        /// The lease's order id.
        order_id: Uuid,
    },
    /// The seller's invoice for the completion share.
    CompletionInvoice {
        /// The lease's order id.
        order_id: Uuid,
        /// The invoice and the lease it is for.
        invoice: LeaseInvoice,
    },
    /// The buyer's preimage.
    Preimage {
        /// The lease's order id.
        order_id: Uuid,
        /// The preimage and the lease it is for. Its `Debug` does not print the secret.
        preimage: LeasePreimage,
    },
    /// The offer or the lease was canceled.
    Canceled {
        /// The order canceled.
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

impl Incoming {
    /// Who must have sent this message. Only the preimage travels peer to peer.
    pub fn must_come_from(&self) -> Origin {
        match self {
            Incoming::Preimage { .. } => Origin::Peer,
            _ => Origin::Instance,
        }
    }

    /// The order the message is about.
    pub fn order_id(&self) -> Option<Uuid> {
        match self {
            Incoming::Offered { order_id, .. }
            | Incoming::Take { order_id, .. }
            | Incoming::PeerKey { order_id, .. }
            | Incoming::UpfrontInvoice { order_id, .. }
            | Incoming::ChannelOpened { order_id, .. }
            | Incoming::Active { order_id }
            | Incoming::CompletionInvoice { order_id, .. }
            | Incoming::Preimage { order_id, .. }
            | Incoming::Canceled { order_id } => Some(*order_id),
            Incoming::Refused { order_id, .. } => *order_id,
        }
    }
}

/// Reads a lease message and checks its own fields.
pub fn read(message: &Message) -> Result<Incoming, WireError> {
    let kind = message.get_inner_message_kind();
    if let Message::CantDo(_) = message {
        let reason = match &kind.payload {
            Some(Payload::CantDo(reason)) => reason.clone(),
            _ => None,
        };
        return Ok(Incoming::Refused {
            order_id: kind.id,
            request_id: kind.request_id,
            reason,
        });
    }
    let Message::Order(_) = message else {
        return Err(WireError::Shape("not an order message"));
    };
    let order_id = kind.id.ok_or(WireError::Shape("no order id"))?;
    let lease = match &kind.payload {
        Some(Payload::Lease(payload)) => Some(payload),
        _ => None,
    };
    Ok(match (&kind.action, lease, &kind.payload) {
        (Action::LeaseOffer, Some(LeasePayload::Offer(offer)), _) => {
            offer.validate()?;
            Incoming::Offered {
                order_id,
                request_id: kind.request_id,
                offer: offer.clone(),
            }
        }
        (Action::TakeLease, Some(LeasePayload::Take(take)), _) => {
            take.validate()?;
            Incoming::Take {
                order_id,
                take: take.clone(),
            }
        }
        (Action::TradePubkey, None, Some(Payload::Peer(peer))) => Incoming::PeerKey {
            order_id,
            trade_key: PublicKey::parse(&peer.pubkey)
                .map_err(|_| WireError::Shape("peer trade key"))?,
        },
        (Action::LeaseInvoice, Some(LeasePayload::Invoice(invoice)), _) => {
            invoice.validate()?;
            Incoming::UpfrontInvoice {
                order_id,
                invoice: invoice.clone(),
            }
        }
        (Action::LeaseChannelOpened, Some(LeasePayload::Channel(channel)), _) => {
            channel.validate()?;
            Incoming::ChannelOpened {
                order_id,
                channel: channel.clone(),
            }
        }
        (Action::LeaseActive, None, None) => Incoming::Active { order_id },
        (Action::LeaseCompletionInvoice, Some(LeasePayload::Invoice(invoice)), _) => {
            invoice.validate()?;
            Incoming::CompletionInvoice {
                order_id,
                invoice: invoice.clone(),
            }
        }
        (Action::LeasePreimage, Some(LeasePayload::Preimage(preimage)), _) => {
            preimage.validate()?;
            Incoming::Preimage {
                order_id,
                preimage: preimage.clone(),
            }
        }
        (Action::Canceled, None, None) => Incoming::Canceled { order_id },
        _ => return Err(WireError::Shape("not a lease message")),
    })
}

/// True when `sender`, the key that authored the event, may send `incoming`: the instance for
/// everything but the preimage, the lease peer's trade key for the preimage. With no peer key
/// known yet, no preimage is acceptable.
pub fn sender_ok(
    incoming: &Incoming,
    sender: &PublicKey,
    instance: &PublicKey,
    peer: Option<&PublicKey>,
) -> bool {
    match incoming.must_come_from() {
        Origin::Instance => sender == instance,
        Origin::Peer => peer == Some(sender),
    }
}

/// Seller: checks a take against the offer it answers, without relying on the instance. The
/// contract must be the offer at a capacity inside its range (the buyer cannot write its own
/// price), freshly dated, and the buyer's node must vouch for a Nostr key. Returns the contract
/// and that key.
pub fn check_take(
    offer: &LeaseOffer,
    take: &LeaseTake,
    now: u64,
) -> Result<(LeaseContract, PublicKey), WireError> {
    let contract = take.validate()?;
    offer.check_contract(&contract)?;
    if contract.created_at > now.saturating_add(MAX_CONTRACT_FUTURE_SECS) {
        return Err(WireError::NotOurs("the contract is dated in the future"));
    }
    if contract.created_at.saturating_add(MAX_CONTRACT_AGE_SECS) < now {
        return Err(WireError::NotOurs("the contract is stale"));
    }
    // The key that signed the binding event is the key the node is asked to vouch for.
    let buyer = Event::from_json(&take.binding)
        .map_err(|_| WireError::Shape("binding"))?
        .pubkey;
    take.check_binding(&contract, &buyer.to_hex(), now)?;
    Ok((contract, buyer))
}

/// Checks that a message's lease id is `contract`'s.
pub fn check_names(contract: &LeaseContract, lease_id: &str) -> Result<(), WireError> {
    if contract.id() == lease_id {
        Ok(())
    } else {
        Err(WireError::NotOurs("another lease id"))
    }
}

/// Seller: checks that a preimage is for `contract` and opens its payment hash. Returns the
/// preimage (64 hex) to settle the hold invoice with.
pub fn check_preimage<'a>(
    contract: &LeaseContract,
    preimage: &'a LeasePreimage,
) -> Result<&'a str, WireError> {
    preimage.validate()?;
    check_names(contract, &preimage.lease_id)?;
    if !preimage.opens(contract) {
        return Err(WireError::NotOurs(
            "the preimage does not open the payment hash",
        ));
    }
    Ok(&preimage.preimage)
}

fn seal(
    message: &Message,
    identity: &Keys,
    trade: &Keys,
    receiver: PublicKey,
    pow: u8,
) -> Result<Event, WireError> {
    let options = WrapOptions {
        pow,
        expiration: None,
        signed: true,
    };
    wrap_message_nip44(message, identity, trade, receiver, options)
        .map_err(|e| WireError::Transport(e.to_string()))
}

/// Wraps a message for an instance: authored by the trade key, with proof of the identity key
/// inside the ciphertext, and `pow` bits of proof of work if the instance asks for it. A preimage
/// is refused: it never goes to an instance.
pub fn seal_for_instance(
    message: &Message,
    identity: &Keys,
    trade: &Keys,
    instance: PublicKey,
    pow: u8,
) -> Result<Event, WireError> {
    if message.get_inner_message_kind().action == Action::LeasePreimage {
        return Err(WireError::Shape("a preimage never goes to an instance"));
    }
    seal(message, identity, trade, instance, pow)
}

/// Wraps a message for the lease peer's trade key, from our trade key alone: the peer learns no
/// identity key from it.
pub fn seal_for_peer(message: &Message, trade: &Keys, peer: PublicKey) -> Result<Event, WireError> {
    seal(message, trade, trade, peer, 0)
}

/// Opens an event addressed to our trade key. `Ok(None)` when it is not for us. On success, the
/// message with its own fields checked, and the key that authored the event: pass both to
/// [`sender_ok`] before acting on anything.
pub fn open(event: &Event, trade: &Keys) -> Result<Option<(Incoming, PublicKey)>, WireError> {
    let Some(opened) =
        unwrap_message_nip44(event, trade).map_err(|e| WireError::Transport(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(Some((read(&opened.message)?, opened.sender)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::node_binding::{
        Action as BindingAction, Network, SignedStatement, Statement, sign_ln_message,
    };
    use mostro_core::prelude::Peer;
    use nostr_sdk::prelude::FinalizeEvent;
    use secp256k1::{Secp256k1, SecretKey};

    const NOW: u64 = 1_800_000_000;

    /// A node runner: an identity key, a trade key and a node key.
    struct Party {
        identity: Keys,
        trade: Keys,
        node: SecretKey,
    }

    impl Party {
        fn new() -> Self {
            let node = SecretKey::from_slice(&Keys::generate().secret_key().to_secret_bytes())
                .expect("key");
            Party {
                identity: Keys::generate(),
                trade: Keys::generate(),
                node,
            }
        }

        fn node_key(&self) -> secp256k1::PublicKey {
            secp256k1::PublicKey::from_secret_key(&Secp256k1::signing_only(), &self.node)
        }

        /// This node's binding of `identity`.
        fn binding_of(&self, identity: &Keys) -> String {
            let statement = Statement::new(
                BindingAction::Bind,
                Network::Regtest,
                &identity.public_key().to_hex(),
                self.node_key(),
                NOW - 60,
            )
            .expect("statement");
            let node_signature = sign_ln_message(statement.text().as_bytes(), &self.node);
            SignedStatement {
                statement,
                node_signature,
            }
            .to_event_builder()
            .finalize(identity)
            .expect("event")
            .as_json()
        }

        fn offer(&self) -> LeaseOffer {
            LeaseOffer {
                network: Network::Regtest,
                seller_node: self.node_key().to_string(),
                binding: self.binding_of(&self.identity),
                min_capacity_sat: 1_000_000,
                max_capacity_sat: 5_000_000,
                duration_blocks: 4_320,
                fee_base_sat: 1_000,
                fee_ppm: 2_500,
                upfront_percent: 50,
                min_confirmations: 3,
                announce: true,
                expires_at: NOW + 86_400,
            }
        }

        fn contract(&self, offer: &LeaseOffer, preimage: &[u8; 32]) -> LeaseContract {
            use sha2::{Digest, Sha256};
            let hash: String = Sha256::digest(preimage)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            LeaseContract {
                network: offer.network,
                seller_node: offer.seller_node_key().expect("node key"),
                buyer_node: self.node_key(),
                terms: offer.terms_for(2_000_000).expect("terms"),
                payment_hash: hash,
                created_at: NOW,
            }
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn bolt11() -> String {
        format!("lnbcrt30u1{}", "q".repeat(200))
    }

    /// Through JSON, as it travels, then read.
    fn sent(message: &Message) -> Result<Incoming, WireError> {
        assert!(message.verify(), "we only build well-formed messages");
        let json = message.as_json().expect("json");
        read(&Message::from_json(&json).expect("parse"))
    }

    #[test]
    fn every_message_we_build_reads_back_as_what_it_is() {
        let (seller, buyer) = (Party::new(), Party::new());
        let offer = seller.offer();
        let secret = [7u8; 32];
        let contract = buyer.contract(&offer, &secret);
        let id = Uuid::new_v4();
        let binding = buyer.binding_of(&buyer.identity);

        // A new offer has no id yet, so it is not something an agent receives; its echo is.
        let new_offer = super::offer(5, offer.clone());
        assert!(new_offer.verify());
        assert_eq!(sent(&new_offer), Err(WireError::Shape("no order id")));
        let echo = lease_message(
            Some(id),
            Some(5),
            Action::LeaseOffer,
            Some(LeasePayload::Offer(offer.clone())),
        );
        assert_eq!(
            sent(&echo),
            Ok(Incoming::Offered {
                order_id: id,
                request_id: Some(5),
                offer: offer.clone()
            })
        );

        let message = take(
            id,
            6,
            &contract,
            binding.clone(),
            Some("203.0.113.7:9735".into()),
        );
        let Ok(Incoming::Take { order_id, take }) = sent(&message) else {
            panic!("not a take");
        };
        assert_eq!(order_id, id);
        assert_eq!(take.contract, contract.text());
        assert_eq!(take.binding, binding);

        let invoice = LeaseInvoice {
            lease_id: contract.id(),
            invoice: bolt11(),
        };
        assert_eq!(
            sent(&upfront_invoice(id, &contract, bolt11())),
            Ok(Incoming::UpfrontInvoice {
                order_id: id,
                invoice: invoice.clone()
            })
        );
        assert_eq!(
            sent(&completion_invoice(id, &contract, bolt11())),
            Ok(Incoming::CompletionInvoice {
                order_id: id,
                invoice
            })
        );

        let outpoint = format!("{}:1", "cd".repeat(32));
        let message = channel_opened(id, &contract, &outpoint).expect("outpoint");
        let Ok(Incoming::ChannelOpened { channel, .. }) = sent(&message) else {
            panic!("not a channel report");
        };
        assert_eq!(channel.channel_point(), outpoint);
        let (no_index, bad_index) = ("cd".repeat(32), format!("{}:x", "cd".repeat(32)));
        for bad in ["", "cd:1", no_index.as_str(), bad_index.as_str()] {
            assert!(channel_opened(id, &contract, bad).is_err(), "{bad}");
        }

        assert_eq!(sent(&active(id)), Ok(Incoming::Active { order_id: id }));

        let message = preimage(id, &contract, hex(&secret));
        let Ok(Incoming::Preimage { preimage, .. }) = sent(&message) else {
            panic!("not a preimage");
        };
        assert_eq!(
            check_preimage(&contract, &preimage),
            Ok(hex(&secret).as_str())
        );

        // What the instance sends that we never build.
        let canceled = Message::new_order(Some(id), None, None, Action::Canceled, None);
        assert_eq!(sent(&canceled), Ok(Incoming::Canceled { order_id: id }));
        let peer = Message::new_order(
            Some(id),
            None,
            None,
            Action::TradePubkey,
            Some(Payload::Peer(Peer::new(
                seller.trade.public_key().to_hex(),
                None,
            ))),
        );
        assert_eq!(
            sent(&peer),
            Ok(Incoming::PeerKey {
                order_id: id,
                trade_key: seller.trade.public_key()
            })
        );
        let refusal = Message::cant_do(
            Some(id),
            Some(6),
            Some(Payload::CantDo(Some(CantDoReason::NotAllowedByStatus))),
        );
        assert_eq!(
            read(&refusal),
            Ok(Incoming::Refused {
                order_id: Some(id),
                request_id: Some(6),
                reason: Some(CantDoReason::NotAllowedByStatus)
            })
        );

        // Our own cancel is for the instance, not something we act on if it comes back.
        assert!(cancel(id, 9).verify());
        assert_eq!(
            read(&cancel(id, 9)),
            Err(WireError::Shape("not a lease message"))
        );
    }

    #[test]
    fn a_message_with_the_wrong_payload_or_a_bad_field_is_refused() {
        let (seller, buyer) = (Party::new(), Party::new());
        let offer = seller.offer();
        let contract = buyer.contract(&offer, &[7u8; 32]);
        let id = Uuid::new_v4();

        // An invoice action carrying a channel report; a fiat action; an active notice with a
        // payload.
        let report = LeasePayload::Channel(LeaseChannel {
            lease_id: contract.id(),
            funding_txid: "cd".repeat(32),
            output_index: 0,
        });
        for message in [
            lease_message(Some(id), None, Action::LeaseInvoice, Some(report.clone())),
            lease_message(Some(id), None, Action::LeaseActive, Some(report)),
            Message::new_order(Some(id), None, None, Action::FiatSent, None),
        ] {
            assert_eq!(read(&message), Err(WireError::Shape("not a lease message")));
        }

        let junk = lease_message(
            Some(id),
            None,
            Action::LeaseInvoice,
            Some(LeasePayload::Invoice(LeaseInvoice {
                lease_id: contract.id(),
                invoice: "not an invoice".into(),
            })),
        );
        assert!(matches!(read(&junk), Err(WireError::Invalid(_))));
        let short = preimage(id, &contract, "5a".repeat(31));
        assert!(matches!(read(&short), Err(WireError::Invalid(_))));
    }

    #[test]
    fn only_the_peer_may_send_the_preimage_and_only_the_instance_the_rest() {
        let (seller, buyer) = (Party::new(), Party::new());
        let offer = seller.offer();
        let secret = [7u8; 32];
        let contract = buyer.contract(&offer, &secret);
        let id = Uuid::new_v4();
        let instance = Keys::generate().public_key();
        let peer = buyer.trade.public_key();
        let stranger = Keys::generate().public_key();

        let secret_message = read(&preimage(id, &contract, hex(&secret))).expect("preimage");
        assert_eq!(secret_message.must_come_from(), Origin::Peer);
        assert!(sender_ok(&secret_message, &peer, &instance, Some(&peer)));
        assert!(!sender_ok(
            &secret_message,
            &instance,
            &instance,
            Some(&peer)
        ));
        assert!(!sender_ok(
            &secret_message,
            &stranger,
            &instance,
            Some(&peer)
        ));
        assert!(!sender_ok(&secret_message, &peer, &instance, None));

        let others = [
            read(&upfront_invoice(id, &contract, bolt11())).expect("invoice"),
            read(&active(id)).expect("active"),
            Incoming::Canceled { order_id: id },
            Incoming::Refused {
                order_id: Some(id),
                request_id: None,
                reason: None,
            },
        ];
        for message in others {
            assert_eq!(message.must_come_from(), Origin::Instance);
            assert_eq!(message.order_id(), Some(id));
            assert!(sender_ok(&message, &instance, &instance, Some(&peer)));
            assert!(!sender_ok(&message, &peer, &instance, Some(&peer)));
            assert!(!sender_ok(&message, &stranger, &instance, None));
        }
    }

    #[test]
    fn a_seller_checks_a_take_itself() {
        let (seller, buyer) = (Party::new(), Party::new());
        let offer = seller.offer();
        let good = buyer.contract(&offer, &[7u8; 32]);
        let with = |contract: &LeaseContract, binding: String| LeaseTake {
            contract: contract.text(),
            binding,
            node_address: None,
        };
        let binding = buyer.binding_of(&buyer.identity);

        let (contract, vouched_for) =
            check_take(&offer, &with(&good, binding.clone()), NOW).expect("a good take");
        assert_eq!(contract, good);
        assert_eq!(vouched_for, buyer.identity.public_key());

        // The buyer cannot write its own price or capacity.
        let mut cheaper = good.clone();
        cheaper.terms.fee_ppm = 1;
        let mut too_big = good.clone();
        too_big.terms.capacity_sat = 5_000_001;
        for contract in [cheaper, too_big] {
            let result = check_take(&offer, &with(&contract, binding.clone()), NOW);
            assert!(matches!(result, Err(WireError::Invalid(_))), "{result:?}");
        }
        // Nor reuse an old contract, nor post-date one.
        assert_eq!(
            check_take(
                &offer,
                &with(&good, binding.clone()),
                NOW + MAX_CONTRACT_AGE_SECS + 1
            ),
            Err(WireError::NotOurs("the contract is stale"))
        );
        assert_eq!(
            check_take(
                &offer,
                &with(&good, binding.clone()),
                NOW - MAX_CONTRACT_FUTURE_SECS - 1
            ),
            Err(WireError::NotOurs("the contract is dated in the future"))
        );
        // A contract for another seller's offer.
        let other_offer = Party::new().offer();
        let result = check_take(&other_offer, &with(&good, binding.clone()), NOW);
        assert!(matches!(result, Err(WireError::Invalid(_))), "{result:?}");
        // The binding must be the contract's buyer node's.
        let stranger = Party::new();
        let result = check_take(
            &offer,
            &with(&good, stranger.binding_of(&stranger.identity)),
            NOW,
        );
        assert!(matches!(result, Err(WireError::Invalid(_))), "{result:?}");
        let result = check_take(&offer, &with(&good, "{}".into()), NOW);
        assert_eq!(result, Err(WireError::Shape("binding")));
    }

    #[test]
    fn a_preimage_must_be_for_this_lease_and_open_its_hash() {
        let (seller, buyer) = (Party::new(), Party::new());
        let offer = seller.offer();
        let secret = [7u8; 32];
        let contract = buyer.contract(&offer, &secret);
        let other = buyer.contract(&offer, &[8u8; 32]);

        let right = LeasePreimage {
            lease_id: contract.id(),
            preimage: hex(&secret),
        };
        assert_eq!(check_preimage(&contract, &right), Ok(hex(&secret).as_str()));
        assert_eq!(
            check_preimage(&other, &right),
            Err(WireError::NotOurs("another lease id"))
        );
        let wrong = LeasePreimage {
            lease_id: contract.id(),
            preimage: hex(&[8u8; 32]),
        };
        assert_eq!(
            check_preimage(&contract, &wrong),
            Err(WireError::NotOurs(
                "the preimage does not open the payment hash"
            ))
        );
        assert!(check_names(&contract, &contract.id()).is_ok());
        assert!(check_names(&contract, &other.id()).is_err());
    }

    #[test]
    fn sealed_messages_open_only_for_their_receiver_and_name_their_sender() {
        let (seller, buyer) = (Party::new(), Party::new());
        let instance = Keys::generate();
        let offer = seller.offer();
        let secret = [7u8; 32];
        let contract = buyer.contract(&offer, &secret);
        let id = Uuid::new_v4();

        // The instance relays the seller's invoice to the buyer's trade key.
        let relayed = seal(
            &upfront_invoice(id, &contract, bolt11()),
            &instance,
            &instance,
            buyer.trade.public_key(),
            0,
        )
        .expect("seal");
        let (incoming, sender) = open(&relayed, &buyer.trade)
            .expect("open")
            .expect("for the buyer");
        assert!(matches!(incoming, Incoming::UpfrontInvoice { .. }));
        assert_eq!(sender, instance.public_key());
        assert!(sender_ok(&incoming, &sender, &instance.public_key(), None));
        assert_eq!(
            open(&relayed, &seller.trade),
            Ok(None),
            "not for the seller"
        );

        // The buyer releases the preimage to the seller's trade key; nothing in it names the
        // buyer's identity key.
        let release = preimage(id, &contract, hex(&secret));
        let direct =
            seal_for_peer(&release, &buyer.trade, seller.trade.public_key()).expect("seal");
        assert_eq!(direct.pubkey, buyer.trade.public_key());
        let (incoming, sender) = open(&direct, &seller.trade)
            .expect("open")
            .expect("for the seller");
        assert_eq!(sender, buyer.trade.public_key());
        assert!(sender_ok(
            &incoming,
            &sender,
            &instance.public_key(),
            Some(&buyer.trade.public_key())
        ));
        let Incoming::Preimage { preimage, .. } = incoming else {
            panic!("not a preimage");
        };
        assert_eq!(
            check_preimage(&contract, &preimage),
            Ok(hex(&secret).as_str())
        );
        assert_eq!(
            open(&direct, &instance),
            Ok(None),
            "the instance cannot read it"
        );

        // A preimage is never wrapped for an instance.
        assert_eq!(
            seal_for_instance(
                &release,
                &buyer.identity,
                &buyer.trade,
                instance.public_key(),
                0
            ),
            Err(WireError::Shape("a preimage never goes to an instance"))
        );
        // Anything else is, authored by the trade key.
        let to_instance = seal_for_instance(
            &cancel(id, 1),
            &buyer.identity,
            &buyer.trade,
            instance.public_key(),
            0,
        )
        .expect("seal");
        assert_eq!(to_instance.pubkey, buyer.trade.public_key());
    }
}
