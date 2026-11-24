//! The daemon's one worker: it owns the wallet, the node, the relay connection and every run,
//! and does one thing at a time. A tick steps all runs; a request is answered between ticks.
//!
//! Nothing here is new lease or ring logic. A run is prepared by the same code the one-shot
//! commands use ([`crate::lease::cmd`], [`crate::ring::cmd`]), stepped by the same step
//! functions, and saved by the same stores. What the engine adds:
//!
//! - one relay query per tick for all runs, handed out by trade key ([`Link::deliver`]);
//! - the policy: a new run is weighed before anything of it is stored or sent, parked when it
//!   is over a limit, and given a [`Grant`] that the run's node access is then held to
//!   ([`Guard`]);
//! - answers to the requests of `docs/agent-api.md`.
//!
//! A run started by a one-shot command has no grant. The daemon carries it on as that command
//! would have: the person who typed it gave it its limits.

use std::collections::BTreeMap;

use bipnip_ln::LightningBackend;
use mostro_core::lease::LeaseContract;
use mostro_core::node_binding::Action as BindingAction;
use mostro_core::prelude::Message;
use mostro_core::rules::Rules;
use nostr_sdk::prelude::{Client, Event, EventId, Filter, Kind, PublicKey, Timestamp, ToBech32};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::api::{
    self, ApiError, Call, Code, LeaseOfferParams, LeaseTakeParams, PairApprove, Request,
    RingCreateParams, RingJoinParams,
};
use super::policy::{Ask, Grant, Guard, Parked, PolicyStore, Usage, over, seller_reasons};
use super::reads::Reader;
use super::session::{self, SessionStore, Signed};
use crate::binding::make_binding_event;
use crate::config::{Config, InstanceConfig, InstanceEntry};
use crate::keys::Wallet;
use crate::lease::cmd::{
    self as lease_cmd, OfferParams, RESUME_LOOKBACK_SECS, Session, TakeParams, unix_now,
};
use crate::lease::link::{Link, StepReport, launch_buyer, launch_seller, step_buyer, step_seller};
use crate::lease::run::{BuyerConfig, BuyerRun, Outgoing, SellerConfig, SellerRun};
use crate::lease::store::{LeaseStore, StoreError};
use crate::lease::watch::Watch;
use crate::lease::{DEFAULT_MAX_CLTV_DELTA, DEFAULT_SELLER_CLTV_DELTA};
use crate::relay::{self, RELAY_TIMEOUT};
use crate::ring::cmd::{self as ring_cmd, CreateParams, JoinParams};
use crate::ring::link as ring_link;
use crate::ring::run::RingRun;
use crate::ring::store::RingStore;

/// The most a buyer's node may pay in routing fees for one lease payment, in millisatoshi: the
/// default of `lease take --max-fee-msat`.
const BUYER_MAX_ROUTING_FEE_MSAT: u64 = 10_000;
/// How many ticks pass between tries to reach an instance whose proof of work is not known yet.
const RESOLVE_EVERY_TICKS: u32 = 12;
/// How many ticks pass between looks for finished runs that are old enough to put away.
const TIDY_EVERY_TICKS: u32 = 720;
/// How long a finished run stays where it is listed before it is put away, in seconds.
const KEEP_FINISHED_SECS: u64 = 30 * 24 * 3600;
/// How often a lease that is only watched is looked at, in seconds.
const WATCH_EVERY_SECS: u64 = 60;

/// Where a request's lines go: the client's connection. Lines that do not fit are dropped; the
/// engine never waits for a client.
pub type Lines = mpsc::Sender<String>;

/// One run of any kind. A handful of these live in a list; boxing the larger kinds would buy
/// nothing.
#[allow(clippy::large_enum_variant)]
enum Run {
    Seller(SellerRun),
    Buyer(BuyerRun),
    Ring(RingRun),
}

impl Run {
    /// The name the run is stored under, without the `.json`.
    fn name(&self) -> String {
        match self {
            Run::Seller(run) => format!("seller-{}", run.store_key()),
            Run::Buyer(run) => format!("buyer-{}", run.store_key()),
            Run::Ring(run) => format!("ring-{}", run.store_key()),
        }
    }

    fn is_lease(&self) -> bool {
        !matches!(self, Run::Ring(_))
    }

    fn finished(&self) -> bool {
        match self {
            Run::Seller(run) => run.finished(),
            Run::Buyer(run) => run.finished(),
            Run::Ring(run) => run.finished(),
        }
    }

    /// True for a lease that is active and only watched: it holds no slot under `max_runs`
    /// and is looked at once every [`WATCH_EVERY_SECS`].
    fn watching(&self) -> bool {
        match self {
            Run::Seller(run) => run.watching(),
            Run::Buyer(run) => run.watching(),
            Run::Ring(run) => run.outcome_owed(),
        }
    }

    fn instance(&self) -> PublicKey {
        match self {
            Run::Seller(run) => run.instance(),
            Run::Buyer(run) => run.instance(),
            Run::Ring(run) => run.instance(),
        }
    }

    fn trade_index(&self) -> u32 {
        match self {
            Run::Seller(run) => run.trade_index(),
            Run::Buyer(run) => run.trade_index(),
            Run::Ring(run) => run.trade_index(),
        }
    }

    /// One line about the run, as the one-shot commands print it. Nothing secret in it.
    fn line(&self) -> String {
        match self {
            Run::Seller(run) => lease_cmd::describe_seller(run),
            Run::Buyer(run) => lease_cmd::describe_buyer(run),
            Run::Ring(run) => ring_cmd::describe(run),
        }
    }

    fn state(&self) -> Value {
        let state = match self {
            Run::Seller(run) => serde_json::to_value(run.state()),
            Run::Buyer(run) => serde_json::to_value(run.state()),
            Run::Ring(run) => serde_json::to_value(run.state()),
        };
        state.unwrap_or(Value::Null)
    }

    /// The run for a client: names, states, amounts, public node keys and outpoints only.
    fn json(&self) -> Value {
        let common = |role: &str, order_id: Option<String>| {
            json!({
                "run": self.name(),
                "role": role,
                "order_id": order_id,
                "state": self.state(),
                "finished": self.finished(),
                "watching": self.watching(),
                "line": self.line(),
            })
        };
        let mut value = match self {
            Run::Seller(run) => {
                let mut value = common("seller", run.order_id().map(|id| id.to_string()));
                value["lease"] = json!(run.contract().map(|contract| contract.id()));
                value["channel"] = json!(run.funding_outpoint());
                value["bond"] = json!(run.bond_status());
                let offered = run.offered();
                value["offered"] = json!({
                    "min_capacity_sat": offered.min_capacity_sat,
                    "max_capacity_sat": offered.max_capacity_sat,
                    "duration_blocks": offered.duration_blocks,
                    "fee_base_sat": offered.fee_base_sat,
                    "fee_ppm": offered.fee_ppm,
                    "upfront_percent": offered.upfront_percent,
                    "bond_sat": offered.bond_sat,
                    "announce": offered.announce,
                    "rules": offered.rules,
                    "expires_at": offered.expires_at,
                });
                if let Some(contract) = run.contract() {
                    value["terms"] =
                        lease_numbers(contract, contract.buyer_node.to_string(), run.watch());
                }
                value
            }
            Run::Buyer(run) => {
                let mut value = common("buyer", Some(run.order_id().to_string()));
                value["lease"] = json!(run.contract().id());
                value["bond"] = json!(run.bond_status());
                value["waiting_on"] = json!(run.waiting_on());
                let contract = run.contract();
                value["terms"] =
                    lease_numbers(contract, contract.seller_node.to_string(), run.watch());
                value
            }
            Run::Ring(run) => {
                let mut value = common("member", run.order_id().map(|id| id.to_string()));
                let terms = run.terms();
                value["terms"] = json!({
                    "capacity_sat": terms.capacity_sat,
                    "bond_sat": terms.bond_sat,
                    "announce": terms.announce,
                    "open_window_secs": terms.open_window_secs,
                    "members": run.ring().map(|ring| ring.members.len()),
                });
                value["ring"] = json!(run.ring().map(|ring| ring.id()));
                value["channel"] = json!(run.opened_outpoint());
                value["ours_confirmed"] = json!(run.our_channel_confirmed());
                value["incoming_confirmed"] = json!(run.incoming_confirmed());
                value["bond"] = json!(run.bond_status());
                value["owed_bond"] = json!(run.owed_status());
                value["waiting_on"] = json!(run.waiting_on());
                value
            }
        };
        if value["order_id"].is_null()
            && let Some(map) = value.as_object_mut()
        {
            map.remove("order_id");
        }
        value
    }

