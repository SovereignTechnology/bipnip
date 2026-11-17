//! Ring runs on the node simulator: several nodes share one table of channels, each member has a
//! run, and the test plays the instance.

use std::sync::{Arc, Mutex};

use bipnip_ln::{LightningBackend, OpenRequest};
use mostro_core::node_binding::{
    Action as BindingAction, Network, SignedStatement, Statement, sign_ln_message,
};
use mostro_core::prelude::{Action, CantDoReason, Message, Payload};
use mostro_core::ring::{Ring, RingTerms};
use mostro_core::ring_message::{
    RingClosed, RingCommit, RingCommitment, RingEdge, RingMessageError, RingOffer, RingOpen,
    RingPayload, RingRoster, RingSeat,
};
use nostr_sdk::prelude::{FinalizeEvent, Keys};
use uuid::Uuid;

use crate::lease::tests::{NOW, Node, World, confirm, secret};
use crate::lease_wire;
use crate::ring::run::{JoinRequest, RingRun, RingRunError, RingState};
use crate::ring_wire::{self, RingIncoming, RingWireError};

const CAPACITY: u64 = 2_000_000;

/// A node runner: a node and the keys its agent uses.
struct Member {
    node: Node,
    identity: Keys,
    trade: Keys,
}

impl Member {
    fn node_key(&self) -> secp256k1::PublicKey {
        self.node.id().parse().expect("node key")
    }

    /// This node's binding of `identity`.
    fn binding_of(&self, identity: &Keys) -> String {
        let secret =
            secp256k1::SecretKey::from_slice(&self.node.secret.secret_bytes()).expect("key");
        let statement = Statement::new(
            BindingAction::Bind,
            Network::Regtest,
            &identity.public_key().to_hex(),
            self.node_key(),
            NOW - 60,
        )
        .expect("statement");
        let node_signature = sign_ln_message(statement.text().as_bytes(), &secret);
        SignedStatement {
            statement,
            node_signature,
        }
        .to_event_builder()
        .finalize(identity)
        .expect("event")
        .as_json()
    }

    fn seat(&self) -> RingSeat {
        RingSeat {
            node: self.node.id(),
            binding: self.binding_of(&self.identity),
            node_address: Some("203.0.113.7:9735".into()),
        }
    }
}

/// Some node runners around one instance, played by the test. Member 0 creates the ring.
struct Table {
    world: Arc<Mutex<World>>,
    members: Vec<Member>,
    instance: Keys,
    order_id: Uuid,
}

impl Table {
    fn new(size: u8) -> Self {
        let world = Arc::new(Mutex::new(World::default()));
        let members = (1..=size)
            .map(|n| Member {
                node: Node {
                    world: world.clone(),
                    secret: secret(n),
                    network: "regtest",
                },
                identity: Keys::generate(),
                trade: Keys::generate(),
            })
            .collect();
        Table {
            world,
            members,
            instance: Keys::generate(),
            order_id: Uuid::new_v4(),
        }
    }

    fn terms(&self) -> RingTerms {
        RingTerms {
            network: Network::Regtest,
            capacity_sat: CAPACITY,
            announce: true,
            open_window_secs: 48 * 3_600,
        }
    }

    fn offer(&self, slots: u8) -> RingOffer {
        RingOffer {
            network: Network::Regtest,
            capacity_sat: CAPACITY,
            slots,
            announce: true,
            open_window_secs: 48 * 3_600,
            expires_at: NOW + 86_400,
            seat: self.members[0].seat(),
        }
    }

    /// The roster of the members at `order`, in that opening order.
    fn roster(&self, order: &[usize]) -> RingRoster {
        let seats = order.iter().map(|i| self.members[*i].seat()).collect();
        RingRoster::from_seats(self.terms(), seats, NOW).expect("roster")
    }

