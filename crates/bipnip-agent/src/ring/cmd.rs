//! The ring commands: create a ring, join one, and carry on after a restart. Glue between the
//! configuration, the wallet, the node, the relays and the ring run; the checks that matter are
//! in [`super::run`].

use std::time::Duration;

use mostro_core::prelude::Message;
use mostro_core::ring_message::{RingOffer, RingSeat};
use mostro_core::rules::Rules;
use uuid::Uuid;

use super::link::{launch, step};
use super::run::{JoinRequest, RingRun};
use super::store::RingStore;
use crate::config::Config;
use crate::lease::cmd::{
    NEW_RUN_LOOKBACK_SECS, RESUME_LOOKBACK_SECS, Session, link_for, next_trade_index, own_binding,
    unix_now,
};
use crate::lease::link::{Link, StepReport};
use crate::relay;

/// A ring to create.
#[derive(Debug, Clone)]
pub struct CreateParams {
    /// Capacity of every channel, in satoshi.
    pub capacity_sat: u64,
    /// How many members the ring has, ourselves included.
    pub slots: u8,
    /// Whether the channels are announced.
    pub announce: bool,
    /// Seconds members have to open once the ring is full.
    pub open_window_secs: u64,
    /// What every member locks at the instance until the ring ends, in satoshi; 0 for none.
    pub bond_sat: u64,
    /// How long the slots stay open, in seconds.
    pub expires_in_secs: u64,
    /// Where the member that opens to us can reach our node (`host:port`).
    pub node_address: Option<String>,
    /// Fee rate of our funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
    /// What every member's node must show in the public graph; none by default.
    pub rules: Rules,
}

/// A ring to join.
#[derive(Debug, Clone)]
pub struct JoinParams {
    /// The ring's order, as the instance published it.
    pub order_id: Uuid,
    /// Where the member that opens to us can reach our node (`host:port`).
    pub node_address: Option<String>,
    /// Fee rate of our funding transaction, in sat/vB.
    pub feerate_sat_vb: u64,
}