    /// What starting this run would do with the node's money.
    fn ask(&self) -> Ask {
        match self {
            // A seller opens a channel of at most the largest capacity offered, pays nothing but
            // the funding fee, and sells at the offer's price at that capacity (its lowest).
            Run::Seller(run) => {
                let offer = run.offered();
                let proportional =
                    u128::from(offer.max_capacity_sat) * u128::from(offer.fee_ppm) / 1_000_000;
                let fee_sat = u64::try_from(proportional)
                    .ok()
                    .and_then(|fee| fee.checked_add(offer.fee_base_sat));
                Ask {
                    capacity_sat: offer.max_capacity_sat,
                    feerate_sat_vb: run.feerate_sat_vb(),
                    // A price that cannot be computed is taken as nothing, so it passes no floor.
                    sale_ppb_per_block: Some(
                        fee_sat
                            .and_then(|fee| {
                                crate::book::ppb_per_block(
                                    fee,
                                    offer.max_capacity_sat,
                                    offer.duration_blocks,
                                )
                            })
                            .unwrap_or(0),
                    ),
                    ..Ask::default()
                }
            }
            // A buyer pays the lease's whole fee and, where the order asks, locks a bond. A fee
            // that cannot be computed is taken as the largest there is, so it passes no limit.
            Run::Buyer(run) => Ask {
                fee_sat: run.contract().terms.total_fee_sat().unwrap_or(u64::MAX),
                capacity_sat: 0,
                bond_sat: run.bond_sat(),
                peer: Some(run.contract().seller_node.to_string()),
                ..Ask::default()
            },
            // A ring member opens one channel, paying its funding fee, and, where the ring asks,
            // locks a bond.
            Run::Ring(run) => Ask {
                capacity_sat: run.terms().capacity_sat,
                bond_sat: run.terms().bond_sat,
                feerate_sat_vb: run.feerate_sat_vb(),
                ..Ask::default()
            },
        }
    }
}

/// A lease's numbers for a client: what the contract says, the other party's node, and the
/// height the lease ends at once its channel is confirmed.
fn lease_numbers(contract: &LeaseContract, peer: String, watch: &Watch) -> Value {
    let terms = &contract.terms;
    json!({
        "capacity_sat": terms.capacity_sat,
        "fee_sat": terms.total_fee_sat().ok(),
        "upfront_sat": terms.upfront_sat().ok(),
        "duration_blocks": terms.duration_blocks,
        "announce": terms.announce,
        "peer": peer,
        "ends_at_height": watch
            .funding_height
            .map(|height| height.saturating_add(terms.duration_blocks)),
    })
}

/// A run under way, with what it needs to go on.
struct Entry {
    name: String,
    run: Run,
    link: Link,
    /// What the run may do with the node; `None` for a run a one-shot command started.
    grant: Option<Grant>,
    /// The line last told to watchers.
    line: String,
    /// Why the run is not stepped any more, if it could not be saved.
    halted: Option<String>,
    /// For a run that is only watched: not stepped again before this time (unix seconds).
    next_look: u64,
}

struct Watcher {
    run: Option<String>,
    lines: Lines,
    /// The paired page that watches (its session key), or `None` for the operator.
    page: Option<String>,
}

/// Who asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The operator, on the agent's own socket.
    Owner,
    /// A paired page through the web door, by its session key (64 hex). Until the engine has
    /// admitted the request ([`Engine::handle`]), the key may be one nobody paired.
    Page(String),
}

impl Caller {
    fn page(&self) -> Option<&str> {
        match self {
            Caller::Owner => None,
            Caller::Page(key) => Some(key),
        }
    }
}

/// A request as a socket hands it over.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// From the agent's own socket.
    Owner(Request),
    /// From the web door: the checked event and the request it carries.
    Page(Signed, Request),
}

/// A page's pairing request, waiting for the operator.
struct Pending {
    code: String,
    key: String,
    label: String,
    at: u64,
}

/// What the engine remembers of a key that asked through the web door, paired or not: the ids
/// of its recent requests (a second one with the same id is a replay) and when it asked.
#[derive(Default)]
struct PageMemory {
    seen: Vec<(EventId, u64)>,
    asked: Vec<u64>,
    last_start: u64,
}

/// The most keys the engine remembers at once; a new one beyond that is told to wait.
const MAX_PAGE_MEMORIES: usize = 4096;

/// The daemon's state. See the module's text.
pub struct Engine {
    config: Config,
    wallet: Wallet,
    /// The liquidity identity's public key: whom a page's request must name.
    identity: PublicKey,
    backend: Box<dyn LightningBackend>,
    client: Client,
    instances: Vec<InstanceConfig>,
    unresolved: Vec<InstanceEntry>,
    leases: LeaseStore,
    rings: RingStore,
    policy: PolicyStore,
    sessions: SessionStore,
    entries: Vec<Entry>,
    watchers: Vec<Watcher>,
    ticks: u32,
    /// When this engine started, unix seconds: a page's request dated before it is refused.
    started: u64,
    /// Until when pages may ask to be paired, unix seconds.
    pairing_until: u64,
    pending: Vec<Pending>,
    pages: BTreeMap<String, PageMemory>,
    log: Box<dyn FnMut(String)>,
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::internal(error.to_string())
}

/// A run as a page is shown it: without the one-line summary (it names the channel's outpoint)
/// and without the outpoint of a channel that is not announced, which the graph does not show
/// either. Anything that is not a run passes as it is.
fn page_view(fields: &Value) -> Value {
    let mut fields = fields.clone();
    if let Some(map) = fields.as_object_mut()
        && map.contains_key("role")
    {
        map.remove("line");
        let announced = map
            .get("terms")
            .and_then(|terms| terms.get("announce"))
            .or_else(|| {
                map.get("offered")
                    .and_then(|offered| offered.get("announce"))
            })
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !announced && map.contains_key("channel") {
            map.insert("channel".into(), Value::Null);
        }
    }
    fields
}

fn send_line(lines: &Lines, line: String) {
    // A client that does not read loses lines; the engine does not wait for it.
    let _ = lines.try_send(line);
}

/// True when `event` is addressed to `key` by a `p` tag.
fn addressed_to(event: &Event, key: &PublicKey) -> bool {
    let hex = key.to_hex();
    event.tags.iter().any(|tag| {
        let tag = tag.as_slice();
        tag.first().map(String::as_str) == Some("p") && tag.get(1) == Some(&hex)
    })
}

