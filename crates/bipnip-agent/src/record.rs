//! A node's record, counted from signed outcomes (`docs/nips/liquidity-outcomes.md`) the way this
//! agent reads them. Pure: the outcomes come in already checked ([`read`]), with who wrote each.
//!
//! The reading rules, from the draft:
//!
//! - one outcome per author and subject; an author that wrote two different ones about the same
//!   subject counts for nothing on it;
//! - a lease is `kept` for the record only when both parties say so; a ring edge is `opened` when
//!   its receiver says so (it would gain from saying otherwise when there was a bond) or a
//!   trusted instance does, and `missing` only when a trusted instance says so;
//! - `closed-early` counts against a node only when the node itself says it closed, or both
//!   parties name it as the closer; otherwise the lease shows as disputed;
//! - at most one `kept` or `opened` per pair of nodes per 2016 blocks (by funding height, or per
//!   subject where none is given), so two nodes of one operator gain little by leasing to each
//!   other; the number of different counterparties is shown beside the counts;
//! - negatives are counted, never folded into a ratio.
//!
//! What it does not do yet: confirm an outcome from this agent's own node's graph (the draft's
//! third way).

use std::collections::{BTreeMap, BTreeSet};

use mostro_core::outcome::{ClosedBy, Deal, Outcome, OutcomeResult, Role};
use nostr_sdk::prelude::{Event, PublicKey as NostrKey};
use secp256k1::PublicKey;
use serde::Serialize;

/// Blocks in which at most one positive outcome counts per pair of nodes.
pub const PAIR_WINDOW_BLOCKS: u32 = 2016;

/// Who wrote an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Author {
    /// The node with this role, by the binding in the event.
    Party(RoleKey),
    /// An instance this agent trusts.
    Instance,
}

/// [`Role`] with an order, to key maps by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RoleKey {
    /// A lease's seller.
    Seller,
    /// A lease's buyer.
    Buyer,
    /// A ring edge's opener.
    Opener,
    /// A ring edge's receiver.
    Receiver,
}

impl From<Role> for RoleKey {
    fn from(role: Role) -> Self {
        match role {
            Role::Seller => RoleKey::Seller,
            Role::Buyer => RoleKey::Buyer,
            Role::Opener => RoleKey::Opener,
            Role::Receiver => RoleKey::Receiver,
        }
    }
}

/// An outcome that passed its checks, and who wrote it.
#[derive(Debug, Clone)]
pub struct Stated {
    /// The outcome.
    pub outcome: Outcome,
    /// Its author.
    pub author: Author,
}

/// Reads `events` as outcomes: each must parse strictly, and be written by one of its two
/// nodes (by the binding in its content) or by one of `trusted` instances. Others are passed
/// over.
pub fn read(events: &[Event], trusted: &[NostrKey], now: u64) -> Vec<Stated> {
    events
        .iter()
        .filter_map(|event| {
            let outcome = Outcome::from_event(event).ok()?;
            let author = if trusted.contains(&event.pubkey) {
                Author::Instance
            } else {
                Author::Party(outcome.author_node(event, now).ok()?.into())
            };
            Some(Stated { outcome, author })
        })
        .collect()
}

/// A node's record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Record {
    /// Leases the node sold or bought that both parties say were kept.
    pub kept: u32,
    /// Ring edges the node owed and that were opened.
    pub opened: u32,
    /// Ring edges the node owed that a trusted instance says were missing.
    pub missing: u32,
    /// Leases the node closed early, by its own word or both parties'.
    pub closed_early: u32,
    /// Leases about the node whose parties disagree, or where only the other party says the
    /// node closed early.
    pub disputed: u32,
    /// Leases the node bought whose completion share both parties say went unpaid.
    pub unpaid: u32,
    /// Different nodes the counted outcomes are with.
    pub counterparties: u32,
    /// Subjects about the node read, counted or not.
    pub subjects: u32,
}

/// A subject: the lease or ring id, and the two nodes (a ring has one subject per edge).
type Subject = (String, [PublicKey; 2]);

/// The roles of a deal, in the order its nodes are written.
fn roles(deal: Deal) -> [Role; 2] {
    match deal {
        Deal::Lease => [Role::Seller, Role::Buyer],
        Deal::Ring => [Role::Opener, Role::Receiver],
    }
}

/// What is counted so far.
#[derive(Default)]
struct Tally {
    record: Record,
    /// Positive outcomes counted: the pair of nodes and the window (or subject).
    positives: BTreeSet<(PublicKey, PublicKey, String)>,
    counterparties: BTreeSet<PublicKey>,
}

impl Tally {
    /// A kept lease or an opened edge with `other`, counted once per pair and window.
    fn positive(
        &mut self,
        deal: Deal,
        pair: (PublicKey, PublicKey),
        window: String,
        other: PublicKey,
    ) {
        if self.positives.insert((pair.0, pair.1, window)) {
            match deal {
                Deal::Lease => self.record.kept += 1,
                Deal::Ring => self.record.opened += 1,
            }
            self.counterparties.insert(other);
        }
    }

