use mostro_core::node_binding::Network;
use nostr_sdk::prelude::{FinalizeEvent, Keys};
use secp256k1::{Secp256k1, SecretKey};

use super::*;

fn node(n: u8) -> PublicKey {
    let mut b = [0u8; 32];
    b[31] = n;
    let sk = SecretKey::from_slice(&b).expect("key");
    PublicKey::from_secret_key(&Secp256k1::signing_only(), &sk)
}

/// A lease between seller `s` and buyer `b`, numbered `n`, funded at `height`.
fn lease(n: u8, s: u8, b: u8, result: OutcomeResult, height: Option<u32>) -> Outcome {
    Outcome {
        deal: Deal::Lease,
        subject: format!("{n:02x}").repeat(32),
        network: Network::Regtest,
        nodes: [node(s), node(b)],
        result,
        closer: None,
        funding_height: height,
        close_height: None,
        outpoint: None,
        instance: None,
    }
}

fn closed_by(mut outcome: Outcome, who: Role) -> Outcome {
    outcome.result = OutcomeResult::ClosedEarly;
    outcome.closer = Some(ClosedBy::Party(who));
    outcome
}

fn by(outcome: Outcome, role: Role) -> Stated {
    Stated {
        outcome,
        author: Author::Party(role.into()),
    }
}

fn edge(n: u8, opener: u8, receiver: u8, result: OutcomeResult) -> Outcome {
    Outcome {
        deal: Deal::Ring,
        nodes: [node(opener), node(receiver)],
        result,
        ..lease(n, opener, receiver, result, None)
    }
}

#[test]
fn a_lease_counts_as_kept_only_when_both_parties_say_so() {
    let kept = lease(1, 1, 2, OutcomeResult::Kept, Some(100));
    let alone = summarize(&node(1), &[by(kept.clone(), Role::Seller)]);
    assert_eq!(alone.kept, 0);
    assert_eq!(alone.subjects, 1);
    let both = [by(kept.clone(), Role::Seller), by(kept, Role::Buyer)];
    for n in [1, 2] {
        let record = summarize(&node(n), &both);
        assert_eq!((record.kept, record.counterparties), (1, 1), "node {n}");
    }
    // A third node is not in it.
    assert_eq!(summarize(&node(3), &both), Record::default());
}

#[test]
fn closed_early_counts_against_a_node_only_on_its_own_word_or_both() {
    let base = lease(1, 1, 2, OutcomeResult::Kept, Some(100));
    // The buyer says the seller closed; the seller says nothing: disputed, not counted.
    let claim = [by(closed_by(base.clone(), Role::Seller), Role::Buyer)];
    let record = summarize(&node(1), &claim);
    assert_eq!((record.closed_early, record.disputed), (0, 1));
    // The seller admits it.
    let admitted = [by(closed_by(base.clone(), Role::Seller), Role::Seller)];
    assert_eq!(summarize(&node(1), &admitted).closed_early, 1);
    // Both name the seller.
    let both = [
        by(closed_by(base.clone(), Role::Seller), Role::Seller),
        by(closed_by(base.clone(), Role::Seller), Role::Buyer),
    ];
    assert_eq!(summarize(&node(1), &both).closed_early, 1);
    assert_eq!(summarize(&node(2), &both).closed_early, 0);
    // Each blames the other: disputed for both.
    let blame = [
        by(closed_by(base.clone(), Role::Buyer), Role::Seller),
        by(closed_by(base, Role::Seller), Role::Buyer),
    ];
    for n in [1, 2] {
        let record = summarize(&node(n), &blame);
        assert_eq!((record.closed_early, record.disputed), (0, 1), "node {n}");
    }
}

#[test]
fn an_author_that_contradicts_itself_counts_for_nothing() {
    let kept = lease(1, 1, 2, OutcomeResult::Kept, Some(100));
    let stated = [
        by(kept.clone(), Role::Seller),
        by(kept.clone(), Role::Buyer),
        by(closed_by(kept, Role::Seller), Role::Buyer),
    ];
    assert_eq!(summarize(&node(1), &stated).kept, 0);
}

#[test]
fn two_nodes_count_one_kept_lease_per_window_and_negatives_are_kept_apart() {
    let mut stated = Vec::new();
    // Ten leases between nodes 1 and 2 within one window of blocks, and one in the next.
    for n in 1..=10 {
        let kept = lease(n, 1, 2, OutcomeResult::Kept, Some(100 + u32::from(n)));
        stated.push(by(kept.clone(), Role::Seller));
        stated.push(by(kept, Role::Buyer));
    }
    let later = lease(
        11,
        1,
        2,
        OutcomeResult::Kept,
        Some(100 + PAIR_WINDOW_BLOCKS),
    );
    stated.push(by(later.clone(), Role::Seller));
    stated.push(by(later, Role::Buyer));
    // One lease with node 3, closed early by node 1, admitted.
    stated.push(by(
        closed_by(lease(12, 1, 3, OutcomeResult::Kept, Some(50)), Role::Seller),
        Role::Seller,
    ));
    let record = summarize(&node(1), &stated);
    assert_eq!(record.kept, 2);
    assert_eq!(record.closed_early, 1);
    assert_eq!(record.counterparties, 2);
    assert_eq!(record.subjects, 12);
}

#[test]
fn a_ring_edge_counts_for_its_opener_on_the_receivers_word_or_an_instances() {
    let opened = edge(1, 1, 2, OutcomeResult::Opened);
    let record = summarize(&node(1), &[by(opened.clone(), Role::Receiver)]);
    assert_eq!(record.opened, 1);
    // Not for the receiver: it owed another edge.
    assert_eq!(summarize(&node(2), &[by(opened, Role::Receiver)]).opened, 0);
    // Missing on the receiver's word alone: disputed (with a bond it would gain from it).
    let missing = edge(2, 1, 2, OutcomeResult::Missing);
    let record = summarize(&node(1), &[by(missing.clone(), Role::Receiver)]);
    assert_eq!((record.missing, record.disputed), (0, 1));
    let instance = Stated {
        outcome: missing,
        author: Author::Instance,
    };
    assert_eq!(summarize(&node(1), &[instance]).missing, 1);
}

#[test]
fn only_outcomes_from_a_node_of_the_deal_or_a_trusted_instance_are_read() {
    let house = Keys::generate();
    let stranger = Keys::generate();
    let outcome = edge(1, 1, 2, OutcomeResult::Missing);
    let signed = |keys: &Keys| {
        outcome
            .to_event_builder("")
            .expect("valid")
            .finalize(keys)
            .expect("signs")
    };
    let events = [signed(&house), signed(&stranger)];
    let read = read(&events, &[house.public_key()], 1_794_800_000);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].author, Author::Instance);
    assert_eq!(read[0].outcome, outcome);
}