async fn step_run(
    run: &mut Run,
    link: &mut Link,
    backend: &dyn LightningBackend,
    leases: &LeaseStore,
    rings: &RingStore,
    now: u64,
) -> Result<StepReport, StoreError> {
    match run {
        Run::Seller(run) => step_seller(run, link, backend, leases, now).await,
        Run::Buyer(run) => step_buyer(run, link, backend, leases, now).await,
        Run::Ring(run) => ring_link::step(run, link, backend, rings, now).await,
    }
}

impl Engine {
    /// Opens the stores, connects to the relays, settles each instance's proof of work, and
    /// takes up every stored run that is not finished. `log` gets a line for each thing worth
    /// telling: a run's change of state, a problem. It is never given a secret.
    pub async fn start(
        config: Config,
        wallet: Wallet,
        backend: Box<dyn LightningBackend>,
        log: Box<dyn FnMut(String)>,
    ) -> Result<Self, String> {
        let leases = lease_cmd::store(&config)?;
        let rings = ring_cmd::store(&config)?;
        let policy = PolicyStore::open(&config.policy_dir()).map_err(|e| e.to_string())?;
        let sessions = SessionStore::open(&config.sessions_dir()).map_err(|e| e.to_string())?;
        let identity = wallet.liquidity().map_err(|e| e.to_string())?.public_key();
        let client = relay::connected(&config.relays).await?;
        let mut engine = Engine {
            unresolved: config.instances.clone(),
            config,
            wallet,
            identity,
            backend,
            client,
            instances: Vec::new(),
            leases,
            rings,
            policy,
            sessions,
            entries: Vec::new(),
            watchers: Vec::new(),
            ticks: 0,
            started: unix_now(),
            pairing_until: 0,
            pending: Vec::new(),
            pages: BTreeMap::new(),
            log,
        };
        engine.resolve_instances().await;
        engine.resume()?;
        engine.tidy(unix_now());
        Ok(engine)
    }

    /// Settles the proof of work of the instances that are still without one, and takes up
    /// their stored runs.
    async fn resolve_instances(&mut self) {
        let waiting = std::mem::take(&mut self.unresolved);
        for entry in waiting {
            match relay::resolve_instance(&entry, &self.config).await {
                Ok(instance) => {
                    (self.log)(format!(
                        "instance {} (proof of work: {} bits)",
                        instance.pubkey.to_hex(),
                        instance.pow
                    ));
                    self.instances.push(instance);
                }
                Err(e) => {
                    (self.log)(format!(
                        "problem: instance {} is not usable yet: {e}",
                        entry.pubkey.to_hex()
                    ));
                    self.unresolved.push(entry);
                }
            }
        }
    }

    /// Takes up every stored run that is not finished, not already under way, and on an
    /// instance whose proof of work is settled.
    fn resume(&mut self) -> Result<(), String> {
        let leases = self.leases.load().map_err(|e| e.to_string())?;
        let rings = self.rings.load().map_err(|e| e.to_string())?;
        for (file, why) in leases.problems.iter().chain(rings.problems.iter()) {
            (self.log)(format!("refused {file}: {why}"));
        }
        let runs = leases
            .sellers
            .into_iter()
            .map(Run::Seller)
            .chain(leases.buyers.into_iter().map(Run::Buyer))
            .chain(rings.runs.into_iter().map(Run::Ring));
        for mut run in runs {
            let name = run.name();
            if run.finished() || self.entries.iter().any(|entry| entry.name == name) {
                continue;
            }
            // A preimage released by the process before may never have left it.
            if let Run::Buyer(buyer) = &mut run {
                buyer.resumed();
            }
            let Some(instance) = self
                .instances
                .iter()
                .find(|instance| instance.pubkey == run.instance())
            else {
                continue;
            };
            let link = self.link(instance, run.trade_index())?;
            let grant = self.policy.grant(&name).map_err(|e| e.to_string())?;
            let line = run.line();
            (self.log)(line.clone());
            self.entries.push(Entry {
                name,
                run,
                link,
                grant,
                line,
                halted: None,
                next_look: 0,
            });
        }
        Ok(())
    }

    /// Puts away the runs that finished long ago, with their grants: moved into `done`
    /// directories, never deleted. They are no longer loaded at a start, listed, or counted.
    fn tidy(&mut self, now: u64) {
        let moved = self
            .leases
            .put_away_finished(KEEP_FINISHED_SECS, now)
            .and_then(|mut leases| {
                leases.extend(self.rings.put_away_finished(KEEP_FINISHED_SECS, now)?);
                Ok(leases)
            });
        match moved {
            Ok(moved) => {
                for run in &moved {
                    if let Err(e) = self.policy.put_away_grant(run) {
                        (self.log)(format!("problem: {e}"));
                    }
                }
                if !moved.is_empty() {
                    (self.log)(format!(
                        "put away {} runs that finished long ago",
                        moved.len()
                    ));
                }
            }
            Err(e) => (self.log)(format!("problem: finished runs could not be put away: {e}")),
        }
    }

    fn link(&self, instance: &InstanceConfig, trade_index: u32) -> Result<Link, String> {
        let identity = self.wallet.liquidity().map_err(|e| e.to_string())?;
        let trade = self
            .wallet
            .liquidity_trade(trade_index)
            .map_err(|e| e.to_string())?;
        Ok(Link::shared(
            self.client.clone(),
            identity,
            trade,
            instance.pubkey,
            instance.pow,
        )
        .with_outcomes(self.config.policy.publish_outcomes))
    }

    fn tell(&mut self, run: &str, status: &str, fields: Value) {
        let page_line = api::status_line(status, page_view(&fields));
        let line = api::status_line(status, fields);
        self.watchers.retain(|watcher| {
            if watcher.run.as_deref().is_some_and(|wanted| wanted != run) {
                return !watcher.lines.is_closed();
            }
            let line = if watcher.page.is_some() {
                &page_line
            } else {
                &line
            };
            watcher.lines.try_send(line.clone()).is_ok()
        });
    }