    /// Every member of `order`'s commit to `ring`.
    fn open(&self, order: &[usize], ring: &Ring) -> RingOpen {
        RingOpen {
            ring_id: ring.id(),
            commits: order
                .iter()
                .map(|i| RingCommitment {
                    nostr_pubkey: self.members[*i].identity.public_key().to_hex(),
                    signature: RingCommit::sign(ring, &self.members[*i].identity).signature,
                })
                .collect(),
        }
    }

    /// The instance sends `incoming` to member `i`.
    async fn deliver(
        &self,
        run: &mut RingRun,
        i: usize,
        incoming: RingIncoming,
    ) -> Result<Vec<Message>, RingRunError> {
        let member = &self.members[i];
        run.on_message(
            incoming,
            &self.instance.public_key(),
            &member.identity,
            &member.node,
            NOW,
        )
        .await
    }

    /// What member `i` sent, as the instance passes it on.
    fn relayed(message: &Message) -> RingIncoming {
        assert!(message.verify(), "runs only send well-formed messages");
        ring_wire::read(message).expect("a ring message")
    }

    /// One run per member, each holding a slot of a ring as large as the table.
    async fn seated(&self) -> Vec<RingRun> {
        let slots = self.members.len() as u8;
        let offer = self.offer(slots);
        let instance = self.instance.public_key();
        let mut runs = Vec::new();
        let (mut creator, first) = RingRun::create(instance, offer.clone(), 1, 7, 2);
        assert!(first.verify());
        assert_eq!(creator.state(), RingState::Offering);
        let echo = RingIncoming::Offered {
            order_id: self.order_id,
            request_id: Some(1),
            offer,
        };
        assert!(
            self.deliver(&mut creator, 0, echo)
                .await
                .expect("echo")
                .is_empty()
        );
        runs.push(creator);
        for (i, member) in self.members.iter().enumerate().skip(1) {
            let request = JoinRequest {
                order_id: self.order_id,
                request_id: 10 + i as u64,
                trade_index: 8,
                terms: self.terms(),
                slots,
                seat: member.seat(),
                feerate_sat_vb: 2,
            };
            let seat = request.seat.clone();
            let (mut run, take) = RingRun::join(instance, request);
            assert!(take.verify());
            assert_eq!(run.state(), RingState::Joining);
            let ack = RingIncoming::SeatTaken {
                order_id: self.order_id,
                request_id: Some(10 + i as u64),
                seat,
            };
            assert!(
                self.deliver(&mut run, i, ack)
                    .await
                    .expect("ack")
                    .is_empty()
            );
            runs.push(run);
        }
        for run in &runs {
            assert_eq!(run.state(), RingState::Seated);
        }
        runs
    }

    /// Every run with the roster of `order` accepted and its commit made.
    async fn committed(&self, order: &[usize]) -> (Vec<RingRun>, Ring) {
        let mut runs = self.seated().await;
        let roster = self.roster(order);
        let ring = roster.validate().expect("ring");
        for (i, run) in runs.iter_mut().enumerate() {
            let incoming = RingIncoming::Roster {
                order_id: self.order_id,
                roster: roster.clone(),
            };
            let out = self.deliver(run, i, incoming).await.expect("roster");
            // The answer is our own signature of this ring's id.
            let [commit] = out.as_slice() else {
                panic!("expected one commit, got {out:?}");
            };
            let Some(Payload::Ring(RingPayload::Commit(commit))) =
                &commit.get_inner_message_kind().payload
            else {
                panic!("not a commit");
            };
            commit
                .verify(&ring, &self.members[i].identity.public_key())
                .expect("our commit verifies");
            assert_eq!(run.state(), RingState::Committed);
        }
        (runs, ring)
    }

    /// Every run with every commit seen: the ring is open.
    async fn opened(&self, order: &[usize]) -> (Vec<RingRun>, Ring) {
        let (mut runs, ring) = self.committed(order).await;
        for (i, run) in runs.iter_mut().enumerate() {
            let incoming = RingIncoming::Open {
                order_id: self.order_id,
                open: self.open(order, &ring),
            };
            assert!(
                self.deliver(run, i, incoming)
                    .await
                    .expect("open")
                    .is_empty()
            );
            assert_eq!(run.state(), RingState::Open);
        }
        (runs, ring)
    }

