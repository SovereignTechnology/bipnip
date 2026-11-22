//! Two daemons, a relay and a stand-in instance: a whole lease, asked for and followed through
//! the sockets alone. The nodes are the simulator the lease tests use, on the real clock.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mostro_core::lease_message::LeasePayload;
use mostro_core::lease_order::{LeaseOrder, LeaseOrderStatus};
use mostro_core::node_binding::KIND_NODE_BINDING;
use mostro_core::prelude::{Action, Message, Payload, Peer, unwrap_message_nip44};
use nostr_sdk::local_relay::MockRelay;
use nostr_sdk::prelude::{
    Client, EventBuilder, EventId, Filter, FinalizeEvent, Keys, Kind, PublicKey, Tag, Timestamp,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

use super::policy::PolicyStore;
use super::server::serve;
use super::session::{KIND_WEB_REQUEST, START_EVERY_SECS};
use crate::config::Config;
use crate::keys::Wallet;
use crate::lease::tests::{Node, World, secret};
use crate::lease_wire;
use crate::relay::connected;
use crate::secretfile;

const POLL: Duration = Duration::from_millis(100);

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// The part of an instance a lease needs: it publishes the offer as an order and relays.
struct Instance {
    keys: Keys,
    client: Client,
    done: HashSet<EventId>,
    order_id: Uuid,
    seller: Option<PublicKey>,
    buyer: Option<PublicKey>,
}

impl Instance {
    async fn send(&self, to: PublicKey, message: Message) {
        let event = lease_wire::seal_for_peer(&message, &self.keys, to).expect("seal");
        self.client.send_event(&event).await.expect("send");
    }

    fn peer_key(&self, of: PublicKey) -> Message {
        Message::new_order(
            Some(self.order_id),
            None,
            None,
            Action::TradePubkey,
            Some(Payload::Peer(Peer::new(of.to_hex(), None))),
        )
    }

    async fn step(&mut self) {
        let filter = Filter::new()
            .kind(Kind::PrivateDirectMessage)
            .pubkey(self.keys.public_key());
        let events = self
            .client
            .fetch_events(filter)
            .timeout(Duration::from_secs(5))
            .await
            .expect("fetch");
        let mut fresh: Vec<_> = events
            .iter()
            .filter(|event| !self.done.contains(&event.id))
            .cloned()
            .collect();
        fresh.sort_by_key(|event| (event.created_at, event.id));
        for event in fresh {
            self.done.insert(event.id);
            let opened = unwrap_message_nip44(&event, &self.keys)
                .expect("unwrap")
                .expect("for the instance");
            let kind = opened.message.get_inner_message_kind().clone();
            let relayed = Message::new_order(
                Some(self.order_id),
                None,
                None,
                kind.action.clone(),
                kind.payload.clone(),
            );
            match kind.action {
                Action::LeaseOffer => {
                    // Only the first offer becomes the order; a second one is not answered.
                    if self.seller.is_some() {
                        continue;
                    }
                    let Some(Payload::Lease(LeasePayload::Offer(offer))) = &kind.payload else {
                        panic!("an offer without its terms");
                    };
                    let order =
                        LeaseOrder::from_offer(self.order_id, LeaseOrderStatus::Pending, offer)
                            .expect("order")
                            .to_event_builder()
                            .finalize(&self.keys)
                            .expect("sign");
                    self.client.send_event(&order).await.expect("publish");
                    self.seller = Some(opened.sender);
                    let echo = Message::new_order(
                        Some(self.order_id),
                        kind.request_id,
                        None,
                        Action::LeaseOffer,
                        kind.payload.clone(),
                    );
                    self.send(opened.sender, echo).await;
                }
                Action::TakeLease => {
                    let seller = self.seller.expect("an offer first");
                    self.buyer = Some(opened.sender);
                    self.send(seller, relayed).await;
                    self.send(seller, self.peer_key(opened.sender)).await;
                    self.send(opened.sender, self.peer_key(seller)).await;
                }
                Action::LeaseInvoice | Action::LeaseChannelOpened | Action::LeaseActive => {
                    assert_eq!(Some(opened.sender), self.seller, "only the seller");
                    self.send(self.buyer.expect("a take first"), relayed).await;
                }
                other => panic!("the instance was sent {other:?}"),
            }
        }
    }
}

/// One agent: its directory, its configuration and what its daemon is made from.
struct Agent {
    config: Config,
    mnemonic: String,
    node_secret: u8,
}

impl Agent {
    fn new(name: &str, relay: &str, instance: &PublicKey, policy: &str, node_secret: u8) -> Self {
        Self::with(name, relay, instance, policy, node_secret, "")
    }

    /// An agent whose configuration ends with `more` (another section).
    fn with(
        name: &str,
        relay: &str,
        instance: &PublicKey,
        policy: &str,
        node_secret: u8,
        more: &str,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("bipnip-daemon-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        secretfile::create_private_dir(&dir).expect("dir");
        let config = Config::parse(&format!(
            "network = \"regtest\"\ndata_dir = \"{}\"\nrelays = [\"{relay}\"]\n\n\
             [instance]\npubkey = \"{}\"\npow = 0\n\n[policy]\n{policy}\n{more}",
            dir.display(),
            instance.to_hex()
        ))
        .expect("config");
        Agent {
            config,
            mnemonic: Wallet::generate()
                .expect("wallet")
                .expose_mnemonic()
                .to_string(),
            node_secret,
        }
    }

    fn socket(&self) -> PathBuf {
        self.config.socket_path()
    }

    /// The daemon, as `bipnip-agent run` starts it. Each call is a new start on the same
    /// directory, mnemonic and node.
    async fn run(&self, world: &Arc<Mutex<World>>, tag: &'static str) -> Result<(), String> {
        let node = Node {
            world: world.clone(),
            secret: secret(self.node_secret),
            network: "regtest",
        };
        let wallet = Wallet::from_mnemonic(self.mnemonic.clone()).expect("wallet");
        serve(
            self.config.clone(),
            wallet,
            Box::new(node),
            POLL,
            Box::new(move |line| eprintln!("{tag}: {line}")),
        )
        .await
    }
}

/// One request: every line the daemon answers with.
async fn ask(socket: &Path, method: &str, params: Value) -> Vec<Value> {
    let line = json!({"v": 1, "method": method, "params": params}).to_string();
    send(socket, &line).await
}

/// One line sent as it is: every line the daemon answers with.
async fn send(socket: &Path, line: &str) -> Vec<Value> {
    let mut stream = None;
    for _ in 0..100 {
        match UnixStream::connect(socket).await {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let mut stream = stream.expect("the daemon listens");
    stream.write_all(line.as_bytes()).await.expect("write");
    stream.write_all(b"\n").await.expect("write");
    let mut text = String::new();
    stream.read_to_string(&mut text).await.expect("read");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

/// The result of a request that must work.
async fn ok(socket: &Path, method: &str, params: Value) -> Value {
    let lines = ask(socket, method, params).await;
    let last = lines.last().expect("a final line").clone();
    assert_eq!(last["ok"], json!(true), "{method}: {last}");
    last["result"].clone()
}

/// The error of a request that must fail.
async fn error(socket: &Path, method: &str, params: Value) -> Value {
    let lines = ask(socket, method, params).await;
    let last = lines.last().expect("a final line").clone();
    assert_eq!(last["ok"], json!(false), "{method}: {last}");
    last["error"].clone()
}

/// A page's request, signed with its session key: for `agent` when given, made at `at`.
fn signed(page: &Keys, agent: Option<PublicKey>, method: &str, params: Value, at: u64) -> String {
    let content = json!({"v": 1, "method": method, "params": params}).to_string();
    let nonce = Uuid::new_v4().simple().to_string();
    let mut builder = EventBuilder::new(Kind::from(KIND_WEB_REQUEST), content)
        .tag(Tag::parse(["nonce", nonce.as_str()]).expect("tag"))
        .custom_created_at(Timestamp::from_secs(at));
    if let Some(agent) = agent {
        builder = builder.tag(Tag::public_key(agent));
    }
    serde_json::to_string(&builder.finalize(page).expect("sign")).expect("json")
}

/// The final line of a page's request, made now.
async fn page_ask(
    door: &Path,
    page: &Keys,
    agent: Option<PublicKey>,
    method: &str,
    params: Value,
) -> Value {
    let lines = send(door, &signed(page, agent, method, params, unix_now())).await;
    lines.last().expect("a final line").clone()
}

/// The result of a page's request that must work.
async fn page_ok(
    door: &Path,
    page: &Keys,
    agent: Option<PublicKey>,
    method: &str,
    params: Value,
) -> Value {
    let last = page_ask(door, page, agent, method, params).await;
    assert_eq!(last["ok"], json!(true), "{method}: {last}");
    last["result"].clone()
}

/// The error of a page's request that must fail.
async fn page_error(
    door: &Path,
    page: &Keys,
    agent: Option<PublicKey>,
    method: &str,
    params: Value,
) -> Value {
    let last = page_ask(door, page, agent, method, params).await;
    assert_eq!(last["ok"], json!(false), "{method}: {last}");
    last["error"].clone()
}

/// The node binding events (kind 31735) the relay holds, by id.
async fn bindings(url: &str) -> Vec<EventId> {
    let client = connected(&[url.to_string()]).await.expect("connect");
    let events = client
        .fetch_events(Filter::new().kind(Kind::Custom(KIND_NODE_BINDING)))
        .timeout(Duration::from_secs(5))
        .await
        .expect("fetch");
    client.shutdown().await;
    events.iter().map(|event| event.id).collect()
}

/// The state of the one lease run an agent has stored.
async fn lease_state(socket: &Path) -> Option<String> {
    let listed = ok(socket, "lease.list", Value::Null).await;
    let runs = listed["runs"].as_array().expect("runs");
    assert!(runs.len() <= 1, "one lease at most: {listed}");
    runs.first()
        .and_then(|run| run["state"].as_str())
        .map(str::to_string)
}

async fn wait_for_state(socket: &Path, wanted: &str, world: &Arc<Mutex<World>>) {
    for _ in 0..300 {
        // Blocks arrive: whatever channel is open gets its confirmations.
        for channel in world.lock().expect("world").channels.iter_mut() {
            channel.confirmations = 3;
        }
        if lease_state(socket).await.as_deref() == Some(wanted) {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!(
        "the run never reached {wanted}; it is at {:?}",
        lease_state(socket).await
    );
}

const OFFER: &str = r#"{"min_capacity_sat": 500000, "max_capacity_sat": 1000000,
    "duration_blocks": 4320, "fee_base_sat": 1000, "fee_ppm": 2500, "feerate_sat_vb": 2}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_is_asked_for_approved_and_carried_through_a_restart_by_the_sockets_alone() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let world = Arc::new(Mutex::new(World::default()));
    world.lock().expect("world").clock = Some(unix_now());
    let mut instance = Instance {
        keys: Keys::generate(),
        client: connected(std::slice::from_ref(&url))
            .await
            .expect("connect"),
        done: HashSet::new(),
        order_id: Uuid::new_v4(),
        seller: None,
        buyer: None,
    };
    let instance_key = instance.keys.public_key();
    let order_id = instance.order_id.to_string();
    // The instance works beside the daemons for the whole test. It can be held still, so that
    // the daemons are stopped at a moment when nothing is half relayed.
    let paused = Arc::new(AtomicBool::new(false));
    let relaying = {
        let paused = paused.clone();
        tokio::spawn(async move {
            loop {
                if !paused.load(Ordering::SeqCst) {
                    instance.step().await;
                }
                tokio::time::sleep(POLL).await;
            }
        })
    };
    // The seller may run one lease but has no capacity allowance: its offer needs a person.
    let seller = Agent::new("seller", &url, &instance_key, "max_runs = 1", 1);
    // The buyer may spend 3000 sat on a lease without being asked.
    let buyer = Agent::new(
        "buyer",
        &url,
        &instance_key,
        "max_runs = 1\nmax_fee_per_lease_sat = 3000\nmax_fee_per_day_sat = 5000",
        2,
    );
    let offer: Value = serde_json::from_str(OFFER).expect("offer");

    // Until the lease is taken.
    let first_half = async {
        let status = ok(&seller.socket(), "status", Value::Null).await;
        assert_eq!(status["network"], json!("regtest"));
        assert_eq!(status["runs"]["under_way"], json!(0));
        assert_eq!(
            status["instances"][0]["pubkey"],
            json!(instance_key.to_hex())
        );

        // Over the seller's limits: parked, nothing stored, nothing sent.
        let parked = error(&seller.socket(), "lease.offer", offer.clone()).await;
        assert_eq!(parked["code"], json!("needs-approval"), "{parked}");
        assert!(
            parked["message"]
                .as_str()
                .is_some_and(|m| m.contains("max_capacity_per_day_sat")),
            "{parked}"
        );
        let first = parked["approval"].as_str().expect("its name").to_string();
        assert_eq!(lease_state(&seller.socket()).await, None);
        // Nor published: the node's binding links it to the agent's identity in public, so it
        // waits until the offer is allowed.
        assert_eq!(bindings(&url).await, Vec::new());
        // Asked twice, parked twice; one is denied.
        let again = error(&seller.socket(), "lease.offer", offer.clone()).await;
        let second = again["approval"].as_str().expect("its name").to_string();
        let waiting = ok(&seller.socket(), "approvals", Value::Null).await;
        assert_eq!(waiting["approvals"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            waiting["approvals"][0]["ask"]["capacity_sat"],
            json!(1_000_000)
        );
        ok(&seller.socket(), "deny", json!({"approval": second})).await;
        let gone = error(&seller.socket(), "approve", json!({"approval": second})).await;
        assert_eq!(gone["code"], json!("not-found"));
        // Approved: the offer goes out as it was shown.
        let started = ok(&seller.socket(), "approve", json!({"approval": first})).await;
        assert_eq!(started["role"], json!("seller"));
        // A run is told in numbers too: what the seller offered.
        assert_eq!(started["offered"]["max_capacity_sat"], json!(1_000_000));
        assert_eq!(started["offered"]["fee_ppm"], json!(2500));
        let published = bindings(&url).await;
        assert_eq!(
            published.len(),
            1,
            "the binding goes out with the approved offer"
        );
        let waiting = ok(&seller.socket(), "approvals", Value::Null).await;
        assert_eq!(waiting["approvals"], json!([]));
        wait_for_state(&seller.socket(), "open", &world).await;
        // One run is all the seller may have under way.
        let full = error(&seller.socket(), "lease.offer", offer.clone()).await;
        assert!(
            full["message"]
                .as_str()
                .is_some_and(|m| m.contains("max_runs")),
            "{full}"
        );
        // Parked again, so its binding is not published again either.
        assert_eq!(bindings(&url).await, published);

        // The buyer sees the order and what taking it costs.
        let orders = ok(&buyer.socket(), "orders", json!({"kind": "lease"})).await;
        assert_eq!(orders["orders"][0]["order_id"], json!(order_id));
        assert_eq!(orders["orders"].as_array().map(Vec::len), Some(1));
        let rings = ok(&buyer.socket(), "orders", json!({"kind": "ring"})).await;
        assert_eq!(rings["orders"], json!([]));
        // With the sellers' records: this seller has none yet, and says so in numbers.
        let recorded = ok(
            &buyer.socket(),
            "orders",
            json!({"kind": "lease", "records": true}),
        )
        .await;
        assert_eq!(recorded["orders"][0]["seller_record"]["kept"], json!(0));
        assert_eq!(recorded["orders"][0]["seller_record"]["subjects"], json!(0));
        assert!(recorded.get("records_failed").is_none(), "{recorded}");
        // A named instance that is not configured is refused before anything is read.
        let elsewhere = error(
            &buyer.socket(),
            "orders",
            json!({"instance": Keys::generate().public_key().to_hex()}),
        )
        .await;
        assert_eq!(elsewhere["code"], json!("not-found"));
        // A million sat would cost 3500 in fees: over the buyer's limit, so parked, and denied.
        let dear = error(
            &buyer.socket(),
            "lease.take",
            json!({"order_id": order_id, "capacity_sat": 1_000_000}),
        )
        .await;
        assert_eq!(dear["code"], json!("needs-approval"), "{dear}");
        assert!(
            dear["message"]
                .as_str()
                .is_some_and(|m| m.contains("max_fee_per_lease_sat")),
            "{dear}"
        );
        ok(
            &buyer.socket(),
            "deny",
            json!({"approval": dear["approval"]}),
        )
        .await;
        assert_eq!(lease_state(&buyer.socket()).await, None);
        // 600000 sat cost 2500: within the limits, so it starts at once. The instance is held
        // still first: the take stays on the relay until the daemons are back.
        paused.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let lines = ask(
            &buyer.socket(),
            "lease.take",
            json!({"order_id": order_id, "capacity_sat": 600_000}),
        )
        .await;
        assert_eq!(
            lines.first().map(|line| &line["status"]),
            Some(&json!("started"))
        );
        let taken = lines.last().expect("final");
        assert_eq!(taken["ok"], json!(true), "{taken}");
        assert_eq!(taken["result"]["role"], json!("buyer"));
        // The lease in numbers: what it buys, what it costs, and from whom.
        let terms = &taken["result"]["terms"];
        assert_eq!(terms["capacity_sat"], json!(600_000));
        assert_eq!(terms["fee_sat"], json!(2_500));
        assert_eq!(terms["duration_blocks"], json!(4320));
        assert_eq!(terms["peer"], orders["orders"][0]["seller_node"], "{taken}");
        // What is not a request does nothing.
        let bad = error(&buyer.socket(), "wallet.dump", Value::Null).await;
        assert_eq!(bad["code"], json!("bad-request"));
        let missing = error(&buyer.socket(), "lease.cancel", json!({"run": "buyer-00"})).await;
        assert_eq!(missing["code"], json!("not-found"));
    };
    tokio::select! {
        end = seller.run(&world, "seller") => panic!("the seller's daemon ended: {end:?}"),
        end = buyer.run(&world, "buyer") => panic!("the buyer's daemon ended: {end:?}"),
        () = first_half => {}
    }
    // The sockets of the daemons that are gone: without them, a client waits for the new ones.
    tokio::time::sleep(Duration::from_millis(200)).await;
    for agent in [&seller, &buyer] {
        let _ = std::fs::remove_file(agent.socket());
    }

    // Both daemons are gone, mid-lease. Started again, they carry the lease on from disk.
    let second_half = async {
        // A watcher is told where the run is and then every change, up to the end.
        let mut watch = UnixStream::connect(&buyer.socket()).await;
        for _ in 0..100 {
            if watch.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            watch = UnixStream::connect(&buyer.socket()).await;
        }
        let mut watch = BufReader::new(watch.expect("the daemon listens again"));
        watch
            .get_mut()
            .write_all(b"{\"v\":1,\"method\":\"watch\"}\n")
            .await
            .expect("write");
        // Both daemons took their run up again; now the take is relayed.
        assert_eq!(lease_state(&seller.socket()).await.as_deref(), Some("open"));
        assert_eq!(
            lease_state(&buyer.socket()).await.as_deref(),
            Some("taking")
        );
        paused.store(false, Ordering::SeqCst);
        wait_for_state(&buyer.socket(), "active", &world).await;
        wait_for_state(&seller.socket(), "active", &world).await;
        let mut seen = Vec::new();
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(10), watch.read_line(&mut line))
                .await
                .expect("the watcher is told in time")
                .expect("read");
            assert!(
                read > 0,
                "the watch ended before the lease was active: {seen:?}"
            );
            let line: Value = serde_json::from_str(&line).expect("a JSON line");
            let done = line["status"] == json!("run") && line["state"] == json!("active");
            seen.push(line);
            if done {
                break;
            }
        }
        assert!(seen.iter().any(|line| line["status"] == json!("watching")));

        // A lease that is active, with all of its fee paid upfront, is only watched: it is not
        // under way (it holds no slot), and its fee counts against the day.
        tokio::time::sleep(POLL * 3).await;
        let status = ok(&buyer.socket(), "status", Value::Null).await;
        assert_eq!(status["runs"]["under_way"], json!(0), "{status}");
        assert_eq!(status["last_day"]["fee_sat"], json!(2_500));
        let status = ok(&seller.socket(), "status", Value::Null).await;
        assert_eq!(status["last_day"]["capacity_sat"], json!(1_000_000));
        let listed = ok(&seller.socket(), "lease.list", Value::Null).await;
        assert_eq!(listed["runs"][0]["finished"], json!(false));
        assert_eq!(listed["runs"][0]["watching"], json!(true));
        assert_eq!(listed["runs"][0]["under_way"], json!(false));
        assert_eq!(status["runs"]["watching"], json!(1), "{status}");
        assert_eq!(listed["runs"][0]["order_id"], json!(order_id));
    };
    tokio::select! {
        end = seller.run(&world, "seller") => panic!("the seller's daemon ended: {end:?}"),
        end = buyer.run(&world, "buyer") => panic!("the buyer's daemon ended: {end:?}"),
        () = second_half => {}
    }
    assert!(!relaying.is_finished(), "the stand-in instance stopped");
    relaying.abort();

    // What each run did with its node is what it was granted, to the satoshi.
    let grants = |agent: &Agent, prefix: &str| {
        let store = PolicyStore::open(&agent.config.policy_dir()).expect("store");
        let run = std::fs::read_dir(agent.config.leases_dir())
            .expect("leases")
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .find_map(|name| {
                name.strip_suffix(".json")
                    .filter(|stem| stem.starts_with(prefix))
                    .map(str::to_string)
            })
            .expect("the stored run");
        store.grant(&run).expect("read").expect("the run's grant")
    };
    let sold = grants(&seller, "seller-");
    assert!(sold.approved);
    assert_eq!(sold.opened_sat, 600_000);
    assert!(sold.paid.is_empty());
    let bought = grants(&buyer, "buyer-");
    assert!(!bought.approved);
    assert_eq!(bought.ask.fee_sat, 2_500);
    assert_eq!(bought.opened_sat, 0);
    assert_eq!(
        bought.paid.iter().map(|paid| paid.msat).collect::<Vec<_>>(),
        vec![2_500_000]
    );
    let world = world.lock().expect("world");
    assert_eq!(world.channels.len(), 1);
    assert_eq!(world.channels[0].capacity_sat, 600_000);
    assert_eq!(world.payments.len(), 1);
}

/// A node key, any real one: what `record` would ask about.
const SOME_NODE: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_paired_held_to_its_scope_and_moves_nothing_by_itself() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let world = Arc::new(Mutex::new(World::default()));
    world.lock().expect("world").clock = Some(unix_now());
    let instance = Keys::generate().public_key();
    // Limits that would let an offer through unasked: a page's is parked all the same.
    let agent = Agent::with(
        "paged",
        &url,
        &instance,
        "max_runs = 4\nmax_capacity_per_day_sat = 10000000",
        3,
        "\n[web]\n",
    );
    let owner = agent.socket();
    let door = agent.config.web_socket_path().expect("a web door");
    let identity = Wallet::from_mnemonic(agent.mnemonic.clone())
        .expect("wallet")
        .liquidity()
        .expect("identity")
        .public_key();
    let me = Some(identity);
    let page = Keys::generate();
    let offer: Value = serde_json::from_str(OFFER).expect("offer");

    let checks = async {
        ok(&owner, "status", Value::Null).await;
        // A page's request dated within a few seconds of the start is not taken (nothing seen
        // before a restart can be replayed after it): wait those out.
        let stale = page_error(&door, &page, None, "pair.status", Value::Null).await;
        assert_eq!(stale["code"], json!("stale"), "{stale}");
        assert!(stale["now"].as_u64().is_some());
        tokio::time::sleep(Duration::from_secs(6)).await;

        // The door takes signed requests only.
        let plain = ask(&door, "status", Value::Null).await;
        assert_eq!(
            plain.last().expect("final")["error"]["code"],
            json!("bad-request")
        );

        // Pairing is closed until the operator opens it, and asking then leaves nothing.
        let closed = page_error(
            &door,
            &page,
            None,
            "pair.request",
            json!({"label": "a page"}),
        )
        .await;
        assert_eq!(closed["code"], json!("refused"), "{closed}");
        assert_eq!(
            ok(&owner, "pair.list", Value::Null).await["waiting"],
            json!([])
        );
        ok(&owner, "pair.open", json!({"secs": 120})).await;
        let asked = page_ok(
            &door,
            &page,
            None,
            "pair.request",
            json!({"label": "a page"}),
        )
        .await;
        let code = asked["code"].as_str().expect("a code").to_string();
        assert_eq!(asked["agent"], json!(identity.to_hex()));
        // Asked again, the same code.
        let again = page_ok(&door, &page, None, "pair.request", Value::Null).await;
        assert_eq!(again["code"], json!(code));
        // Listed where the agent runs, never with its code.
        let waiting = ok(&owner, "pair.list", Value::Null).await;
        assert_eq!(waiting["waiting"][0]["label"], json!("a page"));
        assert!(!waiting.to_string().contains(&code));
        // Not paired yet: nothing but pairing.
        let early = page_error(&door, &page, me, "status", Value::Null).await;
        assert_eq!(early["code"], json!("unpaired"));
        // A wrong code pairs nothing; the page's own pairs it, to look only.
        let wrong = if code == "1111-2222-3333" {
            "4444-5555-6666"
        } else {
            "1111-2222-3333"
        };
        assert_eq!(
            error(&owner, "pair.approve", json!({"code": wrong})).await["code"],
            json!("not-found")
        );
        let paired = ok(&owner, "pair.approve", json!({"code": code})).await;
        assert_eq!(paired["scope"], json!("read"));
        assert_eq!(
            page_ok(&door, &page, None, "pair.status", Value::Null).await["paired"],
            json!(true)
        );

        // A page is not told the policy or the day's budget.
        let status = page_ok(&door, &page, me, "status", Value::Null).await;
        assert_eq!(status["network"], json!("regtest"));
        assert!(
            status.get("policy").is_none() && status.get("last_day").is_none(),
            "{status}"
        );
        // A request names this agent, is fresh, and is taken once.
        let elsewhere = Some(Keys::generate().public_key());
        assert_eq!(
            page_error(&door, &page, elsewhere, "status", Value::Null).await["code"],
            json!("refused")
        );
        let once = signed(&page, me, "lease.list", Value::Null, unix_now());
        assert_eq!(
            send(&door, &once).await.last().expect("final")["ok"],
            json!(true)
        );
        let replayed = send(&door, &once).await;
        assert_eq!(
            replayed.last().expect("final")["error"]["code"],
            json!("refused")
        );
        let old = signed(&page, me, "status", Value::Null, unix_now() - 600);
        assert_eq!(
            send(&door, &old).await.last().expect("final")["error"]["code"],
            json!("stale")
        );
        let ahead = signed(&page, me, "status", Value::Null, unix_now() + 60);
        assert_eq!(
            send(&door, &ahead).await.last().expect("final")["error"]["code"],
            json!("stale")
        );
        // The book is a page's to read.
        page_ok(&door, &page, me, "orders", json!({"records": true})).await;

        // Looking only: no run. And never the operator's requests, whatever the scope.
        assert_eq!(
            page_error(&door, &page, me, "lease.offer", offer.clone()).await["code"],
            json!("refused")
        );
        for (method, params) in [
            ("approve", json!({"approval": "aa"})),
            ("deny", json!({"approval": "aa"})),
            ("binding.publish", Value::Null),
            ("record", json!({"node": SOME_NODE})),
            ("sessions", Value::Null),
            ("pair.open", Value::Null),
            ("pair.approve", json!({"code": code})),
            ("revoke", json!({"session": page.public_key().to_hex()})),
        ] {
            let refused = page_error(&door, &page, me, method, params).await;
            assert_eq!(refused["code"], json!("refused"), "{method}: {refused}");
        }
        // What only a page asks is not the operator's.
        assert_eq!(
            error(&owner, "pair.status", Value::Null).await["code"],
            json!("bad-request")
        );

        // Revoked: unpaired from then on.
        ok(
            &owner,
            "revoke",
            json!({"session": page.public_key().to_hex()}),
        )
        .await;
        assert_eq!(
            page_error(&door, &page, me, "status", Value::Null).await["code"],
            json!("unpaired")
        );
        let sessions = ok(&owner, "sessions", Value::Null).await;
        assert_eq!(sessions["sessions"][0]["live"], json!(false));

        // Paired again, to trade.
        ok(&owner, "pair.open", Value::Null).await;
        let asked = page_ok(&door, &page, None, "pair.request", Value::Null).await;
        let paired = ok(
            &owner,
            "pair.approve",
            json!({"code": asked["code"], "scope": "trade", "expires_days": 1, "label": "desk"}),
        )
        .await;
        assert_eq!(paired["label"], json!("desk"));
        // Within every limit, a page's offer is parked all the same, and nothing is published.
        let parked = page_error(&door, &page, me, "lease.offer", offer.clone()).await;
        assert_eq!(parked["code"], json!("needs-approval"), "{parked}");
        let approval = parked["approval"].as_str().expect("its name").to_string();
        assert_eq!(bindings(&url).await, Vec::new());
        assert_eq!(lease_state(&owner).await, None);
        // Asked again at once: told to wait.
        assert_eq!(
            page_error(&door, &page, me, "lease.offer", offer.clone()).await["code"],
            json!("busy")
        );
        // The page sees its own parked request; the operator sees which page asked.
        let own = page_ok(&door, &page, me, "approvals", Value::Null).await;
        assert_eq!(own["approvals"][0]["approval"], json!(approval));
        let all = ok(&owner, "approvals", Value::Null).await;
        assert_eq!(all["approvals"][0]["asked_by"]["label"], json!("desk"));
        assert_eq!(
            all["approvals"][0]["asked_by"]["session"],
            json!(page.public_key().to_hex())
        );
        // Approved where the agent runs: the run names the approval, and is the page's.
        let started = ok(&owner, "approve", json!({"approval": approval})).await;
        assert_eq!(started["approval"], json!(approval));
        let run = started["run"].as_str().expect("a run").to_string();
        let listed = page_ok(&door, &page, me, "lease.list", Value::Null).await;
        assert_eq!(listed["runs"][0]["mine"], json!(true), "{listed}");
        assert!(listed["runs"][0].get("line").is_none(), "{listed}");
        assert_eq!(listed["refused"], json!([]));
        // The page may cancel what it asked for (here the run itself answers that there is nothing
        // to cancel yet: no instance ever took the offer), never what the operator asked for.
        let mine = page_error(&door, &page, me, "lease.cancel", json!({"run": run})).await;
        assert!(
            !mine["message"]
                .as_str()
                .unwrap_or_default()
                .contains("did not ask"),
            "{mine}"
        );
        let theirs = ok(&owner, "lease.offer", offer.clone()).await;
        let theirs = theirs["run"].as_str().expect("a run").to_string();
        assert_ne!(theirs, run);
        let not_mine = page_error(&door, &page, me, "lease.cancel", json!({"run": theirs})).await;
        assert!(
            not_mine["message"]
                .as_str()
                .unwrap_or_default()
                .contains("did not ask"),
            "{not_mine}"
        );
        let listed = page_ok(&door, &page, me, "lease.list", Value::Null).await;
        let flags: Vec<&Value> = listed["runs"]
            .as_array()
            .expect("runs")
            .iter()
            .map(|run| &run["mine"])
            .collect();
        assert_eq!(flags.len(), 2);
        assert!(
            flags.contains(&&json!(true)) && flags.contains(&&json!(false)),
            "{listed}"
        );

        // Its watches: two at most.
        let watch = signed(&page, me, "watch", Value::Null, unix_now());
        let mut first = UnixStream::connect(&door).await.expect("connect");
        first
            .write_all(format!("{watch}\n").as_bytes())
            .await
            .expect("write");
        let watch = signed(&page, me, "watch", Value::Null, unix_now());
        let mut second = UnixStream::connect(&door).await.expect("connect");
        second
            .write_all(format!("{watch}\n").as_bytes())
            .await
            .expect("write");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let third = page_error(&door, &page, me, "watch", Value::Null).await;
        assert_eq!(third["code"], json!("busy"), "{third}");

        // Ended by the page itself: its watches close, and what it still had waiting goes.
        tokio::time::sleep(Duration::from_secs(START_EVERY_SECS)).await;
        let waiting = page_error(&door, &page, me, "lease.offer", offer.clone()).await;
        assert_eq!(waiting["code"], json!("needs-approval"));
        page_ok(&door, &page, me, "pair.end", Value::Null).await;
        assert_eq!(
            ok(&owner, "approvals", Value::Null).await["approvals"],
            json!([])
        );
        let mut rest = String::new();
        tokio::time::timeout(Duration::from_secs(5), first.read_to_string(&mut rest))
            .await
            .expect("the watch is closed")
            .expect("read");
        drop(second);
        assert_eq!(
            page_error(&door, &page, me, "status", Value::Null).await["code"],
            json!("unpaired")
        );
    };
    tokio::select! {
        end = agent.run(&world, "paged") => panic!("the daemon ended: {end:?}"),
        () = checks => {}
    }
}
