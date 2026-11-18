//! The lease commands: offer liquidity, take an order, and carry on after a restart. This is the
//! glue between the configuration, the wallet, the node, the relays and the lease runs; the
//! rules that protect money live in [`super::run`] and [`super::link`], not here.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bipnip_ln::LightningBackend;
use mostro_core::lease_message::LeaseOffer;
use mostro_core::node_binding::{Action, latest_for_node};
use mostro_core::rules::Rules;
use serde::Serialize;
use uuid::Uuid;

use super::link::{Link, StepReport, launch_buyer, launch_seller, step_buyer, step_seller};
use super::run::{
    BondTerms, BuyerConfig, BuyerRun, Outgoing, SellerConfig, SellerRun, TakeRequest,
};
use super::store::LeaseStore;
use crate::binding::make_binding_event;
use crate::config::{Config, InstanceConfig};
use crate::keys::Wallet;
use crate::{relay, secretfile};

/// Name of the file, in the leases directory, that holds the next unused lease trade index.
const NEXT_TRADE_INDEX_FILE: &str = "next-trade-index";
/// How far back a new run's link reads: nothing older can be for it.
pub(crate) const NEW_RUN_LOOKBACK_SECS: u64 = 60;
/// How far back a resumed run's link reads: longer than any hold invoice lives.
pub(crate) const RESUME_LOOKBACK_SECS: u64 = 3 * 24 * 3600;

/// What the commands work with.
pub struct Session<'a> {
    /// The agent's configuration.
    pub config: &'a Config,
    /// The wallet the keys come from.
    pub wallet: &'a Wallet,
    /// The agent's own node.
    pub backend: &'a dyn LightningBackend,
    /// The instance to work on, when the caller chose one (the daemon, which may know
    /// several). Without it, the only configured instance.
    pub instance: Option<&'a InstanceConfig>,
    /// A relay connection to share between runs (the daemon's). Without it, every run connects
    /// for itself.
    pub hub: Option<&'a nostr_sdk::prelude::Client>,
    /// True when the caller weighs a new run against the `[policy]` limits itself (the daemon,
    /// which parks what is over a limit). False for a command a person typed: there the one
    /// limit that applies, the bond, is checked here and a bond over it is refused.
    pub caller_checks_limits: bool,
}

impl Session<'_> {
    /// The instance this session works on, with its proof of work settled.
    pub(crate) async fn instance(&self) -> Result<InstanceConfig, String> {
        match self.instance {
            Some(instance) => Ok(instance.clone()),
            None => instance(self.config).await,
        }
    }
}

/// What a seller offers.
#[derive(Debug, Clone, Copy)]
pub struct OfferParams {
    /// Smallest channel, in satoshi.
    pub min_capacity_sat: u64,
    /// Largest channel, in satoshi.
    pub max_capacity_sat: u64,
    /// Lease length in blocks.
    pub duration_blocks: u32,
    /// Flat part of the fee, in satoshi.
    pub fee_base_sat: u64,
    /// Part of the fee proportional to the capacity, in parts per million.
    pub fee_ppm: u32,
    /// Share of the fee paid upfront, in percent.
    pub upfront_percent: u8,
    /// Confirmations the buyer waits for before releasing the preimage.
    pub min_confirmations: u32,
    /// Whether the channel is announced.
    pub announce: bool,
    /// How long the offer stays open, in seconds.
    pub expires_in_secs: u64,
    /// What the buyer locks at the instance while the lease is set up, in satoshi; 0 for none.
    pub bond_sat: u64,
    /// What the buyer's node must show in the public graph; none by default.
    pub rules: Rules,
}