    fn channels(&self) -> Vec<(String, String)> {
        let world = self.world.lock().expect("world");
        world
            .channels
            .iter()
            .map(|c| (c.opener.clone(), c.peer.clone()))
            .collect()
    }

    fn confirm_all(&self) {
        let outpoints: Vec<String> = {
            let world = self.world.lock().expect("world");
            world.channels.iter().map(|c| c.outpoint.clone()).collect()
        };
        for outpoint in outpoints {
            confirm(&self.world, &outpoint, 1);
        }
    }

    /// A channel of `capacity` from member `from` to member `to`, outside any ring.
    async fn channel(&self, from: usize, to: usize, capacity: u64) -> String {
        self.members[from]
            .node
            .open_channel(&OpenRequest {
                remote_pubkey: self.members[to].node.id(),
                remote_addr: None,
                capacity_sat: capacity,
                announce: true,
                feerate_sat_vb: 2,
            })
            .await
            .expect("channel")
    }

    fn edge_report(&self, ring: &Ring, outpoint: &str) -> RingIncoming {
        let (txid, index) = outpoint.split_once(':').expect("outpoint");
        RingIncoming::EdgeOpened {
            order_id: self.order_id,
            edge: RingEdge {
                ring_id: ring.id(),
                funding_txid: txid.to_string(),
                output_index: index.parse().expect("index"),
            },
        }
    }
}

/// Saves the run, forgets it and reads it back: a restart.
fn restarted(run: RingRun) -> RingRun {
    let json = run.to_json().expect("json");
    let state = run.state();
    drop(run);
    let back = RingRun::from_json(&json).expect("the stored run");
    assert_eq!(back.state(), state);
    back
}