    /// One round: ask the relays once for everything addressed to the runs under way, then
    /// step each of them.
    pub async fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if !self.unresolved.is_empty() && self.ticks.is_multiple_of(RESOLVE_EVERY_TICKS) {
            self.resolve_instances().await;
            if let Err(e) = self.resume() {
                (self.log)(format!("problem: stored runs: {e}"));
            }
        }
        let now = unix_now();
        if self.ticks.is_multiple_of(TIDY_EVERY_TICKS) {
            self.tidy(now);
        }
        let keys: Vec<PublicKey> = self
            .entries
            .iter()
            .filter(|entry| entry.halted.is_none() && !entry.run.finished())
            .map(|entry| entry.link.trade_pubkey())
            .collect();
        if keys.is_empty() {
            return;
        }
        let filter = Filter::new()
            .kind(Kind::PrivateDirectMessage)
            .pubkeys(keys)
            .since(Timestamp::from(now.saturating_sub(RESUME_LOOKBACK_SECS)));
        match self
            .client
            .fetch_events(filter)
            .timeout(RELAY_TIMEOUT)
            .await
        {
            Ok(events) => {
                for entry in &mut self.entries {
                    let key = entry.link.trade_pubkey();
                    entry.link.deliver(
                        events
                            .iter()
                            .filter(|event| addressed_to(event, &key))
                            .cloned()
                            .collect(),
                    );
                }
            }
            Err(e) => {
                // Nothing new this round; the runs still look at the node.
                (self.log)(format!("problem: relays: {e}"));
                for entry in &mut self.entries {
                    entry.link.deliver(Vec::new());
                }
            }
        }
        for index in 0..self.entries.len() {
            self.step(index, now).await;
        }
        // Finished runs stay on disk; the engine lets go of them.
        self.entries
            .retain(|entry| entry.halted.is_some() || !entry.run.finished());
    }

    async fn step(&mut self, index: usize, now: u64) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.halted.is_some() || entry.run.finished() {
            return;
        }
        if entry.run.watching() {
            if now < entry.next_look {
                return;
            }
            entry.next_look = now.saturating_add(WATCH_EVERY_SECS);
        }
        let stepped = match entry.grant.take() {
            Some(grant) => {
                let guard = Guard::new(
                    self.backend.as_ref(),
                    &self.policy,
                    &self.config.policy,
                    grant,
                );
                let stepped = step_run(
                    &mut entry.run,
                    &mut entry.link,
                    &guard,
                    &self.leases,
                    &self.rings,
                    now,
                )
                .await;
                entry.grant = Some(guard.into_grant());
                stepped
            }
            None => {
                step_run(
                    &mut entry.run,
                    &mut entry.link,
                    self.backend.as_ref(),
                    &self.leases,
                    &self.rings,
                    now,
                )
                .await
            }
        };
        let name = entry.name.clone();
        let mut said = Vec::new();
        match stepped {
            Ok(report) => {
                for problem in report.problems {
                    said.push(("problem", json!({"run": name, "problem": problem})));
                    (self.log)(format!("problem: {name}: {problem}"));
                }
            }
            Err(e) => {
                // A run that cannot be saved must not go on: it could repeat what it did.
                let why = format!("cannot be saved, so it is stopped: {e}");
                entry.halted = Some(why.clone());
                said.push(("problem", json!({"run": name, "problem": why})));
                (self.log)(format!("problem: {name}: {why}"));
            }
        }
        let line = entry.run.line();
        if line != entry.line {
            entry.line = line.clone();
            said.push(("run", entry.run.json()));
            (self.log)(line);
        }
        for (status, fields) in said {
            self.tell(&name, status, fields);
        }
    }

    /// Answers one request. Every line it has to say goes to `lines`; the last one is the
    /// final line, except for `watch`, which has none. A page's request is admitted first
    /// ([`Self::admit`]); the operator's is not, except that it cannot ask what only a page asks.
    pub async fn handle(&mut self, incoming: Incoming, lines: Lines) {
        let now = unix_now();
        let admitted = match incoming {
            Incoming::Owner(request) => {
                session::owner_may(&request.call).map(|()| (Caller::Owner, request))
            }
            Incoming::Page(signed, request) => self
                .admit(&signed, &request, now)
                .map(|()| (Caller::Page(signed.key.to_hex()), request)),
        };
        let (caller, request) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                send_line(&lines, api::error_line(&error));
                return;
            }
        };
        match &request.call {
            Call::Watch(run) => {
                if let Some(key) = caller.page() {
                    self.watchers.retain(|watcher| !watcher.lines.is_closed());
                    let open = self
                        .watchers
                        .iter()
                        .filter(|watcher| watcher.page.as_deref() == Some(key))
                        .count();
                    if open >= session::MAX_PAGE_WATCHES {
                        let busy = ApiError::new(
                            Code::Busy,
                            "this page already watches as much as it may; close a watch first",
                        );
                        send_line(&lines, api::error_line(&busy));
                        return;
                    }
                }
                self.watch(run.clone(), lines, caller.page().map(str::to_string));
                return;
            }
            Call::Orders(_) | Call::Record(_) => {
                self.read(request, lines);
                return;
            }
            _ => {}
        }
        let line = match self.call(&caller, &request, &lines, now).await {
            Ok(result) => api::ok_line(result),
            Err(error) => api::error_line(&error),
        };
        send_line(&lines, line);
    }

    /// Whether a page's request may be taken up: fresh, never seen before, within the page's
    /// rate, addressed to this agent, from a key the operator paired whose session is live, and
    /// within that session's scope. Only `pair.request` and `pair.status` are answered for a key
    /// nobody paired (and may leave out whom they are for, since a page learns that by pairing).
    fn admit(&mut self, signed: &Signed, request: &Request, now: u64) -> Result<(), ApiError> {
        session::fresh(signed.created_at, now, self.started)?;
        let key = signed.key.to_hex();
        let forget_before = now.saturating_sub(session::MAX_AGE_SECS + session::MAX_AHEAD_SECS);
        let rate_since = now.saturating_sub(session::RATE_WINDOW_SECS);
        self.pages.retain(|_, memory| {
            memory.seen.retain(|(_, at)| *at >= forget_before);
            memory.asked.retain(|at| *at >= rate_since);
            !memory.seen.is_empty() || !memory.asked.is_empty() || memory.last_start > rate_since
        });
        if !self.pages.contains_key(&key) && self.pages.len() >= MAX_PAGE_MEMORIES {
            return Err(ApiError::new(
                Code::Busy,
                "too many pages are asking at once; try again in a minute",
            ));
        }
        let memory = self.pages.entry(key.clone()).or_default();
        if memory.seen.iter().any(|(id, _)| *id == signed.id) {
            return Err(ApiError::refused(
                "this request was seen before; a page signs every request anew",
            ));
        }
        if memory.asked.len() >= session::RATE_REQUESTS {
            return Err(ApiError::new(
                Code::Busy,
                "this page asks too often; try again in a minute",
            ));
        }
        memory.seen.push((signed.id, now));
        memory.asked.push(now);
        let pairing = matches!(request.call, Call::PairRequest(_) | Call::PairStatus);
        let addressed = signed.to.contains(&self.identity) || (pairing && signed.to.is_empty());
        if !addressed {
            return Err(ApiError::refused(
                "the request is not addressed to this agent",
            ));
        }
        if pairing {
            return Ok(());
        }
        let live = self
            .sessions
            .get(&key)
            .map_err(internal)?
            .filter(|live| live.live(now))
            .ok_or_else(|| {
                ApiError::new(
                    Code::Unpaired,
                    "this page is not paired with the agent, or no longer is: pair it again",
                )
            })?;
        session::page_may(&request.call, live.scope)
    }

    /// What a request that only reads the relays needs, copied out (see [`super::reads`]).
    fn reader(&self) -> Reader {
        Reader {
            client: self.client.clone(),
            instances: self
                .instances
                .iter()
                .map(|instance| instance.pubkey)
                .collect(),
            network: self.config.network,
        }
    }

    /// Answers `orders` or `record` in a task of its own, so that a slow relay holds up no run
    /// and no other request. The instance a request names is settled here first.
    fn read(&self, request: Request, lines: Lines) {
        let only = match &request.call {
            Call::Orders(params) => match params.instance.as_deref() {
                Some(named) => match self.instance(Some(named)) {
                    Ok(instance) => Some(instance.pubkey),
                    Err(error) => {
                        send_line(&lines, api::error_line(&error));
                        return;
                    }
                },
                None => None,
            },
            _ => None,
        };
        let reader = self.reader();
        tokio::spawn(async move {
            let answer = match &request.call {
                Call::Orders(params) => reader.orders(params, only).await,
                Call::Record(node) => reader.record(node).await,
                _ => Err(ApiError::internal("not a request that only reads")),
            };
            let line = match answer {
                Ok(result) => api::ok_line(result),
                Err(error) => api::error_line(&error),
            };
            send_line(&lines, line);
        });
    }

    fn watch(&mut self, run: Option<String>, lines: Lines, page: Option<String>) {
        // What is under way now, then every change.
        for entry in &self.entries {
            if run.as_deref().is_none_or(|wanted| wanted == entry.name) {
                let mut fields = entry.run.json();
                if page.is_some() {
                    fields = page_view(&fields);
                }
                send_line(&lines, api::status_line("run", fields));
            }
        }
        send_line(&lines, api::status_line("watching", Value::Null));
        self.watchers.push(Watcher { run, lines, page });
    }

    async fn call(
        &mut self,
        caller: &Caller,
        request: &Request,
        lines: &Lines,
        now: u64,
    ) -> Result<Value, ApiError> {
        match &request.call {
            Call::Status => self.status(caller).await,
            Call::LeaseOffer(_) | Call::LeaseTake(_) | Call::RingCreate(_) | Call::RingJoin(_) => {
                if let Some(key) = caller.page() {
                    self.page_may_start(key, now)?;
                }
                self.start_run(request, None, lines, caller).await
            }
            Call::LeaseCancel(run) => self.cancel(run, true, caller).await,
            Call::RingCancel(run) => self.cancel(run, false, caller).await,
            Call::LeaseList => self.list(true, caller),
            Call::RingList => self.list(false, caller),
            Call::BindingPublish => self.publish_binding().await,
            Call::Approvals => {
                let parked = self.live_parked(now)?;
                let shown: Vec<Value> = parked
                    .iter()
                    .filter(|parked| match caller {
                        Caller::Owner => true,
                        Caller::Page(key) => parked.session.as_deref() == Some(key.as_str()),
                    })
                    .map(|parked| self.parked_json(parked))
                    .collect();
                Ok(json!({"approvals": shown}))
            }
            Call::Approve(approval) => {
                let parked = self
                    .live_parked(now)?
                    .into_iter()
                    .find(|parked| parked.approval == *approval)
                    .ok_or_else(|| ApiError::not_found("no such approval, or it lapsed"))?;
                // The words are read again exactly as they were parked.
                let call = api::understand(&parked.method, &parked.params)?;
                let again = Request {
                    method: parked.method.clone(),
                    params: parked.params.clone(),
                    call,
                };
                let result = self
                    .start_run(&again, Some(&parked), lines, &Caller::Owner)
                    .await?;
                self.policy.unpark(approval).map_err(internal)?;
                Ok(result)
            }
            Call::Deny(approval) => {
                self.policy
                    .parked(approval)
                    .map_err(internal)?
                    .ok_or_else(|| ApiError::not_found("no such approval"))?;
                self.policy.unpark(approval).map_err(internal)?;
                Ok(json!({"denied": approval}))
            }
            Call::PairOpen(secs) => {
                self.pairing_until = now.saturating_add(*secs);
                (self.log)(format!("pairing is open for {secs} s"));
                Ok(json!({"open_until": self.pairing_until}))
            }
            Call::PairList => {
                self.pending
                    .retain(|pending| pending.at + session::PENDING_SECS > now);
                // Never the codes: the operator types the one the page shows, which is the
                // comparison. A code picked from a list here could be another page's.
                Ok(json!({
                    "open": now < self.pairing_until,
                    "open_until": self.pairing_until,
                    "waiting": self.pending.iter().map(|pending| json!({
                        "label": pending.label,
                        "at": pending.at,
                    })).collect::<Vec<_>>(),
                }))
            }
            Call::PairApprove(approve) => self.pair_approve(approve, now),
            Call::PairDeny(code) => {
                let before = self.pending.len();
                self.pending.retain(|pending| pending.code != *code);
                if self.pending.len() == before {
                    return Err(ApiError::not_found("no page waits with that code"));
                }
                Ok(json!({"denied": true}))
            }
            Call::Sessions => {
                let sessions = self.sessions.all().map_err(internal)?;
                Ok(json!({
                    "sessions": sessions.iter().map(|session| json!({
                        "session": session.key,
                        "label": session.label,
                        "scope": session.scope,
                        "created_at": session.created_at,
                        "expires_at": session.expires_at,
                        "ended_at": session.ended_at,
                        "live": session.live(now),
                    })).collect::<Vec<_>>(),
                }))
            }
            Call::Revoke(key) => self.end_session(key, now),
            Call::PairRequest(label) => match caller {
                Caller::Page(key) => self.pair_request(key, label.as_deref(), now),
                Caller::Owner => Err(ApiError::internal("only a page asks this")),
            },
            Call::PairStatus => match caller {
                Caller::Page(key) => self.pair_status(key, now),
                Caller::Owner => Err(ApiError::internal("only a page asks this")),
            },
            Call::PairEnd => match caller {
                Caller::Page(key) => self.end_session(key, now),
                Caller::Owner => Err(ApiError::internal("only a page asks this")),
            },
            // Answered in `handle`.
            Call::Watch(_) => Err(ApiError::internal("watch has no answer")),
            Call::Orders(_) | Call::Record(_) => {
                Err(ApiError::internal("answered beside the engine"))
            }
        }
    }

    /// Whether a page may ask for one more run now: not while it has as many waiting for
    /// approval as it may, and not twice within [`session::START_EVERY_SECS`]. Asked before
    /// anything is read or sent for the run.
    fn page_may_start(&mut self, key: &str, now: u64) -> Result<(), ApiError> {
        let waiting = self
            .live_parked(now)?
            .iter()
            .filter(|parked| parked.session.as_deref() == Some(key))
            .count();
        if waiting >= session::MAX_PARKED_PER_SESSION {
            return Err(ApiError::refused(format!(
                "this page has {waiting} requests waiting for approval already; they are \
                 approved or denied where the agent runs"
            )));
        }
        let memory = self.pages.entry(key.to_string()).or_default();
        if memory.last_start.saturating_add(session::START_EVERY_SECS) > now {
            return Err(ApiError::new(
                Code::Busy,
                "this page asked for a run a moment ago; wait a few seconds",
            ));
        }
        memory.last_start = now;
        Ok(())
    }

    /// The parked requests that still wait. A page's request lapses after
    /// [`session::PAGE_PARKED_SECS`], or when its session ended; those are dropped here.
    fn live_parked(&mut self, now: u64) -> Result<Vec<Parked>, ApiError> {
        let mut live = Vec::new();
        for parked in self.policy.all_parked().map_err(internal)? {
            let lapsed = match &parked.session {
                None => false,
                Some(key) => {
                    let session_live = self
                        .sessions
                        .get(key)
                        .map_err(internal)?
                        .is_some_and(|session| session.live(now));
                    !session_live || parked.at.saturating_add(session::PAGE_PARKED_SECS) <= now
                }
            };
            if lapsed {
                self.policy.unpark(&parked.approval).map_err(internal)?;
                (self.log)(format!(
                    "a page's parked request {} lapsed",
                    parked.approval
                ));
            } else {
                live.push(parked);
            }
        }
        Ok(live)
    }

    /// A parked request as it is listed: as stored, and, for one a page asked, the page's label.
    fn parked_json(&self, parked: &Parked) -> Value {
        let mut value = json!(parked);
        if let Some(key) = &parked.session {
            let label = self
                .sessions
                .get(key)
                .ok()
                .flatten()
                .map(|session| session.label);
            value["asked_by"] = json!({"session": key, "label": label});
        }
        value
    }

    /// A page asks to be paired. Only while the operator holds the window open; a page that
    /// already waits is given its code again; none is ever pushed out by a newer one.
    fn pair_request(
        &mut self,
        key: &str,
        label: Option<&str>,
        now: u64,
    ) -> Result<Value, ApiError> {
        if self
            .sessions
            .get(key)
            .map_err(internal)?
            .is_some_and(|session| session.live(now))
        {
            return self.pair_status(key, now);
        }
        self.pending
            .retain(|pending| pending.at + session::PENDING_SECS > now);
        if let Some(pending) = self.pending.iter().find(|pending| pending.key == key) {
            return Ok(json!({
                "paired": false,
                "pending": true,
                "code": pending.code,
                "agent": self.identity.to_hex(),
            }));
        }
        if now >= self.pairing_until {
            return Err(ApiError::refused(
                "pairing is not open: run `bipnip pair open` where the agent runs, then ask again",
            ));
        }
        if self.pending.len() >= session::MAX_PENDING {
            return Err(ApiError::new(
                Code::Busy,
                "as many pages as may wait to be paired are waiting; try again later",
            ));
        }
        let code = loop {
            let code = session::pairing_code()?;
            if self.pending.iter().all(|pending| pending.code != code) {
                break code;
            }
        };
        let label = label.unwrap_or_default().to_string();
        (self.log)(format!("a page asks to be paired ({label:?})"));
        self.pending.push(Pending {
            code: code.clone(),
            key: key.to_string(),
            label,
            at: now,
        });
        Ok(json!({
            "paired": false,
            "pending": true,
            "code": code,
            "agent": self.identity.to_hex(),
        }))
    }

    /// Where a page stands: paired (and how), waiting (with its code), or neither.
    fn pair_status(&self, key: &str, now: u64) -> Result<Value, ApiError> {
        if let Some(session) = self
            .sessions
            .get(key)
            .map_err(internal)?
            .filter(|session| session.live(now))
        {
            return Ok(json!({
                "paired": true,
                "label": session.label,
                "scope": session.scope,
                "expires_at": session.expires_at,
                "agent": self.identity.to_hex(),
            }));
        }
        let pending = self
            .pending
            .iter()
            .find(|pending| pending.key == key && pending.at + session::PENDING_SECS > now);
        Ok(json!({
            "paired": false,
            "pending": pending.is_some(),
            "code": pending.map(|pending| pending.code.clone()),
            "open": now < self.pairing_until,
            "agent": self.identity.to_hex(),
        }))
    }

    /// The operator pairs the page that shows `code`.
    fn pair_approve(&mut self, approve: &PairApprove, now: u64) -> Result<Value, ApiError> {
        self.pending
            .retain(|pending| pending.at + session::PENDING_SECS > now);
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.code == approve.code)
        else {
            return Err(ApiError::not_found(
                "no page waits with that code: type it as the page shows it, while it waits",
            ));
        };
        let pending = self.pending.remove(index);
        let session = session::Session {
            key: pending.key,
            label: approve.label.clone().unwrap_or(pending.label),
            scope: approve.scope,
            created_at: now,
            expires_at: now.saturating_add(approve.expires_days.saturating_mul(86_400)),
            ended_at: None,
        };
        self.sessions.save(&session).map_err(internal)?;
        (self.log)(format!(
            "paired a page ({:?}, {:?}) until {}",
            session.label, session.scope, session.expires_at
        ));
        Ok(json!({
            "session": session.key,
            "label": session.label,
            "scope": session.scope,
            "expires_at": session.expires_at,
        }))
    }

    /// Ends a page's session (the operator revoked it, or the page ended it): its watches close,
    /// what it asked for that still waits is dropped. Runs it asked for that were approved go on.
    fn end_session(&mut self, key: &str, now: u64) -> Result<Value, ApiError> {
        let mut session = self
            .sessions
            .get(key)
            .map_err(internal)?
            .ok_or_else(|| ApiError::not_found("no such session"))?;
        if session.ended_at.is_none() {
            session.ended_at = Some(now);
            self.sessions.save(&session).map_err(internal)?;
        }
        self.watchers
            .retain(|watcher| watcher.page.as_deref() != Some(key));
        self.pending.retain(|pending| pending.key != key);
        let mut dropped = 0;
        for parked in self.policy.all_parked().map_err(internal)? {
            if parked.session.as_deref() == Some(key) {
                self.policy.unpark(&parked.approval).map_err(internal)?;
                dropped += 1;
            }
        }
        (self.log)(format!(
            "ended the session of the page {:?}; {dropped} of its requests no longer wait",
            session.label
        ));
        Ok(json!({"ended": key, "dropped": dropped}))
    }

    fn under_way(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .filter(|entry| !entry.run.finished() && !entry.run.watching())
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn usage(&self, now: u64) -> Result<Usage, ApiError> {
        let used = self.policy.used_since(now).map_err(internal)?;
        Ok(Usage {
            fee_day_sat: used.fee_sat,
            capacity_day_sat: used.capacity_sat,
            bond_day_sat: used.bond_sat,
            runs: self.under_way(),
        })
    }

    async fn status(&mut self, caller: &Caller) -> Result<Value, ApiError> {
        let node = match self.backend.node_info().await {
            Ok(info) => json!({
                "pubkey": info.node_pubkey,
                "network": info.network,
                "block_height": info.block_height,
                "synced": info.synced,
            }),
            Err(e) => json!({"error": e.to_string()}),
        };
        let identity = self.wallet.liquidity().map_err(internal)?.public_key();
        let mut by_state: BTreeMap<String, u32> = BTreeMap::new();
        for entry in &self.entries {
            let role = match &entry.run {
                Run::Seller(_) => "seller",
                Run::Buyer(_) => "buyer",
                Run::Ring(_) => "member",
            };
            let state = entry.run.state();
            let key = format!("{role}:{}", state.as_str().unwrap_or("?"));
            *by_state.entry(key).or_default() += 1;
        }
        let usage = self.usage(unix_now())?;
        let policy = &self.config.policy;
        let mut status = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "network": self.config.network.as_str(),
            "identity": identity.to_bech32().unwrap_or_else(|_| identity.to_hex()),
            "node": node,
            "instances": self.instances.iter().map(|instance| json!({
                "pubkey": instance.pubkey.to_hex(),
                "pow": instance.pow,
            })).collect::<Vec<_>>(),
            "instances_not_ready": self.unresolved.iter()
                .map(|entry| entry.pubkey.to_hex()).collect::<Vec<_>>(),
            "runs": {
                "under_way": usage.runs,
                "watching": self.entries.iter().filter(|entry| entry.run.watching()).count(),
                "halted": self.entries.iter().filter(|entry| entry.halted.is_some()).count(),
                "by_state": by_state,
            },
            "approvals": self.policy.all_parked().map_err(internal)?.len(),
            "last_day": {
                "fee_sat": usage.fee_day_sat,
                "capacity_sat": usage.capacity_day_sat,
            },
            "policy": {
                "max_fee_per_lease_sat": policy.max_fee_per_lease_sat,
                "max_fee_per_day_sat": policy.max_fee_per_day_sat,
                "max_capacity_per_day_sat": policy.max_capacity_per_day_sat,
                "max_bond_sat": policy.max_bond_sat,
                "max_runs": policy.max_runs,
                "allowed_peers": policy.allowed_peers,
            },
        });
        // A page is not told the limits, what is left of them today, or how many requests
        // others are waiting with: only its own.
        if let Caller::Page(key) = caller
            && let Some(map) = status.as_object_mut()
        {
            map.remove("policy");
            map.remove("last_day");
            let own = self
                .policy
                .all_parked()
                .map_err(internal)?
                .iter()
                .filter(|parked| parked.session.as_deref() == Some(key.as_str()))
                .count();
            map.insert("approvals".into(), json!(own));
        }
        Ok(status)
    }

    /// The instance a request names, or the only one there is.
    fn instance(&self, named: Option<&str>) -> Result<InstanceConfig, ApiError> {
        match named {
            Some(text) => {
                let key = api::instance_key(text)?;
                self.instances
                    .iter()
                    .find(|instance| instance.pubkey == key)
                    .cloned()
                    .ok_or_else(|| {
                        ApiError::not_found("that instance is not configured, or not usable yet")
                    })
            }
            None => match self.instances.as_slice() {
                [only] => Ok(only.clone()),
                [] => Err(ApiError::refused(
                    "no instance is usable (none configured, or none reachable yet)",
                )),
                _ => Err(ApiError::bad_request(
                    "several instances are configured: name one with `instance`",
                )),
            },
        }
    }

    /// Starts the run a request asks for: prepares it with every check the one-shot command
    /// makes, weighs it against the policy, and only then stores it and sends its first
    /// message. `approved` is the parked request this is the approval of, if it is one. What a
    /// page asks for is always parked: it goes ahead only when the operator approves it where the
    /// agent runs, which shows what it would really do.
    async fn start_run(
        &mut self,
        request: &Request,
        approved: Option<&Parked>,
        lines: &Lines,
        caller: &Caller,
    ) -> Result<Value, ApiError> {
        let named = match &request.call {
            Call::LeaseOffer(params) => params.instance.as_deref(),
            Call::LeaseTake(params) => params.instance.as_deref(),
            Call::RingCreate(params) => params.instance.as_deref(),
            Call::RingJoin(params) => params.instance.as_deref(),
            _ => return Err(ApiError::internal("not a request that starts a run")),
        };
        let instance = self.instance(named)?;
        let session = Session {
            config: &self.config,
            wallet: &self.wallet,
            backend: self.backend.as_ref(),
            instance: Some(&instance),
            hub: Some(&self.client),
            caller_checks_limits: true,
        };
        // Nothing is published, stored or sent by the preparation.
        let Prepared {
            mut run,
            first,
            link,
            binding,
        } = match &request.call {
            Call::LeaseOffer(params) => prepare_offer(&session, params).await?,
            Call::LeaseTake(params) => prepare_take(&session, params).await?,
            Call::RingCreate(params) => prepare_create(&session, params).await?,
            Call::RingJoin(params) => prepare_join(&session, params).await?,
            _ => return Err(ApiError::internal("not a request that starts a run")),
        };
        let name = run.name();
        if self.entries.iter().any(|entry| entry.name == name) {
            return Err(ApiError::refused("this run is under way already"));
        }
        let ask = run.ask();
        let now = unix_now();
        match approved {
            None => {
                let mut reasons = over(&self.config.policy, &ask, &self.usage(now)?);
                if let Run::Buyer(buyer) = &run {
                    let policy = &self.config.policy;
                    if policy.min_seller_channels > 0 || policy.min_seller_capacity_sat > 0 {
                        let seller = buyer.contract().seller_node.to_string();
                        let graph = self
                            .backend
                            .graph_node(&seller)
                            .await
                            .map_err(|e| ApiError::new(Code::Node, e.to_string()))?;
                        reasons.extend(seller_reasons(policy, graph));
                    }
                }
                if caller.page().is_some() {
                    reasons.insert(
                        0,
                        "asked from a paired web page: what a page asks for waits for `bipnip \
                         approve` where the agent runs"
                            .to_string(),
                    );
                }
                if !reasons.is_empty() {
                    let session = caller.page().map(str::to_string);
                    return Err(self.park(request, ask, reasons, now, session));
                }
            }
            // An approval is for the request as it was shown. If the order now asks for more,
            // it is a different request.
            Some(parked) if !ask.within(&parked.ask) => {
                return Err(ApiError::refused(
                    "what this request would do has changed since it was parked (the order \
                     asks for more, or names another node); deny it and ask again",
                ));
            }
            Some(_) => {}
        }
        // Only now, with the request allowed, does an offer's node binding go out: it links the
        // node to our liquidity identity in public, which a parked request must not have done.
        // If no relay takes it, nothing is stored and the request can be asked again.
        if let Some(binding) = &binding {
            relay::publish(binding, &self.config.relays)
                .await
                .map_err(|e| ApiError::new(Code::Relay, e))?;
        }
        // The grant is on disk before the run exists, so a run of the daemon's is never
        // without one.
        let mut grant = Grant::new(name.clone(), now, ask, approved.is_some());
        grant.session = approved.and_then(|parked| parked.session.clone());
        grant.approval = approved.map(|parked| parked.approval.clone());
        self.policy.save_grant(&grant).map_err(internal)?;
        let report = match (&mut run, &first) {
            (Run::Seller(run), First::Lease(first)) => {
                launch_seller(run, first, &link, &self.leases).await
            }
            (Run::Buyer(run), First::Lease(first)) => {
                launch_buyer(run, first, &link, &self.leases).await
            }
            (Run::Ring(run), First::Ring(first)) => {
                ring_link::launch(run, first, &link, &self.rings).await
            }
            _ => {
                return Err(ApiError::internal(
                    "a run and its first message do not match",
                ));
            }
        }
        .map_err(internal)?;
        let line = run.line();
        (self.log)(line.clone());
        for problem in &report.problems {
            (self.log)(format!("problem: {name}: {problem}"));
        }
        let mut result = run.json();
        result["sent"] = json!(report.sent > 0);
        if let Some(approval) = &grant.approval {
            result["approval"] = json!(approval);
        }
        send_line(lines, api::status_line("started", json!({"run": name})));
        self.tell(&name, "run", result.clone());
        self.entries.push(Entry {
            name,
            run,
            link,
            grant: Some(grant),
            line,
            halted: None,
            next_look: 0,
        });
        Ok(result)
    }

    /// Stores a request that is over a limit, or that a page asked for, and makes the answer that
    /// names it. `session` is the page's, if a page asked.
    fn park(
        &mut self,
        request: &Request,
        ask: Ask,
        reasons: Vec<String>,
        now: u64,
        session: Option<String>,
    ) -> ApiError {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            return ApiError::internal("no randomness for a name");
        }
        let approval = bipnip_ln::hexutil::encode(&bytes);
        let parked = Parked {
            approval: approval.clone(),
            at: now,
            method: request.method.clone(),
            params: request.params.clone(),
            ask,
            over: reasons.clone(),
            session,
        };
        if let Err(e) = self.policy.park(&parked) {
            return internal(e);
        }
        (self.log)(format!(
            "parked {approval}: {} ({})",
            request.method,
            reasons.join("; ")
        ));
        let mut error = ApiError::new(
            Code::NeedsApproval,
            format!(
                "parked: {}. Parked as {approval}; `bipnip approve {approval}` lets it go ahead",
                reasons.join("; ")
            ),
        );
        error.approval = Some(approval);
        error
    }

    async fn cancel(
        &mut self,
        name: &str,
        lease: bool,
        caller: &Caller,
    ) -> Result<Value, ApiError> {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.name == name && entry.run.is_lease() == lease)
        else {
            return Err(ApiError::not_found("no such run under way"));
        };
        // A page cancels only what it asked for; the rest is the operator's.
        if let Caller::Page(key) = caller {
            let asked = entry
                .grant
                .as_ref()
                .and_then(|grant| grant.session.as_deref())
                == Some(key.as_str());
            if !asked {
                return Err(ApiError::refused(
                    "this page did not ask for this run; it is canceled where the agent runs",
                ));
            }
        }
        let request_id = unix_now();
        let mut sent = true;
        match &mut entry.run {
            Run::Seller(run) => {
                let out = run
                    .cancel(self.backend.as_ref(), request_id)
                    .await
                    .map_err(|e| ApiError::refused(e.to_string()))?;
                self.leases.save_seller(run).map_err(internal)?;
                for message in &out {
                    sent &= entry.link.send(message).await.is_ok();
                }
            }
            Run::Buyer(run) => {
                let out = run
                    .cancel(request_id)
                    .map_err(|e| ApiError::refused(e.to_string()))?;
                self.leases.save_buyer(run).map_err(internal)?;
                for message in &out {
                    sent &= entry.link.send(message).await.is_ok();
                }
            }
            Run::Ring(run) => {
                let out = run
                    .cancel(request_id)
                    .map_err(|e| ApiError::refused(e.to_string()))?;
                self.rings.save(run).map_err(internal)?;
                for message in &out {
                    sent &= entry.link.send_to_instance(message).await.is_ok();
                }
            }
        }
        let line = entry.run.line();
        entry.line = line.clone();
        let mut result = entry.run.json();
        // Canceled here either way. If no relay took the notice, the instance learns of it
        // when it next asks this run for something and gets no answer.
        result["sent"] = json!(sent);
        (self.log)(line);
        let name = name.to_string();
        self.tell(&name, "run", result.clone());
        Ok(result)
    }

    /// The stored runs of one kind, as they are on disk: under way and finished alike.
    fn list(&self, lease: bool, caller: &Caller) -> Result<Value, ApiError> {
        let (runs, problems): (Vec<Run>, Vec<(String, StoreError)>) = if lease {
            let loaded = self.leases.load().map_err(internal)?;
            (
                loaded
                    .sellers
                    .into_iter()
                    .map(Run::Seller)
                    .chain(loaded.buyers.into_iter().map(Run::Buyer))
                    .collect(),
                loaded.problems,
            )
        } else {
            let loaded = self.rings.load().map_err(internal)?;
            (
                loaded.runs.into_iter().map(Run::Ring).collect(),
                loaded.problems,
            )
        };
        let runs: Vec<Value> = runs
            .iter()
            .map(|run| {
                let mut value = run.json();
                let name = run.name();
                match self.entries.iter().find(|entry| entry.name == name) {
                    Some(entry) => {
                        value["under_way"] = json!(entry.halted.is_none() && !entry.run.watching());
                        if let Some(why) = &entry.halted {
                            value["halted"] = json!(why);
                        }
                    }
                    None => value["under_way"] = json!(false),
                }
                let grant = match self.entries.iter().find(|entry| entry.name == name) {
                    Some(entry) => entry.grant.clone(),
                    None => self.policy.grant(&name).ok().flatten(),
                };
                if let Some(grant) = grant {
                    if let Some(approval) = &grant.approval {
                        value["approval"] = json!(approval);
                    }
                    if let Caller::Page(key) = caller {
                        value["mine"] = json!(grant.session.as_deref() == Some(key.as_str()));
                    }
                }
                match caller {
                    Caller::Owner => value,
                    Caller::Page(_) => page_view(&value),
                }
            })
            .collect();
        // File names are the operator's to see, not a page's.
        let refused: Vec<Value> = match caller {
            Caller::Owner => problems
                .iter()
                .map(|(file, why)| json!({"file": file, "why": why.to_string()}))
                .collect(),
            Caller::Page(_) => Vec::new(),
        };
        Ok(json!({"runs": runs, "refused": refused}))
    }

    async fn publish_binding(&mut self) -> Result<Value, ApiError> {
        let keys = self.wallet.liquidity().map_err(internal)?;
        let event = make_binding_event(
            self.backend.as_ref(),
            &keys,
            BindingAction::Bind,
            self.config.network,
            unix_now(),
        )
        .await
        .map_err(|e| ApiError::new(Code::Node, e.to_string()))?;
        let report = relay::publish(&event, &self.config.relays)
            .await
            .map_err(|e| ApiError::new(Code::Relay, e))?;
        Ok(json!({
            "event": event.id.to_hex(),
            "accepted": report.accepted,
            "failed": report.failed.iter()
                .map(|(url, why)| json!({"relay": url, "why": why})).collect::<Vec<_>>(),
        }))
    }
}

