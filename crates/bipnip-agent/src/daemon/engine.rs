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
use mostro_core::node_binding::Action as BindingAction;
use mostro_core::prelude::Message;
use nostr_sdk::prelude::{Client, Event, Filter, Kind, PublicKey, Timestamp, ToBech32};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::api::{
    self, ApiError, Call, Code, LeaseOfferParams, LeaseTakeParams, OrderKind, OrdersParams,
    Request, RingCreateParams, RingJoinParams,
};
use super::policy::{Ask, Grant, Guard, Parked, PolicyStore, Usage, over};
use crate::binding::make_binding_event;
use crate::config::{Config, InstanceConfig, InstanceEntry};
use crate::keys::Wallet;
use crate::lease::cmd::{
    self as lease_cmd, OfferParams, RESUME_LOOKBACK_SECS, Session, TakeParams, unix_now,
};
use crate::lease::link::{Link, StepReport, launch_buyer, launch_seller, step_buyer, step_seller};
use crate::lease::run::{BuyerConfig, BuyerRun, Outgoing, SellerConfig, SellerRun};
use crate::lease::store::{LeaseStore, StoreError};
use crate::lease::{DEFAULT_MAX_CLTV_DELTA, DEFAULT_SELLER_CLTV_DELTA};
use crate::relay::{self, OpenOrder, RELAY_TIMEOUT};
use crate::ring::cmd::{self as ring_cmd, CreateParams, JoinParams};
use crate::ring::link as ring_link;
use crate::ring::run::RingRun;
use crate::ring::store::RingStore;

/// The most a buyer's node may pay in routing fees for one lease payment, in millisatoshi: the
/// default of `lease take --max-fee-msat`.
const BUYER_MAX_ROUTING_FEE_MSAT: u64 = 10_000;
/// How many ticks pass between tries to reach an instance whose proof of work is not known yet.
const RESOLVE_EVERY_TICKS: u32 = 12;

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
                "line": self.line(),
            })
        };
        let mut value = match self {
            Run::Seller(run) => {
                let mut value = common("seller", run.order_id().map(|id| id.to_string()));
                value["lease"] = json!(run.contract().map(|contract| contract.id()));
                value["channel"] = json!(run.funding_outpoint());
                value["bond"] = json!(run.bond_status());
                value
            }
            Run::Buyer(run) => {
                let mut value = common("buyer", Some(run.order_id().to_string()));
                value["lease"] = json!(run.contract().id());
                value["bond"] = json!(run.bond_status());
                value["waiting_on"] = json!(run.waiting_on());
                value
            }
            Run::Ring(run) => {
                let mut value = common("member", run.order_id().map(|id| id.to_string()));
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
            // A seller opens a channel of at most the largest capacity offered, and pays nothing.
            Run::Seller(run) => Ask {
                capacity_sat: run.offered().max_capacity_sat,
                ..Ask::default()
            },
            // A buyer pays the lease's whole fee and, where the order asks, locks a bond. A fee
            // that cannot be computed is taken as the largest there is, so it passes no limit.
            Run::Buyer(run) => Ask {
                fee_sat: run.contract().terms.total_fee_sat().unwrap_or(u64::MAX),
                capacity_sat: 0,
                bond_sat: run.bond_sat(),
                peer: Some(run.contract().seller_node.to_string()),
            },
            // A ring member opens one channel and, where the ring asks, locks a bond.
            Run::Ring(run) => Ask {
                capacity_sat: run.terms().capacity_sat,
                bond_sat: run.terms().bond_sat,
                ..Ask::default()
            },
        }
    }
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
}

struct Watcher {
    run: Option<String>,
    lines: Lines,
}

/// The daemon's state. See the module's text.
pub struct Engine {
    config: Config,
    wallet: Wallet,
    backend: Box<dyn LightningBackend>,
    client: Client,
    instances: Vec<InstanceConfig>,
    unresolved: Vec<InstanceEntry>,
    leases: LeaseStore,
    rings: RingStore,
    policy: PolicyStore,
    entries: Vec<Entry>,
    watchers: Vec<Watcher>,
    ticks: u32,
    log: Box<dyn FnMut(String)>,
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::internal(error.to_string())
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
        Run::Buyer(run) => step_buyer(run, link, backend, leases).await,
        Run::Ring(run) => ring_link::step(run, link, backend, rings, now).await,
    }
}