#[tokio::test]
async fn a_ring_of_three_completes_and_survives_restarts_on_the_way() {
    let table = Table::new(3);
    // Not the order the members joined in.
    let order = [2, 0, 1];
    let (runs, ring) = table.opened(&order).await;
    let mut runs: Vec<RingRun> = runs.into_iter().map(restarted).collect();
    assert!(
        table.channels().is_empty(),
        "nothing is opened before every commit is in"
    );

    // Each member opens one channel, to the next in the ring, and reports it.
    let mut reports = Vec::new();
    for (i, run) in runs.iter_mut().enumerate() {
        let out = run
            .tick(&table.members[i].node, NOW + 10)
            .await
            .expect("tick");
        let [report] = out.as_slice() else {
            panic!("expected one channel report, got {out:?}");
        };
        reports.push(report.clone());
        // Opened once, however often the member looks.
        assert!(
            run.tick(&table.members[i].node, NOW + 20)
                .await
                .expect("tick")
                .is_empty()
        );
    }
    let mut opened = table.channels();
    opened.sort();
    let mut expected: Vec<(String, String)> = (0..3)
        .map(|k| {
            (
                table.members[order[k]].node.id(),
                table.members[order[(k + 1) % 3]].node.id(),
            )
        })
        .collect();
    expected.sort();
    assert_eq!(opened, expected);
    let mut runs: Vec<RingRun> = runs.into_iter().map(restarted).collect();

    // The instance passes each report to the member the channel was opened to.
    for (i, report) in reports.iter().enumerate() {
        let position = ring.position(&table.members[i].node_key()).expect("member");
        let next = order[(position + 1) % 3];
        let incoming = Table::relayed(report);
        assert!(matches!(incoming, RingIncoming::EdgeOpened { .. }));
        table
            .deliver(&mut runs[next], next, incoming)
            .await
            .expect("report");
    }
    // Unconfirmed channels are not confirmed to anyone.
    for (i, run) in runs.iter_mut().enumerate() {
        assert!(
            run.tick(&table.members[i].node, NOW + 30)
                .await
                .expect("tick")
                .is_empty()
        );
        assert_eq!(run.waiting_on(), Some("it is not confirmed yet"));
        assert!(!run.incoming_confirmed());
    }
    table.confirm_all();
    let mut runs: Vec<RingRun> = runs.into_iter().map(restarted).collect();

    let mut confirmations = Vec::new();
    for (i, run) in runs.iter_mut().enumerate() {
        let out = run
            .tick(&table.members[i].node, NOW + 40)
            .await
            .expect("tick");
        let [confirmation] = out.as_slice() else {
            panic!("expected one confirmation, got {out:?}");
        };
        assert!(run.incoming_confirmed());
        assert!(run.waiting_on().is_none());
        confirmations.push(confirmation.clone());
        // Confirmed once.
        assert!(
            run.tick(&table.members[i].node, NOW + 50)
                .await
                .expect("tick")
                .is_empty()
        );
    }
    // The instance passes each confirmation to the member that opened the channel.
    for (i, confirmation) in confirmations.iter().enumerate() {
        let position = ring.position(&table.members[i].node_key()).expect("member");
        let previous = order[(position + 2) % 3];
        let incoming = Table::relayed(confirmation);
        assert!(matches!(incoming, RingIncoming::EdgeVerified { .. }));
        table
            .deliver(&mut runs[previous], previous, incoming)
            .await
            .expect("confirmation");
    }
    for (i, run) in runs.iter_mut().enumerate() {
        assert!(run.our_channel_confirmed());
        assert!(!run.finished());
        let closed = RingIncoming::Closed {
            order_id: table.order_id,
            closed: RingClosed {
                ring_id: ring.id(),
                verified: vec![0, 1, 2],
            },
        };
        table.deliver(run, i, closed).await.expect("closed");
        assert_eq!(run.state(), RingState::Success);
        assert!(run.finished());
        assert_eq!(run.reported_confirmed(), &[0, 1, 2]);
    }
    assert_eq!(table.channels().len(), 3);
}