/// What a buyer takes.
#[derive(Debug, Clone)]
pub struct TakeParams {
    /// The order, as the instance published it.
    pub order_id: Uuid,
    /// The capacity wanted, in satoshi, inside the order's range.
    pub capacity_sat: u64,
    /// Where the seller can reach our node (`host:port`), if it should connect to us.
    pub node_address: Option<String>,
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The instance a one-shot command works on: the only one configured, with its proof of work
/// settled. With several, the daemon is the way (`bipnip-agent run`).
async fn instance(config: &Config) -> Result<InstanceConfig, String> {
    match config.instances.as_slice() {
        [] => Err("no instance configured (add an [instance] section)".into()),
        [entry] => relay::resolve_instance(entry, config).await,
        _ => Err(
            "several instances are configured; this command works with one (use the daemon, \
             `bipnip-agent run`, to trade on several)"
                .into(),
        ),
    }
}

pub(crate) fn store(config: &Config) -> Result<LeaseStore, String> {
    let dir = config.leases_dir();
    LeaseStore::open(&dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Hands out the next unused lease trade index, and records that it is used before returning it:
/// an index is never given to two runs, even across a crash.
pub(crate) fn next_trade_index(config: &Config) -> Result<u32, String> {
    // The counter lives with the lease runs; rings share it, so the directory may not exist yet.
    secretfile::create_private_dir(&config.leases_dir()).map_err(|e| e.to_string())?;
    let path = config.leases_dir().join(NEXT_TRADE_INDEX_FILE);
    let index = match secretfile::read_secret(&path) {
        Ok(text) => text
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("{}: not a number", path.display()))?,
        Err(secretfile::SecretFileError::Io(_)) if !path.exists() => 1,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let next = index
        .checked_add(1)
        .ok_or("the lease trade indexes are used up")?;
    secretfile::replace_private(&path, &next.to_string())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(index)
}

pub(crate) async fn link_for(
    session: &Session<'_>,
    instance: &InstanceConfig,
    trade_index: u32,
    since: u64,
) -> Result<Link, String> {
    let identity = session.wallet.liquidity().map_err(|e| e.to_string())?;
    let trade = session
        .wallet
        .liquidity_trade(trade_index)
        .map_err(|e| e.to_string())?;
    if let Some(hub) = session.hub {
        return Ok(Link::shared(
            hub.clone(),
            identity,
            trade,
            instance.pubkey,
            instance.pow,
        ));
    }
    Link::connect(
        &session.config.relays,
        identity,
        trade,
        instance.pubkey,
        instance.pow,
        since,
    )
    .await
}

/// Says so when a first message reached no relay. The run is stored as owing it, and the loop
/// that follows sends it again.
fn owed_note(report: &StepReport, what: &str) {
    if report.sent == 0 {
        eprintln!(
            "note: {what} reached no relay yet and will be sent again ({})",
            report.problems.join("; ")
        );
    }
}

/// Our node's binding to the liquidity identity, made now.
pub(crate) async fn own_binding(
    session: &Session<'_>,
    now: u64,
) -> Result<nostr_sdk::prelude::Event, String> {
    let identity = session.wallet.liquidity().map_err(|e| e.to_string())?;
    make_binding_event(
        session.backend,
        &identity,
        Action::Bind,
        session.config.network,
        now,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Offers liquidity on the configured instance: publishes our node's binding, stores the new run
/// and sends the offer. Returns the run and its link, to drive with [`run_seller`].
pub async fn offer(
    session: &Session<'_>,
    params: OfferParams,
    seller: SellerConfig,
) -> Result<(SellerRun, Link), String> {
    let (mut run, first, link) = prepare_offer(session, params, seller).await?;
    let store = store(session.config)?;
    let report = launch_seller(&mut run, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    owed_note(&report, "the offer");
    Ok((run, link))
}

/// Whether our node can sell a lease at all: a seller's payment is held on its node until the
/// buyer releases the preimage, and not every kind of node can hold one (an LDK Server cannot
/// yet; see `bipnip_ln::ldk`). Asked before anything is offered, so that an offer nobody could
/// honour is never published.
pub(crate) async fn can_hold(backend: &dyn LightningBackend) -> Result<(), String> {
    // A hash no invoice was made for: a node that can hold answers that it knows none.
    match backend.hold_invoice_state(&"00".repeat(32)).await {
        Err(bipnip_ln::BackendError::Unsupported(why)) => Err(format!(
            "this node cannot sell a lease: {why}. It can still take one, or join a ring"
        )),
        _ => Ok(()),
    }
}

/// The seller's settings with the lock time brought down to what our node can give a hold
/// invoice, if it has a limit. Refused when that is too short for the confirmations the offer
/// asks buyers to wait for: the payment would go back while the channel is still confirming.
/// Asked before anything is offered, not when a buyer has already taken the offer.
pub(crate) fn fit_lock_time(
    backend: &dyn LightningBackend,
    seller: SellerConfig,
    min_confirmations: u32,
) -> Result<SellerConfig, String> {
    let cltv_delta = match backend.max_hold_cltv_delta() {
        Some(most) => seller.cltv_delta.min(most),
        None => seller.cltv_delta,
    };
    let needed = min_confirmations.saturating_add(super::SELLER_SETTLE_MARGIN_BLOCKS);
    if cltv_delta < needed {
        return Err(format!(
            "a hold invoice here would lock the buyer's payment for {cltv_delta} blocks; waiting \
             for {min_confirmations} confirmations needs {needed}"
        ));
    }
    Ok(SellerConfig {
        cltv_delta,
        ..seller
    })
}

/// Everything [`offer`] does short of storing the run and sending the offer: the run, its first
/// message and its link. Our node's binding is published here already; nothing is committed.
pub async fn prepare_offer(
    session: &Session<'_>,
    params: OfferParams,
    seller: SellerConfig,
) -> Result<(SellerRun, Outgoing, Link), String> {
    let config = session.config;
    can_hold(session.backend).await?;
    let seller = fit_lock_time(session.backend, seller, params.min_confirmations)?;
    let instance = &session.instance().await?;
    let now = unix_now();
    let info = session
        .backend
        .node_info()
        .await
        .map_err(|e| e.to_string())?;
    let binding = own_binding(session, now).await?;
    let offer = LeaseOffer {
        network: config.network,
        seller_node: info.node_pubkey,
        binding: binding.as_json(),
        min_capacity_sat: params.min_capacity_sat,
        max_capacity_sat: params.max_capacity_sat,
        duration_blocks: params.duration_blocks,
        fee_base_sat: params.fee_base_sat,
        fee_ppm: params.fee_ppm,
        upfront_percent: params.upfront_percent,
        min_confirmations: params.min_confirmations,
        announce: params.announce,
        expires_at: now.saturating_add(params.expires_in_secs),
        bond_sat: params.bond_sat,
        rules: params.rules,
    };
    offer.validate().map_err(|e| e.to_string())?;
    // Buyers look the binding up on the relays before they take the offer.
    relay::publish(&binding, &config.relays).await?;

    let trade_index = next_trade_index(config)?;
    let (run, first) = SellerRun::offer(instance.pubkey, offer, now, trade_index, seller);
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    Ok((run, first, link))
}

/// Takes an order on the configured instance. The order is read from the relays and must be
/// open, signed by the instance and on our network, and the seller's node must hold a current
/// binding there. The new run is stored, preimage included, before the take is sent. Returns the
/// run and its link, to drive with [`run_buyer`].
pub async fn take(
    session: &Session<'_>,
    params: TakeParams,
    buyer: BuyerConfig,
) -> Result<(BuyerRun, Link), String> {
    let (mut run, first, link) = prepare_take(session, params, buyer).await?;
    let store = store(session.config)?;
    let report = launch_buyer(&mut run, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    owed_note(&report, "the take");
    Ok((run, link))
}

/// Everything [`take`] does short of storing the run and sending the take: every check on the
/// order, the seller and the bond, then the run, its first message and its link. Nothing is
/// committed.
pub async fn prepare_take(
    session: &Session<'_>,
    params: TakeParams,
    buyer: BuyerConfig,
) -> Result<(BuyerRun, Outgoing, Link), String> {
    let config = session.config;
    let instance = &session.instance().await?;
    let now = unix_now();
    let order = relay::fetch_lease_order(params.order_id, &instance.pubkey, &config.relays)
        .await?
        .ok_or("the relays hold no such order from this instance")?;
    if !order.is_open(now) {
        return Err(format!(
            "the order is not open (status {})",
            order.status.as_str()
        ));
    }
    if order.network != config.network {
        return Err(format!(
            "the order is on {}, this agent on {}",
            order.network.as_str(),
            config.network.as_str()
        ));
    }
    let seller_node = order.seller_node.to_string();
    let statements = relay::fetch_statements(&seller_node, &config.relays).await?;
    match latest_for_node(&order.seller_node, statements.iter(), now) {
        Some(s) if s.statement.action == Action::Bind && s.statement.network == config.network => {}
        _ => return Err("the seller's node has no current binding on the relays".into()),
    }
    let terms = order
        .terms_for(params.capacity_sat)
        .map_err(|e| e.to_string())?;
    let binding = own_binding(session, now).await?;
    // A bond is paid only up to our own limit, and only to the node the instance's own binding
    // names: both are settled before the take is sent.
    let bond = crate::ring::cmd::instance_node_for(session, instance, order.bond_sat)
        .await?
        .map(|instance_node| BondTerms {
            instance_node,
            bond_sat: order.bond_sat,
        });

    let trade_index = next_trade_index(config)?;
    let request = TakeRequest {
        order_id: params.order_id,
        request_id: now,
        trade_index,
        seller_node,
        network: config.network,
        terms,
        binding: binding.as_json(),
        node_address: params.node_address,
        bond,
    };
    let (run, first) = BuyerRun::take(session.backend, instance.pubkey, request, buyer, now)
        .await
        .map_err(|e| e.to_string())?;
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    Ok((run, first, link))
}

fn name<T: Serialize>(state: &T) -> String {
    serde_json::to_string(state)
        .map(|quoted| quoted.trim_matches('"').to_string())
        .unwrap_or_default()
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_string())
}

/// One line about a seller's run: nothing secret in it.
pub fn describe_seller(run: &SellerRun) -> String {
    format!(
        "seller order={} state={} lease={} channel={} buyer-bond={}",
        or_dash(run.order_id().map(|id| id.to_string())),
        name(&run.state()),
        or_dash(run.contract().map(|contract| contract.id())),
        or_dash(run.funding_outpoint().map(str::to_string)),
        run.bond_status(),
    )
}

/// One line about a buyer's run: nothing secret in it.
pub fn describe_buyer(run: &BuyerRun) -> String {
    format!(
        "buyer order={} state={} lease={} bond={} waiting-on={}",
        run.order_id(),
        name(&run.state()),
        run.contract().id(),
        run.bond_status(),
        or_dash(run.waiting_on().map(str::to_string)),
    )
}

/// A command stops at rest: a lease that is active and was paid for in full upfront is left to
/// the daemon to watch.
fn seller_done(run: &SellerRun) -> bool {
    run.at_rest()
}

fn buyer_done(run: &BuyerRun) -> bool {
    run.at_rest()
}

fn tell(report: StepReport, say: &mut dyn FnMut(String)) {
    for problem in report.problems {
        say(format!("problem: {problem}"));
    }
}

/// Drives a seller's run until it is finished, looking every `poll`: until the lease is active
/// when all of the fee was upfront, otherwise until the completion share is paid at the end of
/// the lease. Each change of state and each problem is passed to `say`.
pub async fn run_seller(
    session: &Session<'_>,
    run: &mut SellerRun,
    link: &mut Link,
    poll: Duration,
    say: &mut dyn FnMut(String),
) -> Result<(), String> {
    let store = store(session.config)?;
    let mut last = None;
    loop {
        let report = step_seller(run, link, session.backend, &store, unix_now())
            .await
            .map_err(|e| e.to_string())?;
        tell(report, say);
        if last != Some(run.state()) {
            last = Some(run.state());
            say(describe_seller(run));
        }
        if seller_done(run) {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

/// Drives a buyer's run until it is finished, looking every `poll`.
pub async fn run_buyer(
    session: &Session<'_>,
    run: &mut BuyerRun,
    link: &mut Link,
    poll: Duration,
    say: &mut dyn FnMut(String),
) -> Result<(), String> {
    let store = store(session.config)?;
    let mut last = None;
    loop {
        let report = step_buyer(run, link, session.backend, &store, unix_now())
            .await
            .map_err(|e| e.to_string())?;
        tell(report, say);
        if last != Some(run.state()) {
            last = Some(run.state());
            say(describe_buyer(run));
        }
        if buyer_done(run) {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

/// One line per stored run, and one per file that was refused.
pub fn list(config: &Config) -> Result<Vec<String>, String> {
    let loaded = store(config)?.load().map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = loaded.sellers.iter().map(describe_seller).collect();
    lines.extend(loaded.buyers.iter().map(describe_buyer));
    lines.extend(
        loaded
            .problems
            .iter()
            .map(|(file, why)| format!("refused {file}: {why}")),
    );
    Ok(lines)
}

/// Carries on with every stored run that is not finished, until all of them are.
pub async fn resume(
    session: &Session<'_>,
    poll: Duration,
    say: &mut dyn FnMut(String),
) -> Result<(), String> {
    let config = session.config;
    let instance = &session.instance().await?;
    let store = store(config)?;
    let loaded = store.load().map_err(|e| e.to_string())?;
    for (file, why) in &loaded.problems {
        say(format!("refused {file}: {why}"));
    }
    let since = unix_now().saturating_sub(RESUME_LOOKBACK_SECS);
    let mut sellers = Vec::new();
    for run in loaded.sellers {
        if !seller_done(&run) && run.instance() == instance.pubkey {
            let link = link_for(session, instance, run.trade_index(), since).await?;
            say(describe_seller(&run));
            sellers.push((run, link));
        }
    }
    let mut buyers = Vec::new();
    for mut run in loaded.buyers {
        if !buyer_done(&run) && run.instance() == instance.pubkey {
            run.resumed();
            let link = link_for(session, instance, run.trade_index(), since).await?;
            say(describe_buyer(&run));
            buyers.push((run, link));
        }
    }
    loop {
        let mut busy = false;
        for (run, link) in &mut sellers {
            if seller_done(run) {
                continue;
            }
            busy = true;
            let before = run.state();
            let report = step_seller(run, link, session.backend, &store, unix_now())
                .await
                .map_err(|e| e.to_string())?;
            tell(report, say);
            if run.state() != before {
                say(describe_seller(run));
            }
        }
        for (run, link) in &mut buyers {
            if buyer_done(run) {
                continue;
            }
            busy = true;
            let before = run.state();
            let report = step_buyer(run, link, session.backend, &store, unix_now())
                .await
                .map_err(|e| e.to_string())?;
            tell(report, say);
            if run.state() != before {
                say(describe_buyer(run));
            }
        }
        if !busy {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::run::{BuyerState, SellerState};

    #[test]
    fn trade_indexes_are_handed_out_once_each() {
        let dir = std::env::temp_dir().join(format!("bipnip-cmd-{}-index", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config::parse(&format!(
            "network = \"regtest\"\ndata_dir = \"{}\"\n",
            dir.display()
        ))
        .expect("config");
        store(&config).expect("store");
        assert_eq!(next_trade_index(&config), Ok(1));
        assert_eq!(next_trade_index(&config), Ok(2));
        assert_eq!(next_trade_index(&config), Ok(3));
        // What is on disk is the next one, so a crash after handing one out cannot repeat it.
        let file = config.leases_dir().join(NEXT_TRADE_INDEX_FILE);
        assert_eq!(secretfile::read_secret(&file).expect("read"), "4");
        // A file that is not a number is refused, not restarted from 1.
        secretfile::replace_private(&file, "many").expect("write");
        assert!(next_trade_index(&config).is_err());
    }

    #[tokio::test]
    async fn a_node_that_cannot_hold_a_payment_offers_no_lease() {
        use std::sync::{Arc, Mutex};

        use crate::lease::tests::{NoHold, Node, World, secret};

        let node = Node {
            world: Arc::new(Mutex::new(World::default())),
            secret: secret(1),
            network: "regtest",
        };
        assert_eq!(can_hold(&node).await, Ok(()));
        let refused = can_hold(&NoHold(node)).await.expect_err("cannot hold");
        assert!(refused.contains("cannot sell a lease"), "{refused}");
    }

    #[test]
    fn the_lock_time_is_what_the_node_can_give_or_the_offer_is_refused() {
        use std::sync::{Arc, Mutex};

        use async_trait::async_trait;
        use bipnip_ln::{
            BackendError, ChannelView, GraphChannel, HoldInvoiceRequest, HoldState, NodeInfo,
            OpenRequest, PaymentState,
        };

        use crate::lease::tests::{Node, World, secret};
        use crate::lease::{DEFAULT_SELLER_CLTV_DELTA, SELLER_SETTLE_MARGIN_BLOCKS};

        /// A node whose hold invoices lock for at most so many blocks.
        struct Capped(Node, u32);

        #[async_trait]
        impl LightningBackend for Capped {
            fn max_hold_cltv_delta(&self) -> Option<u32> {
                Some(self.1)
            }
            async fn node_info(&self) -> Result<NodeInfo, BackendError> {
                self.0.node_info().await
            }
            async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
                self.0.sign_message(msg).await
            }
            async fn add_hold_invoice(
                &self,
                r: &HoldInvoiceRequest,
            ) -> Result<String, BackendError> {
                self.0.add_hold_invoice(r).await
            }
            async fn hold_invoice_state(&self, h: &str) -> Result<HoldState, BackendError> {
                self.0.hold_invoice_state(h).await
            }
            async fn settle_hold_invoice(&self, p: &str) -> Result<(), BackendError> {
                self.0.settle_hold_invoice(p).await
            }
            async fn cancel_hold_invoice(&self, h: &str) -> Result<(), BackendError> {
                self.0.cancel_hold_invoice(h).await
            }
            async fn pay_invoice(
                &self,
                b: &str,
                f: u64,
                w: u32,
            ) -> Result<PaymentState, BackendError> {
                self.0.pay_invoice(b, f, w).await
            }
            async fn payment_state(&self, h: &str) -> Result<PaymentState, BackendError> {
                self.0.payment_state(h).await
            }
            async fn payment_lookup(&self, h: &str) -> Result<Option<PaymentState>, BackendError> {
                self.0.payment_lookup(h).await
            }
            async fn add_invoice(&self, a: u64, e: u32, d: &str) -> Result<String, BackendError> {
                self.0.add_invoice(a, e, d).await
            }
            async fn invoice_state(
                &self,
                b: &str,
            ) -> Result<bipnip_ln::InvoiceStatus, BackendError> {
                self.0.invoice_state(b).await
            }
            async fn closed_channel(
                &self,
                r: &str,
                o: &str,
            ) -> Result<Option<bipnip_ln::ClosedChannel>, BackendError> {
                self.0.closed_channel(r, o).await
            }
            async fn connect(&self, p: &str, a: &str) -> Result<(), BackendError> {
                self.0.connect(p, a).await
            }
            async fn open_channel(&self, r: &OpenRequest) -> Result<String, BackendError> {
                self.0.open_channel(r).await
            }
            async fn channels_with(&self, r: &str) -> Result<Vec<ChannelView>, BackendError> {
                self.0.channels_with(r).await
            }
            async fn graph_node(&self, n: &str) -> Result<Option<(u32, u64)>, BackendError> {
                self.0.graph_node(n).await
            }
            async fn graph_channels(&self, n: &str) -> Result<Vec<GraphChannel>, BackendError> {
                self.0.graph_channels(n).await
            }
        }

        let node = || Node {
            world: Arc::new(Mutex::new(World::default())),
            secret: secret(1),
            network: "regtest",
        };
        let asked = SellerConfig {
            cltv_delta: DEFAULT_SELLER_CLTV_DELTA,
            feerate_sat_vb: 2,
        };
        // A node without a limit gives what was asked.
        let fitted = fit_lock_time(&node(), asked, 3).expect("fits");
        assert_eq!(fitted.cltv_delta, DEFAULT_SELLER_CLTV_DELTA);
        // A node that gives 80 blocks at most: the offer asks for 80, never more.
        let fitted = fit_lock_time(&Capped(node(), 80), asked, 3).expect("fits");
        assert_eq!((fitted.cltv_delta, fitted.feerate_sat_vb), (80, 2));
        // A limit above what was asked changes nothing.
        let fitted = fit_lock_time(&Capped(node(), 500), asked, 3).expect("fits");
        assert_eq!(fitted.cltv_delta, DEFAULT_SELLER_CLTV_DELTA);
        // 80 blocks hold for 44 confirmations and the margin, and not for 45.
        let most = 80 - SELLER_SETTLE_MARGIN_BLOCKS;
        assert!(fit_lock_time(&Capped(node(), 80), asked, most).is_ok());
        let refused = fit_lock_time(&Capped(node(), 80), asked, most + 1).expect_err("too short");
        assert!(refused.contains("80 blocks"), "{refused}");
    }

    #[test]
    fn states_are_named_as_they_are_stored() {
        assert_eq!(name(&SellerState::Opening), "opening");
        assert_eq!(name(&BuyerState::Verifying), "verifying");
    }
}
