//! The lease commands: offer liquidity, take an order, and carry on after a restart. This is the
//! glue between the configuration, the wallet, the node, the relays and the lease runs; the
//! rules that protect money live in [`super::run`] and [`super::link`], not here.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bipnip_ln::LightningBackend;
use mostro_core::lease_message::LeaseOffer;
use mostro_core::node_binding::{Action, latest_for_node};
use serde::Serialize;
use uuid::Uuid;

use super::link::{Link, StepReport, launch_buyer, launch_seller, step_buyer, step_seller};
use super::run::{BondTerms, BuyerConfig, BuyerRun, SellerConfig, SellerRun, TakeRequest};
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

pub(crate) fn instance(config: &Config) -> Result<&InstanceConfig, String> {
    config
        .instance
        .as_ref()
        .ok_or_else(|| "no instance configured (add an [instance] section)".to_string())
}

fn store(config: &Config) -> Result<LeaseStore, String> {
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
    let config = session.config;
    let instance = instance(config)?;
    let store = store(config)?;
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
    };
    offer.validate().map_err(|e| e.to_string())?;
    // Buyers look the binding up on the relays before they take the offer.
    relay::publish(&binding, &config.relays).await?;

    let trade_index = next_trade_index(config)?;
    let (mut run, first) = SellerRun::offer(instance.pubkey, offer, now, trade_index, seller);
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    let report = launch_seller(&mut run, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    owed_note(&report, "the offer");
    Ok((run, link))
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
    let config = session.config;
    let instance = instance(config)?;
    let store = store(config)?;
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
    let bond = crate::ring::cmd::instance_node_for(config, instance, order.bond_sat)
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
    let (mut run, first) = BuyerRun::take(session.backend, instance.pubkey, request, buyer, now)
        .await
        .map_err(|e| e.to_string())?;
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    let report = launch_buyer(&mut run, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    owed_note(&report, "the take");
    Ok((run, link))
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

fn seller_done(run: &SellerRun) -> bool {
    run.finished()
}

fn buyer_done(run: &BuyerRun) -> bool {
    run.finished()
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
        let report = step_buyer(run, link, session.backend, &store)
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
    let instance = instance(config)?;
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
    for run in loaded.buyers {
        if !buyer_done(&run) && run.instance() == instance.pubkey {
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
            let report = step_buyer(run, link, session.backend, &store)
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

    #[test]
    fn states_are_named_as_they_are_stored() {
        assert_eq!(name(&SellerState::Opening), "opening");
        assert_eq!(name(&BuyerState::Verifying), "verifying");
    }
}