#[tokio::test]
async fn a_roster_that_is_not_the_ring_we_joined_gets_no_commit() {
    let table = Table::new(4);
    let mut runs = table.seated().await;
    // The ring was offered with four slots; member 1 is the one under test.
    let good = table.roster(&[3, 1, 0, 2]);
    let roster = |roster: RingRoster| RingIncoming::Roster {
        order_id: table.order_id,
        roster,
    };

    // Not from the instance.
    let stranger = Keys::generate();
    let result = runs[1]
        .on_message(
            roster(good.clone()),
            &stranger.public_key(),
            &table.members[1].identity,
            &table.members[1].node,
            NOW,
        )
        .await;
    assert!(matches!(result, Err(RingRunError::Sender)), "{result:?}");

    // A ring of the right size that leaves us out.
    let outsider = Member {
        node: Node {
            world: table.world.clone(),
            secret: secret(9),
            network: "regtest",
        },
        identity: Keys::generate(),
        trade: Keys::generate(),
    };
    let seats = vec![
        table.members[0].seat(),
        table.members[2].seat(),
        table.members[3].seat(),
        outsider.seat(),
    ];
    let without_us = RingRoster::from_seats(table.terms(), seats, NOW).expect("roster");
    let result = table.deliver(&mut runs[1], 1, roster(without_us)).await;
    assert!(
        matches!(
            result,
            Err(RingRunError::NotOurRing("our node is not in the roster"))
        ),
        "{result:?}"
    );

    // Our node, bound to someone else's key.
    let mut seized = good.clone();
    seized.seats[1].binding = table.members[1].binding_of(&stranger);
    let result = table.deliver(&mut runs[1], 1, roster(seized)).await;
    assert!(
        matches!(
            result,
            Err(RingRunError::NotOurRing(
                "our seat is bound to someone else's key"
            ))
        ),
        "{result:?}"
    );

    // Another capacity, and a smaller ring, than the order we joined showed.
    let mut terms = table.terms();
    terms.capacity_sat += 1;
    let seats = [3, 1, 0, 2]
        .iter()
        .map(|i| table.members[*i].seat())
        .collect();
    let bigger = RingRoster::from_seats(terms, seats, NOW).expect("roster");
    let smaller = table.roster(&[1, 0, 2]);
    for wrong in [bigger.clone(), smaller] {
        let result = table.deliver(&mut runs[1], 1, roster(wrong)).await;
        assert!(
            matches!(
                result,
                Err(RingRunError::NotOurRing(
                    "the roster is not the ring we joined"
                ))
            ),
            "{result:?}"
        );
    }
    // The creator compares with its own offer.
    let result = table.deliver(&mut runs[0], 0, roster(bigger)).await;
    assert!(
        matches!(
            result,
            Err(RingRunError::Wire(RingWireError::Invalid(
                RingMessageError::Mismatch(_)
            )))
        ),
        "{result:?}"
    );

    // Another member's binding is not a binding at all, or is another node's.
    let mut broken = good.clone();
    broken.seats[0].binding = "{}".to_string();
    let mut unbound = good.clone();
    unbound.seats[0].binding = table.members[2].seat().binding;
    for wrong in [broken, unbound] {
        let result = table.deliver(&mut runs[1], 1, roster(wrong)).await;
        assert!(
            matches!(result, Err(RingRunError::Wire(RingWireError::Invalid(_)))),
            "{result:?}"
        );
    }

    assert_eq!(runs[1].state(), RingState::Seated);
    assert_eq!(runs[0].state(), RingState::Seated);
    // The real roster is accepted after all that.
    let out = table
        .deliver(&mut runs[1], 1, roster(good))
        .await
        .expect("roster");
    assert_eq!(out.len(), 1);
    assert_eq!(runs[1].state(), RingState::Committed);
}

#[tokio::test]
async fn no_channel_is_opened_until_every_member_has_committed() {
    let table = Table::new(3);
    let order = [0, 1, 2];
    let (mut runs, ring) = table.committed(&order).await;
    let open = |open: RingOpen| RingIncoming::Open {
        order_id: table.order_id,
        open,
    };
    let good = table.open(&order, &ring);

    let mut missing = good.clone();
    missing.commits.pop();
    let stranger = Keys::generate();
    let mut swapped = good.clone();
    swapped.commits[2] = RingCommitment {
        nostr_pubkey: stranger.public_key().to_hex(),
        signature: RingCommit::sign(&ring, &stranger).signature,
    };
    let mut forged = good.clone();
    forged.commits[2].signature = good.commits[0].signature.clone();
    for wrong in [missing, swapped, forged] {
        let result = table.deliver(&mut runs[0], 0, open(wrong)).await;
        assert!(
            matches!(
                result,
                Err(RingRunError::Wire(RingWireError::Invalid(
                    RingMessageError::Commit(_)
                )))
            ),
            "{result:?}"
        );
        assert_eq!(runs[0].state(), RingState::Committed);
        assert!(
            runs[0]
                .tick(&table.members[0].node, NOW + 10)
                .await
                .expect("tick")
                .is_empty()
        );
    }
    // A channel report before the ring is open is out of order.
    let early = table.edge_report(&ring, &format!("{}:0", "cd".repeat(32)));
    let result = table.deliver(&mut runs[0], 0, early).await;
    assert!(
        matches!(result, Err(RingRunError::OutOfOrder(_))),
        "{result:?}"
    );
    assert!(table.channels().is_empty());

    table
        .deliver(&mut runs[0], 0, open(good))
        .await
        .expect("open");
    assert_eq!(runs[0].state(), RingState::Open);
    let out = runs[0]
        .tick(&table.members[0].node, NOW + 10)
        .await
        .expect("tick");
    assert_eq!(out.len(), 1);
    assert_eq!(table.channels().len(), 1);
}