/// A new run's first message.
enum First {
    Lease(Outgoing),
    Ring(Message),
}

/// A run made ready to start: nothing of it is published, stored or sent yet.
struct Prepared {
    run: Run,
    first: First,
    link: Link,
    /// An offer's node binding, published only once the request is allowed.
    binding: Option<Event>,
}

impl Prepared {
    fn new(run: Run, first: First, link: Link) -> Self {
        Prepared {
            run,
            first,
            link,
            binding: None,
        }
    }
}

fn refused(error: String) -> ApiError {
    ApiError::refused(error)
}

async fn prepare_offer(
    session: &Session<'_>,
    params: &LeaseOfferParams,
) -> Result<Prepared, ApiError> {
    let offer = OfferParams {
        min_capacity_sat: params.min_capacity_sat,
        max_capacity_sat: params.max_capacity_sat,
        duration_blocks: params.duration_blocks,
        fee_base_sat: params.fee_base_sat,
        fee_ppm: params.fee_ppm,
        upfront_percent: params.upfront_percent,
        min_confirmations: params.min_confirmations,
        announce: params.announce,
        expires_in_secs: params.expires_in_secs,
        bond_sat: params.bond_sat,
        rules: Rules {
            min_channels: params.min_buyer_channels,
            min_capacity_sat: params.min_buyer_capacity_sat,
        },
    };
    let seller = SellerConfig {
        cltv_delta: DEFAULT_SELLER_CLTV_DELTA,
        feerate_sat_vb: params.feerate_sat_vb,
    };
    let (run, first, link, binding) = lease_cmd::prepare_offer(session, offer, seller)
        .await
        .map_err(refused)?;
    Ok(Prepared {
        binding: Some(binding),
        ..Prepared::new(Run::Seller(run), First::Lease(first), link)
    })
}