    fn negative(&mut self, field: fn(&mut Record) -> &mut u32, other: PublicKey) {
        *field(&mut self.record) += 1;
        self.counterparties.insert(other);
    }
}

/// `node`'s record from `stated`.
pub fn summarize(node: &PublicKey, stated: &[Stated]) -> Record {
    // One outcome per author and subject; an author that contradicts itself is dropped.
    let mut by_subject: BTreeMap<Subject, BTreeMap<Author, Option<Outcome>>> = BTreeMap::new();
    for statement in stated
        .iter()
        .filter(|statement| statement.outcome.nodes.contains(node))
    {
        let outcome = &statement.outcome;
        by_subject
            .entry((outcome.subject.clone(), outcome.nodes))
            .or_default()
            .entry(statement.author)
            .and_modify(|kept| {
                if kept.as_ref().is_some_and(|known| !same(known, outcome)) {
                    *kept = None;
                }
            })
            .or_insert_with(|| Some(outcome.clone()));
    }
    let mut tally = Tally::default();
    tally.record.subjects = u32::try_from(by_subject.len()).unwrap_or(u32::MAX);
    for ((subject, nodes), authors) in &by_subject {
        let said: BTreeMap<Author, &Outcome> = authors
            .iter()
            .filter_map(|(author, outcome)| outcome.as_ref().map(|outcome| (*author, outcome)))
            .collect();
        let Some(first) = said.values().next() else {
            continue;
        };
        let Some(ours) = roles(first.deal)
            .into_iter()
            .find(|role| first.node(*role) == Some(*node))
        else {
            continue;
        };
        let other = if nodes[0] == *node {
            nodes[1]
        } else {
            nodes[0]
        };
        let pair = if nodes[0] < nodes[1] {
            (nodes[0], nodes[1])
        } else {
            (nodes[1], nodes[0])
        };
        let window = first
            .funding_height
            .map(|h| format!("h{}", h / PAIR_WINDOW_BLOCKS))
            .unwrap_or_else(|| subject.clone());
        let party = |role: Role| said.get(&Author::Party(role.into())).copied();
        let instance = said.get(&Author::Instance).copied();
        match first.deal {
            Deal::Lease => {
                let own_word = party(ours).is_some_and(|o| admits(o, ours));
                match (party(Role::Seller), party(Role::Buyer)) {
                    (Some(s), Some(b)) if s.result == b.result => match s.result {
                        OutcomeResult::Kept => tally.positive(Deal::Lease, pair, window, other),
                        OutcomeResult::ClosedEarly => {
                            let both_name_us = s.closer == Some(ClosedBy::Party(ours))
                                && b.closer == Some(ClosedBy::Party(ours));
                            if both_name_us || own_word {
                                tally.negative(|r| &mut r.closed_early, other);
                            } else if [s.closer, b.closer].contains(&Some(ClosedBy::Party(ours))) {
                                tally.record.disputed += 1;
                            }
                        }
                        OutcomeResult::Unpaid if ours == Role::Buyer => {
                            tally.negative(|r| &mut r.unpaid, other);
                        }
                        _ => {}
                    },
                    _ if own_word => tally.negative(|r| &mut r.closed_early, other),
                    // One party alone, or two that disagree: nothing is confirmed; a word
                    // against the node is shown as disputed.
                    _ => {
                        if said.values().any(|o| o.result != OutcomeResult::Kept) {
                            tally.record.disputed += 1;
                        }
                    }
                }
            }
            Deal::Ring => {
                // Only the edge the node owed counts for it.
                if ours != Role::Opener {
                    continue;
                }
                let says = |o: Option<&Outcome>, result: OutcomeResult| {
                    o.is_some_and(|o| o.result == result)
                };
                let receiver = party(Role::Receiver);
                if says(receiver, OutcomeResult::Opened) || says(instance, OutcomeResult::Opened) {
                    tally.positive(Deal::Ring, pair, window, other);
                } else if says(instance, OutcomeResult::Missing) {
                    tally.negative(|r| &mut r.missing, other);
                } else if says(receiver, OutcomeResult::Missing) {
                    tally.record.disputed += 1;
                }
            }
        }
    }
    tally.record.counterparties = u32::try_from(tally.counterparties.len()).unwrap_or(u32::MAX);
    tally.record
}

/// True when `outcome`, written by the node with `role`, says that node closed early.
fn admits(outcome: &Outcome, role: Role) -> bool {
    outcome.result == OutcomeResult::ClosedEarly && outcome.closer == Some(ClosedBy::Party(role))
}

/// Two outcomes say the same thing (the parts a reader counts).
fn same(a: &Outcome, b: &Outcome) -> bool {
    a.result == b.result && a.closer == b.closer && a.deal == b.deal
}

#[cfg(test)]
mod tests;