pub(crate) fn store(config: &Config) -> Result<RingStore, String> {
    let dir = config.rings_dir();
    RingStore::open(&dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Our seat: our node, its binding to the liquidity identity, and where to reach it.
async fn own_seat(
    session: &Session<'_>,
    node_address: Option<String>,
    now: u64,
) -> Result<RingSeat, String> {
    let info = session
        .backend
        .node_info()
        .await
        .map_err(|e| e.to_string())?;
    let seat = RingSeat {
        node: info.node_pubkey,
        binding: own_binding(session, now).await?.as_json(),
        node_address,
    };
    seat.validate().map_err(|e| e.to_string())?;
    Ok(seat)
}

/// Says so when a first message reached no relay. The run is stored as owing it, and the loop
/// that follows sends it again.
fn note_unsent(report: &StepReport, what: &str) {
    if report.sent == 0 {
        eprintln!(
            "note: {what} reached no relay yet and will be sent again ({})",
            report.problems.join("; ")
        );
    }
}

/// For a ring or a lease with a bond: checks the bond against our own limit and finds the node
/// the bond would be paid to, from the instance's own verified binding. Without a bond, nothing.
/// The daemon weighs the bond with its other limits itself, and may have had it approved, so
/// for it only the node is looked up.
pub(crate) async fn instance_node_for(
    session: &Session<'_>,
    instance: &crate::config::InstanceConfig,
    bond_sat: u64,
) -> Result<Option<String>, String> {
    if bond_sat == 0 {
        return Ok(None);
    }
    let config = session.config;
    let limit = config.policy.max_bond_sat;
    if !session.caller_checks_limits && bond_sat > limit {
        return Err(format!(
            "this asks for a bond of {bond_sat} sat; this agent locks at most {limit} \
             (policy.max_bond_sat in the config)"
        ));
    }
    let node = relay::fetch_instance_node(&instance.pubkey, config.network, &config.relays)
        .await?
        .ok_or("the instance has published no node binding, so a bond cannot be checked")?;
    Ok(Some(node))
}

/// Proposes a ring on the configured instance and takes its first slot. Returns the run and
/// its link, to drive with [`run`].
pub async fn create(
    session: &Session<'_>,
    params: CreateParams,
) -> Result<(RingRun, Link), String> {
    let (mut ring, first, link) = prepare_create(session, params).await?;
    let store = store(session.config)?;
    let report = launch(&mut ring, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    note_unsent(&report, "the ring");
    Ok((ring, link))
}

/// Everything [`create`] does short of storing the run and sending the offer.
pub async fn prepare_create(
    session: &Session<'_>,
    params: CreateParams,
) -> Result<(RingRun, Message, Link), String> {
    let config = session.config;
    let instance = &session.instance().await?;
    let now = unix_now();
    let offer = RingOffer {
        network: config.network,
        capacity_sat: params.capacity_sat,
        slots: params.slots,
        announce: params.announce,
        open_window_secs: params.open_window_secs,
        bond_sat: params.bond_sat,
        expires_at: now.saturating_add(params.expires_in_secs),
        seat: own_seat(session, params.node_address, now).await?,
        rules: params.rules,
    };
    offer.validate().map_err(|e| e.to_string())?;
    let instance_node = instance_node_for(session, instance, params.bond_sat).await?;
    let trade_index = next_trade_index(config)?;
    let (ring, first) = RingRun::create(
        instance.pubkey,
        offer,
        now,
        trade_index,
        params.feerate_sat_vb,
        instance_node,
    );
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    Ok((ring, first, link))
}

/// Takes a slot of a ring on the configured instance. The order is read from the relays and
/// must be open, signed by the instance and on our network; the terms and size it shows are
/// what the roster is later held to. Returns the run and its link, to drive with [`run`].
pub async fn join(session: &Session<'_>, params: JoinParams) -> Result<(RingRun, Link), String> {
    let (mut ring, first, link) = prepare_join(session, params).await?;
    let store = store(session.config)?;
    let report = launch(&mut ring, &first, &link, &store)
        .await
        .map_err(|e| e.to_string())?;
    note_unsent(&report, "the take");
    Ok((ring, link))
}

/// Everything [`join`] does short of storing the run and sending the take: the checks on the
/// order and the bond, then the run, its first message and its link.
pub async fn prepare_join(
    session: &Session<'_>,
    params: JoinParams,
) -> Result<(RingRun, Message, Link), String> {
    let config = session.config;
    let instance = &session.instance().await?;
    let now = unix_now();
    let order = relay::fetch_ring_order(params.order_id, &instance.pubkey, &config.relays)
        .await?
        .ok_or("the relays hold no such ring from this instance")?;
    if !order.is_open(now) {
        return Err(format!(
            "the ring has no open slot (status {}, {} of {} taken)",
            order.status.as_str(),
            order.taken,
            order.slots
        ));
    }
    if order.network != config.network {
        return Err(format!(
            "the ring is on {}, this agent on {}",
            order.network.as_str(),
            config.network.as_str()
        ));
    }
    let instance_node = instance_node_for(session, instance, order.bond_sat).await?;
    let trade_index = next_trade_index(config)?;
    let request = JoinRequest {
        order_id: params.order_id,
        request_id: now,
        trade_index,
        terms: order.terms(),
        slots: order.slots,
        seat: own_seat(session, params.node_address, now).await?,
        feerate_sat_vb: params.feerate_sat_vb,
        instance_node,
    };
    let (ring, first) = RingRun::join(instance.pubkey, request);
    let link = link_for(
        session,
        instance,
        trade_index,
        now.saturating_sub(NEW_RUN_LOOKBACK_SECS),
    )
    .await?;
    Ok((ring, first, link))
}

fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_string())
}

/// One line about a ring run: nothing secret in it.
pub fn describe(ring: &RingRun) -> String {
    let state = serde_json::to_string(&ring.state())
        .map(|quoted| quoted.trim_matches('"').to_string())
        .unwrap_or_default();
    format!(
        "ring order={} state={} ring={} opened={} ours-confirmed={} incoming-confirmed={} \
         bond={} owed-bond={} waiting-on={}",
        or_dash(ring.order_id().map(|id| id.to_string())),
        state,
        or_dash(ring.ring().map(|ring| ring.id())),
        or_dash(ring.opened_outpoint().map(str::to_string)),
        ring.our_channel_confirmed(),
        ring.incoming_confirmed(),
        ring.bond_status(),
        ring.owed_status(),
        or_dash(ring.waiting_on().map(str::to_string)),
    )
}

/// Drives a ring run until the ring is over, looking every `poll`. Each change and each problem
/// is passed to `say`.
pub async fn run(
    session: &Session<'_>,
    ring: &mut RingRun,
    link: &mut Link,
    poll: Duration,
    say: &mut dyn FnMut(String),
) -> Result<(), String> {
    let store = store(session.config)?;
    let mut last = String::new();
    loop {
        let report = step(ring, link, session.backend, &store, unix_now())
            .await
            .map_err(|e| e.to_string())?;
        for problem in report.problems {
            say(format!("problem: {problem}"));
        }
        let line = describe(ring);
        if line != last {
            say(line.clone());
            last = line;
        }
        if ring.finished() {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

/// One line per stored ring run, and one per file that was refused.
pub fn list(config: &Config) -> Result<Vec<String>, String> {
    let loaded = store(config)?.load().map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = loaded.runs.iter().map(describe).collect();
    lines.extend(
        loaded
            .problems
            .iter()
            .map(|(file, why)| format!("refused {file}: {why}")),
    );
    Ok(lines)
}

/// Carries on with every stored ring run that is not finished, until all of them are.
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
    let mut rings = Vec::new();
    for ring in loaded.runs {
        if !ring.finished() && ring.instance() == instance.pubkey {
            let link = link_for(session, instance, ring.trade_index(), since).await?;
            say(describe(&ring));
            rings.push((ring, link));
        }
    }
    loop {
        let mut busy = false;
        for (ring, link) in &mut rings {
            if ring.finished() {
                continue;
            }
            busy = true;
            let before = describe(ring);
            let report = step(ring, link, session.backend, &store, unix_now())
                .await
                .map_err(|e| e.to_string())?;
            for problem in report.problems {
                say(format!("problem: {problem}"));
            }
            let after = describe(ring);
            if after != before {
                say(after);
            }
        }
        if !busy {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}