fn order_json(instance: &PublicKey, order: &OpenOrder) -> Value {
    match order {
        OpenOrder::Lease(order) => json!({
            "kind": "lease",
            "instance": instance.to_hex(),
            "order_id": order.id.to_string(),
            "seller_node": order.seller_node.to_string(),
            "min_capacity_sat": order.min_capacity_sat,
            "max_capacity_sat": order.max_capacity_sat,
            "duration_blocks": order.duration_blocks,
            "fee_base_sat": order.fee_base_sat,
            "fee_ppm": order.fee_ppm,
            "upfront_percent": order.upfront_percent,
            "min_confirmations": order.min_confirmations,
            "announce": order.announce,
            "bond_sat": order.bond_sat,
            "expires_at": order.expires_at,
        }),
        OpenOrder::Ring(order) => json!({
            "kind": "ring",
            "instance": instance.to_hex(),
            "order_id": order.id.to_string(),
            "capacity_sat": order.capacity_sat,
            "slots": order.slots,
            "taken": order.taken,
            "announce": order.announce,
            "open_window_secs": order.open_window_secs,
            "bond_sat": order.bond_sat,
            "expires_at": order.expires_at,
        }),
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
        let client = relay::connected(&config.relays).await?;
        let mut engine = Engine {
            unresolved: config.instances.clone(),
            config,
            wallet,
            backend,
            client,
            instances: Vec::new(),
            leases,
            rings,
            policy,
            entries: Vec::new(),
            watchers: Vec::new(),
            ticks: 0,
            log,
        };
        engine.resolve_instances().await;
        engine.resume()?;
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
        for run in runs {
            let name = run.name();
            if run.finished() || self.entries.iter().any(|entry| entry.name == name) {
                continue;
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
            });
        }
        Ok(())
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
        ))
    }

    fn tell(&mut self, run: &str, status: &str, fields: Value) {
        let line = api::status_line(status, fields);
        self.watchers.retain(|watcher| {
            if watcher.run.as_deref().is_some_and(|wanted| wanted != run) {
                return !watcher.lines.is_closed();
            }
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
    /// final line, except for `watch`, which has none.
    pub async fn handle(&mut self, request: Request, lines: Lines) {
        if let Call::Watch(run) = &request.call {
            self.watch(run.clone(), lines);
            return;
        }
        let line = match self.call(&request, &lines).await {
            Ok(result) => api::ok_line(result),
            Err(error) => api::error_line(&error),
        };
        send_line(&lines, line);
    }

    fn watch(&mut self, run: Option<String>, lines: Lines) {
        // What is under way now, then every change.
        for entry in &self.entries {
            if run.as_deref().is_none_or(|wanted| wanted == entry.name) {
                send_line(&lines, api::status_line("run", entry.run.json()));
            }
        }
        send_line(&lines, api::status_line("watching", Value::Null));
        self.watchers.push(Watcher { run, lines });
    }

    async fn call(&mut self, request: &Request, lines: &Lines) -> Result<Value, ApiError> {
        match &request.call {
            Call::Status => self.status().await,
            Call::Orders(params) => self.orders(params).await,
            Call::LeaseOffer(_) | Call::LeaseTake(_) | Call::RingCreate(_) | Call::RingJoin(_) => {
                self.start_run(request, None, lines).await
            }
            Call::LeaseCancel(run) => self.cancel(run, true).await,
            Call::RingCancel(run) => self.cancel(run, false).await,
            Call::LeaseList => self.list(true),
            Call::RingList => self.list(false),
            Call::BindingPublish => self.publish_binding().await,
            Call::Approvals => {
                let parked = self.policy.all_parked().map_err(internal)?;
                Ok(json!({"approvals": parked}))
            }
            Call::Approve(approval) => {
                let parked = self
                    .policy
                    .parked(approval)
                    .map_err(internal)?
                    .ok_or_else(|| ApiError::not_found("no such approval"))?;
                // The words are read again exactly as they were parked.
                let call = api::understand(&parked.method, &parked.params)?;
                let again = Request {
                    method: parked.method.clone(),
                    params: parked.params.clone(),
                    call,
                };
                let result = self.start_run(&again, Some(&parked), lines).await?;
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
            // Answered in `handle`.
            Call::Watch(_) => Err(ApiError::internal("watch has no answer")),
        }
    }

    fn under_way(&self) -> u32 {
        let count = self
            .entries
            .iter()
            .filter(|entry| !entry.run.finished())
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn usage(&self, now: u64) -> Result<Usage, ApiError> {
        let (fee_day_sat, capacity_day_sat) = self.policy.used_since(now).map_err(internal)?;
        Ok(Usage {
            fee_day_sat,
            capacity_day_sat,
            runs: self.under_way(),
        })
    }

    async fn status(&mut self) -> Result<Value, ApiError> {
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
        Ok(json!({
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
        }))
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

    async fn orders(&mut self, params: &OrdersParams) -> Result<Value, ApiError> {
        let instances: Vec<InstanceConfig> = match params.instance.as_deref() {
            Some(named) => vec![self.instance(Some(named))?],
            None => self.instances.clone(),
        };
        let now = unix_now();
        let mut orders = Vec::new();
        for instance in &instances {
            let open = relay::fetch_open_orders(&instance.pubkey, &self.config.relays, now)
                .await
                .map_err(|e| ApiError::new(Code::Relay, e))?;
            for order in &open {
                let wanted = match params.kind {
                    None => true,
                    Some(OrderKind::Lease) => matches!(order, OpenOrder::Lease(_)),
                    Some(OrderKind::Ring) => matches!(order, OpenOrder::Ring(_)),
                };
                // Orders on another network are not ours to take.
                let network = match order {
                    OpenOrder::Lease(order) => order.network,
                    OpenOrder::Ring(order) => order.network,
                };
                if wanted && network == self.config.network {
                    orders.push(order_json(&instance.pubkey, order));
                }
            }
        }
        Ok(json!({"orders": orders}))
    }

    /// Starts the run a request asks for: prepares it with every check the one-shot command
    /// makes, weighs it against the policy, and only then stores it and sends its first
    /// message. `approved` is the parked request this is the approval of, if it is one.
    async fn start_run(
        &mut self,
        request: &Request,
        approved: Option<&Parked>,
        lines: &Lines,
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
        // Nothing is stored or sent by the preparation.
        let (mut run, first, link) = match &request.call {
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
                let reasons = over(&self.config.policy, &ask, &self.usage(now)?);
                if !reasons.is_empty() {
                    return Err(self.park(request, ask, reasons, now));
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
        // The grant is on disk before the run exists, so a run of the daemon's is never
        // without one.
        let grant = Grant::new(name.clone(), now, ask, approved.is_some());
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
        send_line(lines, api::status_line("started", json!({"run": name})));
        self.tell(&name, "run", result.clone());
        self.entries.push(Entry {
            name,
            run,
            link,
            grant: Some(grant),
            line,
            halted: None,
        });
        Ok(result)
    }

    /// Stores a request that is over a limit and makes the answer that names it.
    fn park(&mut self, request: &Request, ask: Ask, reasons: Vec<String>, now: u64) -> ApiError {
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
                "over the policy: {}. Parked as {approval}; `bipnip approve {approval}` lets \
                 it go ahead",
                reasons.join("; ")
            ),
        );
        error.approval = Some(approval);
        error
    }

    async fn cancel(&mut self, name: &str, lease: bool) -> Result<Value, ApiError> {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.name == name && entry.run.is_lease() == lease)
        else {
            return Err(ApiError::not_found("no such run under way"));
        };
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
    fn list(&self, lease: bool) -> Result<Value, ApiError> {
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
                        value["under_way"] = json!(entry.halted.is_none());
                        if let Some(why) = &entry.halted {
                            value["halted"] = json!(why);
                        }
                    }
                    None => value["under_way"] = json!(false),
                }
                value
            })
            .collect();
        let refused: Vec<Value> = problems
            .iter()
            .map(|(file, why)| json!({"file": file, "why": why.to_string()}))
            .collect();
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

fn refused(error: String) -> ApiError {
    ApiError::refused(error)
}

async fn prepare_offer(
    session: &Session<'_>,
    params: &LeaseOfferParams,
) -> Result<(Run, First, Link), ApiError> {
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
    };
    let seller = SellerConfig {
        cltv_delta: DEFAULT_SELLER_CLTV_DELTA,
        feerate_sat_vb: params.feerate_sat_vb,
    };
    let (run, first, link) = lease_cmd::prepare_offer(session, offer, seller)
        .await
        .map_err(refused)?;
    Ok((Run::Seller(run), First::Lease(first), link))
}

async fn prepare_take(
    session: &Session<'_>,
    params: &LeaseTakeParams,
) -> Result<(Run, First, Link), ApiError> {
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
    Ok((Run::Buyer(run), First::Lease(first), link))
}

async fn prepare_create(
    session: &Session<'_>,
    params: &RingCreateParams,
) -> Result<(Run, First, Link), ApiError> {
    let create = CreateParams {
        capacity_sat: params.capacity_sat,
        slots: params.slots,
        announce: params.announce,
        open_window_secs: params.open_window_secs,
        bond_sat: params.bond_sat,
        expires_in_secs: params.expires_in_secs,
        node_address: params.node_address.clone(),
        feerate_sat_vb: params.feerate_sat_vb,
    };
    let (run, first, link) = ring_cmd::prepare_create(session, create)
        .await
        .map_err(refused)?;
    Ok((Run::Ring(run), First::Ring(first), link))
}

async fn prepare_join(
    session: &Session<'_>,
    params: &RingJoinParams,
) -> Result<(Run, First, Link), ApiError> {
    let join = JoinParams {
        order_id: params.order_id,
        node_address: params.node_address.clone(),
        feerate_sat_vb: params.feerate_sat_vb,
    };
    let (run, first, link) = ring_cmd::prepare_join(session, join)
        .await
        .map_err(refused)?;
    Ok((Run::Ring(run), First::Ring(first), link))
}