#[tokio::test]
async fn a_channel_reported_to_us_is_confirmed_only_as_our_node_sees_it() {
    // An old channel from the previous member, with the ring's very capacity.
    let table = Table::new(3);
    let order = [0, 1, 2];
    let old = table.channel(0, 1, CAPACITY).await;
    confirm(&table.world, &old, 500);
    let (mut runs, ring) = table.opened(&order).await;
    let result = table
        .deliver(&mut runs[1], 1, table.edge_report(&ring, &old))
        .await;
    assert!(
        matches!(
            result,
            Err(RingRunError::NotOurRing(
                "the channel existed before the ring"
            ))
        ),
        "{result:?}"
    );

    // An outpoint our node does not have.
    let nowhere = format!("{}:7", "ee".repeat(32));
    table
        .deliver(&mut runs[1], 1, table.edge_report(&ring, &nowhere))
        .await
        .expect("report");
    let out = runs[1]
        .tick(&table.members[1].node, NOW + 10)
        .await
        .expect("tick");
    // It still opens its own channel; it confirms nothing.
    assert_eq!(out.len(), 1);
    assert!(!runs[1].incoming_confirmed());
    assert_eq!(
        runs[1].waiting_on(),
        Some("our node has no channel at that outpoint")
    );
    // The report cannot be swapped for another.
    let other = format!("{}:8", "ee".repeat(32));
    let result = table
        .deliver(&mut runs[1], 1, table.edge_report(&ring, &other))
        .await;
    assert!(
        matches!(result, Err(RingRunError::OutOfOrder(_))),
        "{result:?}"
    );

    // A channel of another capacity, and one that we opened ourselves.
    let table = Table::new(3);
    let (mut runs, ring) = table.opened(&order).await;
    let small = table.channel(0, 1, CAPACITY / 2).await;
    confirm(&table.world, &small, 3);
    table
        .deliver(&mut runs[1], 1, table.edge_report(&ring, &small))
        .await
        .expect("report");
    runs[1]
        .tick(&table.members[1].node, NOW + 10)
        .await
        .expect("tick");
    assert_eq!(runs[1].waiting_on(), Some("its capacity is not the ring's"));
    assert!(!runs[1].incoming_confirmed());

    let ours = table.channel(2, 1, CAPACITY).await;
    confirm(&table.world, &ours, 3);
    // Member 2 is told that member 1 opened a channel to it, and is pointed at one it opened.
    table
        .deliver(&mut runs[2], 2, table.edge_report(&ring, &ours))
        .await
        .expect("report");
    runs[2]
        .tick(&table.members[2].node, NOW + 10)
        .await
        .expect("tick");
    assert_eq!(runs[2].waiting_on(), Some("our node opened that channel"));
    assert!(!runs[2].incoming_confirmed());

    // A confirmation of a channel that is not the one we opened.
    let out = runs[0]
        .tick(&table.members[0].node, NOW + 10)
        .await
        .expect("tick");
    assert_eq!(out.len(), 1);
    let not_ours = RingIncoming::EdgeVerified {
        order_id: table.order_id,
        edge: RingEdge {
            ring_id: ring.id(),
            funding_txid: "ee".repeat(32),
            output_index: 0,
        },
    };
    let result = table.deliver(&mut runs[0], 0, not_ours).await;
    assert!(
        matches!(
            result,
            Err(RingRunError::NotOurRing(
                "that is not the channel we opened"
            ))
        ),
        "{result:?}"
    );
    assert!(!runs[0].our_channel_confirmed());
}