async fn prepare_take(
    session: &Session<'_>,
    params: &LeaseTakeParams,
) -> Result<Prepared, ApiError> {
    let take = TakeParams {
        order_id: params.order_id,
        capacity_sat: params.capacity_sat,
        node_address: params.node_address.clone(),
    };
    let buyer = BuyerConfig {
        max_fee_msat: BUYER_MAX_ROUTING_FEE_MSAT,
        max_cltv_delta: DEFAULT_MAX_CLTV_DELTA,
    };
    let (run, first, link) = lease_cmd::prepare_take(session, take, buyer)
        .await
        .map_err(refused)?;
    Ok(Prepared::new(Run::Buyer(run), First::Lease(first), link))
}

async fn prepare_create(
    session: &Session<'_>,
    params: &RingCreateParams,
) -> Result<Prepared, ApiError> {
    let create = CreateParams {
        capacity_sat: params.capacity_sat,
        slots: params.slots,
        announce: params.announce,
        open_window_secs: params.open_window_secs,
        bond_sat: params.bond_sat,
        expires_in_secs: params.expires_in_secs,
        node_address: params.node_address.clone(),
        feerate_sat_vb: params.feerate_sat_vb,
        rules: Rules {
            min_channels: params.min_channels,
            min_capacity_sat: params.min_capacity_sat,
        },
    };
    let (run, first, link) = ring_cmd::prepare_create(session, create)
        .await
        .map_err(refused)?;
    Ok(Prepared::new(Run::Ring(run), First::Ring(first), link))
}

async fn prepare_join(
    session: &Session<'_>,
    params: &RingJoinParams,
) -> Result<Prepared, ApiError> {
    let join = JoinParams {
        order_id: params.order_id,
        node_address: params.node_address.clone(),
        feerate_sat_vb: params.feerate_sat_vb,
    };
    let (run, first, link) = ring_cmd::prepare_join(session, join)
        .await
        .map_err(refused)?;
    Ok(Prepared::new(Run::Ring(run), First::Ring(first), link))
}