#[tokio::test]
async fn nothing_is_opened_after_the_window_and_a_crash_after_opening_opens_nothing_more() {
    let table = Table::new(3);
    let (mut runs, ring) = table.opened(&[0, 1, 2]).await;

    // Past the window nothing is opened.
    let late = ring.window_ends_at();
    assert!(
        runs[2]
            .tick(&table.members[2].node, late)
            .await
            .expect("tick")
            .is_empty()
    );
    assert!(table.channels().is_empty());

    // The channel is opened, and the agent dies before the run is saved.
    let before = runs[0].to_json().expect("json");
    let lost = runs[0]
        .tick(&table.members[0].node, NOW + 10)
        .await
        .expect("tick");
    assert_eq!(table.channels().len(), 1);
    let mut again = RingRun::from_json(&before).expect("the run as it was saved");
    assert!(again.opened_outpoint().is_none());
    let found = again
        .tick(&table.members[0].node, NOW + 20)
        .await
        .expect("tick");
    assert_eq!(table.channels().len(), 1, "no second channel");
    assert_eq!(again.opened_outpoint(), runs[0].opened_outpoint());
    assert_eq!(
        found[0].as_json().expect("json"),
        lost[0].as_json().expect("json"),
        "the same channel is reported"
    );

    // The window ends with channels missing: the instance's report ends the run.
    let closed = RingIncoming::Closed {
        order_id: table.order_id,
        closed: RingClosed {
            ring_id: ring.id(),
            verified: vec![],
        },
    };
    table.deliver(&mut again, 0, closed).await.expect("closed");
    assert_eq!(again.state(), RingState::Partial);
    assert!(again.finished());
    assert!(
        again
            .tick(&table.members[0].node, NOW + 30)
            .await
            .expect("tick")
            .is_empty()
    );
}

#[tokio::test]
async fn a_ring_can_be_left_while_it_fills_and_refusals_end_what_they_refuse() {
    let table = Table::new(3);
    let instance = table.instance.public_key();

    // The echo of someone else's offer is not ours.
    let offer = table.offer(3);
    let (mut creator, _) = RingRun::create(instance, offer.clone(), 1, 7, 2);
    let mut other = offer.clone();
    other.capacity_sat += 1;
    let echo = |offer: RingOffer, request_id: u64| RingIncoming::Offered {
        order_id: table.order_id,
        request_id: Some(request_id),
        offer,
    };
    for wrong in [echo(other, 1), echo(offer.clone(), 2)] {
        let result = table.deliver(&mut creator, 0, wrong).await;
        assert!(
            matches!(result, Err(RingRunError::OtherOrder)),
            "{result:?}"
        );
    }
    assert_eq!(creator.state(), RingState::Offering);
    assert!(
        creator.cancel(9).is_err(),
        "nothing to cancel without an order id"
    );
    table
        .deliver(&mut creator, 0, echo(offer, 1))
        .await
        .expect("echo");
    let cancel = creator.cancel(9).expect("cancel");
    assert_eq!(cancel[0].get_inner_message_kind().action, Action::Cancel);
    assert_eq!(creator.state(), RingState::Canceled);
    assert!(creator.finished());

    // A take the instance refuses, and a ring the instance cancels while it fills.
    let request = |request_id: u64| JoinRequest {
        order_id: table.order_id,
        request_id,
        trade_index: 8,
        terms: table.terms(),
        slots: 3,
        seat: table.members[1].seat(),
        feerate_sat_vb: 2,
    };
    let (mut refused, _) = RingRun::join(instance, request(5));
    let refusal = |request_id: u64| RingIncoming::Refused {
        order_id: Some(table.order_id),
        request_id: Some(request_id),
        reason: Some(CantDoReason::NotAllowedByStatus),
    };
    let result = table.deliver(&mut refused, 1, refusal(5)).await;
    assert!(
        matches!(
            result,
            Err(RingRunError::Refused(Some(
                CantDoReason::NotAllowedByStatus
            )))
        ),
        "{result:?}"
    );
    assert_eq!(refused.state(), RingState::Canceled);

    let mut runs = table.seated().await;
    let canceled = RingIncoming::Canceled {
        order_id: table.order_id,
    };
    table
        .deliver(&mut runs[1], 1, canceled.clone())
        .await
        .expect("canceled");
    assert_eq!(runs[1].state(), RingState::Canceled);
    // Once the roster is accepted an instance's cancel is out of order.
    let (mut runs, _) = table.committed(&[0, 1, 2]).await;
    let result = table.deliver(&mut runs[1], 1, canceled).await;
    assert!(
        matches!(result, Err(RingRunError::OutOfOrder(_))),
        "{result:?}"
    );
    assert_eq!(runs[1].state(), RingState::Committed);
    assert!(
        runs[1].cancel(9).is_err(),
        "no backing out after the roster"
    );
}

#[tokio::test]
async fn ring_messages_travel_sealed_and_only_the_instance_is_heard() {
    let table = Table::new(2);
    let roster = table.roster(&[0, 1]);
    let ring = roster.validate().expect("ring");
    let member = &table.members[1];

    // What an agent builds reads back as what it is.
    let take = ring_wire::take(table.order_id, 3, member.seat());
    assert!(matches!(
        ring_wire::read(&take),
        Ok(RingIncoming::SeatTaken {
            request_id: Some(3),
            ..
        })
    ));
    let outpoint = format!("{}:1", "cd".repeat(32));
    let opened = ring_wire::edge_opened(table.order_id, &ring, &outpoint).expect("edge");
    let verified = ring_wire::edge_verified(table.order_id, &ring, &outpoint).expect("edge");
    assert!(matches!(
        ring_wire::read(&opened),
        Ok(RingIncoming::EdgeOpened { .. })
    ));
    assert!(matches!(
        ring_wire::read(&verified),
        Ok(RingIncoming::EdgeVerified { .. })
    ));
    for bad in ["", "cd:1", "cd"] {
        assert!(
            ring_wire::edge_opened(table.order_id, &ring, bad).is_err(),
            "{bad}"
        );
    }
    assert!(ring_wire::cancel(table.order_id, 1).verify());
    // A new offer has no id yet; a fiat action is not a ring message.
    let offer = ring_wire::offer(1, table.offer(2));
    assert!(offer.verify());
    assert_eq!(
        ring_wire::read(&offer),
        Err(RingWireError::Shape("no order id"))
    );
    let fiat = Message::new_order(Some(table.order_id), None, None, Action::FiatSent, None);
    assert_eq!(
        ring_wire::read(&fiat),
        Err(RingWireError::Shape("not a ring message"))
    );

    // The instance's roster, sealed for the member's trade key.
    let message = Message::new_order(
        Some(table.order_id),
        None,
        None,
        Action::RingRoster,
        Some(Payload::Ring(RingPayload::Roster(roster))),
    );
    let event = lease_wire::seal_for_peer(&message, &table.instance, member.trade.public_key())
        .expect("seal");
    let (incoming, sender) = ring_wire::open(&event, &member.trade)
        .expect("open")
        .expect("for the member");
    assert!(matches!(incoming, RingIncoming::Roster { .. }));
    assert!(ring_wire::from_instance(
        &sender,
        &table.instance.public_key()
    ));
    assert_eq!(ring_wire::open(&event, &table.members[0].trade), Ok(None));
    // The same message from anyone else is not heard.
    let stranger = Keys::generate();
    let event =
        lease_wire::seal_for_peer(&message, &stranger, member.trade.public_key()).expect("seal");
    let (_, sender) = ring_wire::open(&event, &member.trade)
        .expect("open")
        .expect("for the member");
    assert!(!ring_wire::from_instance(
        &sender,
        &table.instance.public_key()
    ));
    // And our own messages go to the instance sealed like any other.
    let sealed = lease_wire::seal_for_instance(
        &take,
        &member.identity,
        &member.trade,
        table.instance.public_key(),
        0,
    )
    .expect("seal");
    assert_eq!(sealed.pubkey, member.trade.public_key());
}
