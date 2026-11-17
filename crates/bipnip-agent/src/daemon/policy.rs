//! What the daemon may do with the node's money on a request alone, and the record of what it
//! was allowed (`docs/decisions/0010-agent-daemon-and-local-api.md`, point 5).
//!
//! Three things live here:
//!
//! - the rules ([`over`]): a request is weighed as an [`Ask`] (the fee it would pay, the
//!   capacity it would open, the bond it would lock, the peer) against the `[policy]` limits
//!   and what the last day already used;
//! - the record ([`PolicyStore`]): every run the daemon starts gets a [`Grant`], on disk before
//!   the run's first message leaves, so a day's use is counted from files and survives a
//!   restart; a request over a limit is stored as [`Parked`] until it is approved or denied;
//! - the second check ([`Guard`]): the node as a run sees it. A payment or a channel open that
//!   goes beyond the run's grant is refused at the node's door, whatever led the run there.
//!
//! Every limit defaults to zero, so with no `[policy]` section every request is parked.
//!
//! What this is not: a defence against the user account. Whoever can open the socket can also
//! approve. The limits bound what a request does *unseen*.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use bipnip_ln::{
    BackendError, ChannelView, GraphChannel, HoldInvoiceRequest, HoldState, LightningBackend,
    NodeInfo, OpenRequest, PaymentState,
};
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::api::is_name;
use crate::config::PolicyConfig;
use crate::secretfile::{self, SecretFileError};

/// How far back "a day" reaches, in seconds.
pub const DAY_SECS: u64 = 24 * 3600;

/// What a request would do with the node's money.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    /// The whole fee it would pay, upfront and completion, in satoshi (a lease taken).
    pub fee_sat: u64,
    /// The capacity it would put into a channel this node opens, in satoshi (a lease sold, a
    /// ring).
    pub capacity_sat: u64,
    /// The bond it would lock at the instance, in satoshi.
    pub bond_sat: u64,
    /// The node it would lease from, when that is known at the start (66 hex).
    pub peer: Option<String>,
}

impl Ask {
    /// True when `self` asks for no more than `shown` did, of the same peer: what an approval
    /// of `shown` covers.
    pub fn within(&self, shown: &Ask) -> bool {
        self.fee_sat <= shown.fee_sat
            && self.capacity_sat <= shown.capacity_sat
            && self.bond_sat <= shown.bond_sat
            && self.peer == shown.peer
    }
}

/// What is already committed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Fees of the leases taken within the last day, in satoshi.
    pub fee_day_sat: u64,
    /// Capacity of the channels this node was to open within the last day, in satoshi.
    pub capacity_day_sat: u64,
    /// Runs under way.
    pub runs: u32,
}

/// The limits `ask` is over, each named by its key in `[policy]`; empty when it is within all
/// of them.
pub fn over(policy: &PolicyConfig, ask: &Ask, usage: &Usage) -> Vec<String> {
    let mut reasons = Vec::new();
    if usage.runs >= policy.max_runs {
        reasons.push(format!(
            "{} runs are under way; max_runs is {}",
            usage.runs, policy.max_runs
        ));
    }
    if ask.fee_sat > policy.max_fee_per_lease_sat {
        reasons.push(format!(
            "the fee is {} sat; max_fee_per_lease_sat is {}",
            ask.fee_sat, policy.max_fee_per_lease_sat
        ));
    }
    if ask.fee_sat > 0 && usage.fee_day_sat.saturating_add(ask.fee_sat) > policy.max_fee_per_day_sat
    {
        reasons.push(format!(
            "the fee is {} sat on top of {} in the last day; max_fee_per_day_sat is {}",
            ask.fee_sat, usage.fee_day_sat, policy.max_fee_per_day_sat
        ));
    }
    if ask.capacity_sat > 0
        && usage.capacity_day_sat.saturating_add(ask.capacity_sat) > policy.max_capacity_per_day_sat
    {
        reasons.push(format!(
            "it would open {} sat on top of {} in the last day; max_capacity_per_day_sat is {}",
            ask.capacity_sat, usage.capacity_day_sat, policy.max_capacity_per_day_sat
        ));
    }
    if ask.bond_sat > policy.max_bond_sat {
        reasons.push(format!(
            "the bond is {} sat; max_bond_sat is {}",
            ask.bond_sat, policy.max_bond_sat
        ));
    }
    if let Some(peer) = &ask.peer
        && !peer_allowed(policy, peer)
    {
        reasons.push(format!("node {peer} is not in allowed_peers"));
    }
    reasons
}

/// True when `[policy] allowed_peers` lets this node in: the list is empty, or names it.
pub fn peer_allowed(policy: &PolicyConfig, peer: &str) -> bool {
    policy.allowed_peers.is_empty() || policy.allowed_peers.iter().any(|allowed| allowed == peer)
}

/// A payment a run made, or set out to make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paid {
    /// The payment hash, 64 hex.
    pub hash: String,
    /// The invoice's amount, in millisatoshi (routing fees are capped apart, per payment).
    pub msat: u64,
}

/// What one run was allowed, and what it has used of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// The run, by the name it is stored under.
    pub run: String,
    /// When it was granted, unix seconds.
    pub at: u64,
    /// What the run asked for when it started. Its payments and its channel stay within this.
    pub ask: Ask,
    /// True when it was over a limit and a person approved it.
    pub approved: bool,
    /// The payments the run set out to make.
    pub paid: Vec<Paid>,
    /// The capacity of the channels the run opened, in satoshi.
    pub opened_sat: u64,
}

impl Grant {
    /// A grant for a run that starts now.
    pub fn new(run: String, at: u64, ask: Ask, approved: bool) -> Self {
        Grant {
            run,
            at,
            ask,
            approved,
            paid: Vec::new(),
            opened_sat: 0,
        }
    }

    /// The most the run's invoices may add up to, in millisatoshi: its fee and its bond.
    pub fn pay_allowance_msat(&self) -> u64 {
        self.ask
            .fee_sat
            .saturating_add(self.ask.bond_sat)
            .saturating_mul(1_000)
    }
}

/// A request that was over a limit, kept as it was shown until a person decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parked {
    /// Its name.
    pub approval: String,
    /// When it was parked, unix seconds.
    pub at: u64,
    /// The method asked for.
    pub method: String,
    /// Its parameters, as sent.
    pub params: Value,
    /// What it would do with the node's money.
    pub ask: Ask,
    /// The limits it is over.
    pub over: Vec<String>,
}

/// Why the policy's files could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "policy records: {}", self.0)
    }
}

impl std::error::Error for PolicyError {}

fn file_error(path: &Path, error: SecretFileError) -> PolicyError {
    PolicyError(format!("{}: {error}", path.display()))
}

/// The grants and the parked requests, one owner-only file each.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    dir: PathBuf,
}

impl PolicyStore {
    /// Opens the store, creating its directory if needed.
    pub fn open(dir: &Path) -> Result<Self, PolicyError> {
        secretfile::create_private_dir(dir).map_err(|e| file_error(dir, e))?;
        Ok(PolicyStore {
            dir: dir.to_path_buf(),
        })
    }

    fn grant_path(&self, run: &str) -> Result<PathBuf, PolicyError> {
        if !is_name(run) {
            return Err(PolicyError("that is not the name of a run".into()));
        }
        Ok(self.dir.join(format!("grant-{run}.json")))
    }

    fn parked_path(&self, approval: &str) -> Result<PathBuf, PolicyError> {
        if !is_name(approval) {
            return Err(PolicyError("that is not the name of an approval".into()));
        }
        Ok(self.dir.join(format!("approval-{approval}.json")))
    }

    /// Writes a grant, replacing the run's earlier one.
    pub fn save_grant(&self, grant: &Grant) -> Result<(), PolicyError> {
        let path = self.grant_path(&grant.run)?;
        let json = serde_json::to_string(grant).map_err(|e| PolicyError(e.to_string()))?;
        secretfile::replace_private(&path, &json).map_err(|e| file_error(&path, e))
    }

    /// The grant of `run`, if the daemon started it.
    pub fn grant(&self, run: &str) -> Result<Option<Grant>, PolicyError> {
        let path = self.grant_path(run)?;
        if !path.exists() {
            return Ok(None);
        }
        let json = secretfile::read_secret(&path).map_err(|e| file_error(&path, e))?;
        let grant: Grant = serde_json::from_str(&json)
            .map_err(|e| PolicyError(format!("{}: {e}", path.display())))?;
        if grant.run != run {
            return Err(PolicyError(format!(
                "{}: the file is not this run's grant",
                path.display()
            )));
        }
        Ok(Some(grant))
    }

    fn names(&self, prefix: &str) -> Result<Vec<String>, PolicyError> {
        let io = |e: std::io::Error| PolicyError(format!("{}: {e}", self.dir.display()));
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(io)? {
            let name = entry
                .map_err(io)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(stem) = name
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(".json"))
                && is_name(stem)
            {
                names.push(stem.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    /// What the grants of the last day add up to at `now`. A grant that cannot be read is an
    /// error, not a zero: a limit is never passed because a file was unreadable.
    pub fn used_since(&self, now: u64) -> Result<(u64, u64), PolicyError> {
        let (mut fee, mut capacity) = (0u64, 0u64);
        for run in self.names("grant-")? {
            let Some(grant) = self.grant(&run)? else {
                continue;
            };
            if grant.at.saturating_add(DAY_SECS) > now {
                fee = fee.saturating_add(grant.ask.fee_sat);
                capacity = capacity.saturating_add(grant.ask.capacity_sat);
            }
        }
        Ok((fee, capacity))
    }

    /// Stores a parked request.
    pub fn park(&self, parked: &Parked) -> Result<(), PolicyError> {
        let path = self.parked_path(&parked.approval)?;
        let json = serde_json::to_string(parked).map_err(|e| PolicyError(e.to_string()))?;
        secretfile::replace_private(&path, &json).map_err(|e| file_error(&path, e))
    }

    /// The parked request of that name, if there is one.
    pub fn parked(&self, approval: &str) -> Result<Option<Parked>, PolicyError> {
        let path = self.parked_path(approval)?;
        if !path.exists() {
            return Ok(None);
        }
        let json = secretfile::read_secret(&path).map_err(|e| file_error(&path, e))?;
        let parked: Parked = serde_json::from_str(&json)
            .map_err(|e| PolicyError(format!("{}: {e}", path.display())))?;
        if parked.approval != approval {
            return Err(PolicyError(format!(
                "{}: the file is not this approval",
                path.display()
            )));
        }
        Ok(Some(parked))
    }

    /// Every parked request, oldest first.
    pub fn all_parked(&self) -> Result<Vec<Parked>, PolicyError> {
        let mut all = Vec::new();
        for approval in self.names("approval-")? {
            if let Some(parked) = self.parked(&approval)? {
                all.push(parked);
            }
        }
        all.sort_by(|a, b| (a.at, &a.approval).cmp(&(b.at, &b.approval)));
        Ok(all)
    }

    /// Forgets a parked request: it was approved or denied.
    pub fn unpark(&self, approval: &str) -> Result<(), PolicyError> {
        let path = self.parked_path(approval)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PolicyError(format!("{}: {e}", path.display()))),
        }
    }
}

fn refused(why: String) -> BackendError {
    BackendError::Rejected(format!("policy: {why}"))
}

/// The node as one run sees it: everything passes through, except that a payment or a channel
/// open beyond the run's [`Grant`] is refused before it reaches the node.
///
/// A payment or an open is written into the grant before the node is asked, so a crash cannot
/// forget it. A payment the node says failed, or never heard of, no longer counts: an invoice
/// that expired and was replaced does not use the allowance twice. An open the node could not
/// answer for (it was unreachable) stays counted.
pub struct Guard<'a> {
    inner: &'a dyn LightningBackend,
    store: &'a PolicyStore,
    policy: &'a PolicyConfig,
    grant: Mutex<Grant>,
}

impl<'a> Guard<'a> {
    /// A guard over `inner` for the run `grant` belongs to.
    pub fn new(
        inner: &'a dyn LightningBackend,
        store: &'a PolicyStore,
        policy: &'a PolicyConfig,
        grant: Grant,
    ) -> Self {
        Guard {
            inner,
            store,
            policy,
            grant: Mutex::new(grant),
        }
    }

    /// The grant as it now stands, to keep for the run's next step.
    pub fn into_grant(self) -> Grant {
        self.grant
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> Grant {
        self.grant
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, change: impl FnOnce(&mut Grant)) -> Result<(), BackendError> {
        let mut grant = self.grant.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = grant.clone();
        change(&mut next);
        self.store
            .save_grant(&next)
            .map_err(|e| BackendError::Unavailable(format!("policy: {e}")))?;
        *grant = next;
        Ok(())
    }
}

#[async_trait]
impl LightningBackend for Guard<'_> {
    async fn node_info(&self) -> Result<NodeInfo, BackendError> {
        self.inner.node_info().await
    }

    async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
        self.inner.sign_message(msg).await
    }

    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError> {
        self.inner.add_hold_invoice(req).await
    }

    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError> {
        self.inner.hold_invoice_state(payment_hash).await
    }

    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        self.inner.settle_hold_invoice(preimage).await
    }

    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError> {
        self.inner.cancel_hold_invoice(payment_hash).await
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        max_fee_msat: u64,
        wait_secs: u32,
    ) -> Result<PaymentState, BackendError> {
        let invoice = Bolt11Invoice::from_str(bolt11)
            .map_err(|_| refused("that is not an invoice".into()))?;
        let hash = invoice.payment_hash().to_string();
        let msat = invoice
            .amount_milli_satoshis()
            .ok_or_else(|| refused("the invoice names no amount".into()))?;
        let grant = self.snapshot();
        let mut committed = 0u64;
        for paid in grant.paid.iter().filter(|paid| paid.hash != hash) {
            match self.inner.payment_state(&paid.hash).await {
                // Failed, or the node never heard of it: nothing left the wallet.
                Ok(PaymentState::Failed { .. }) | Err(BackendError::Rejected(_)) => {}
                // Paid, under way, or the node could not say: it counts.
                _ => committed = committed.saturating_add(paid.msat),
            }
        }
        let allowance = grant.pay_allowance_msat();
        if committed.saturating_add(msat) > allowance {
            return Err(refused(format!(
                "this run was granted {allowance} msat of payments; {committed} are committed \
                 and this invoice asks for {msat}"
            )));
        }
        self.record(|grant| {
            grant.paid.retain(|paid| paid.hash != hash);
            grant.paid.push(Paid {
                hash: hash.clone(),
                msat,
            });
        })?;
        self.inner
            .pay_invoice(bolt11, max_fee_msat, wait_secs)
            .await
    }

    async fn payment_state(&self, payment_hash: &str) -> Result<PaymentState, BackendError> {
        self.inner.payment_state(payment_hash).await
    }

    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        description: &str,
    ) -> Result<String, BackendError> {
        self.inner
            .add_invoice(amount_msat, expiry_secs, description)
            .await
    }

    async fn connect(&self, remote_pubkey: &str, remote_addr: &str) -> Result<(), BackendError> {
        self.inner.connect(remote_pubkey, remote_addr).await
    }

    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError> {
        if !peer_allowed(self.policy, &req.remote_pubkey) {
            return Err(refused(format!(
                "node {} is not in allowed_peers",
                req.remote_pubkey
            )));
        }
        let grant = self.snapshot();
        if grant.opened_sat.saturating_add(req.capacity_sat) > grant.ask.capacity_sat {
            return Err(refused(format!(
                "this run was granted {} sat of channel capacity; {} are opened and this asks \
                 for {}",
                grant.ask.capacity_sat, grant.opened_sat, req.capacity_sat
            )));
        }
        // Counted before the node is asked, as a payment is: a crash cannot forget it. Taken
        // back only when the node says plainly that it did not open.
        self.record(|grant| {
            grant.opened_sat = grant.opened_sat.saturating_add(req.capacity_sat);
        })?;
        match self.inner.open_channel(req).await {
            Ok(outpoint) => Ok(outpoint),
            Err(BackendError::Rejected(why)) => {
                self.record(|grant| {
                    grant.opened_sat = grant.opened_sat.saturating_sub(req.capacity_sat);
                })?;
                Err(BackendError::Rejected(why))
            }
            Err(e) => Err(e),
        }
    }

    async fn channels_with(&self, remote_pubkey: &str) -> Result<Vec<ChannelView>, BackendError> {
        self.inner.channels_with(remote_pubkey).await
    }

    async fn graph_node(&self, node_pubkey: &str) -> Result<Option<(u32, u64)>, BackendError> {
        self.inner.graph_node(node_pubkey).await
    }

    async fn graph_channels(&self, node_pubkey: &str) -> Result<Vec<GraphChannel>, BackendError> {
        self.inner.graph_channels(node_pubkey).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::lease::tests::{Node, World, secret};

    fn policy() -> PolicyConfig {
        PolicyConfig {
            max_fee_per_lease_sat: 5_000,
            max_fee_per_day_sat: 12_000,
            max_capacity_per_day_sat: 2_000_000,
            max_bond_sat: 20_000,
            max_runs: 2,
            allowed_peers: Vec::new(),
        }
    }

    fn ask(fee_sat: u64, capacity_sat: u64, bond_sat: u64) -> Ask {
        Ask {
            fee_sat,
            capacity_sat,
            bond_sat,
            peer: None,
        }
    }

    fn store(name: &str) -> PolicyStore {
        let dir = std::env::temp_dir().join(format!("bipnip-policy-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        PolicyStore::open(&dir).expect("store")
    }

    #[test]
    fn a_fresh_agent_lets_nothing_through() {
        let nothing = PolicyConfig::default();
        let idle = Usage::default();
        // Even a request that moves no money needs a run, and no run is allowed.
        assert_eq!(over(&nothing, &ask(0, 0, 0), &idle).len(), 1);
        let reasons = over(&nothing, &ask(1, 1, 1), &idle);
        for key in [
            "max_runs",
            "max_fee_per_lease_sat",
            "max_fee_per_day_sat",
            "max_capacity_per_day_sat",
            "max_bond_sat",
        ] {
            assert!(reasons.iter().any(|reason| reason.contains(key)), "{key}");
        }
    }

    #[test]
    fn each_limit_is_exact() {
        let policy = policy();
        let idle = Usage::default();
        assert!(over(&policy, &ask(5_000, 2_000_000, 20_000), &idle).is_empty());
        for (ask, usage, key) in [
            (ask(5_001, 0, 0), idle, "max_fee_per_lease_sat"),
            (ask(0, 2_000_001, 0), idle, "max_capacity_per_day_sat"),
            (ask(0, 0, 20_001), idle, "max_bond_sat"),
            (
                ask(4_000, 0, 0),
                Usage {
                    fee_day_sat: 8_001,
                    ..idle
                },
                "max_fee_per_day_sat",
            ),
            (
                ask(0, 500_000, 0),
                Usage {
                    capacity_day_sat: 1_500_001,
                    ..idle
                },
                "max_capacity_per_day_sat",
            ),
            (ask(0, 0, 0), Usage { runs: 2, ..idle }, "max_runs"),
        ] {
            let reasons = over(&policy, &ask, &usage);
            assert_eq!(reasons.len(), 1, "{key}: {reasons:?}");
            assert!(reasons[0].contains(key), "{key}: {reasons:?}");
        }
        // A day's fee already used does not stand in the way of a request that pays none.
        let spent = Usage {
            fee_day_sat: 1_000_000,
            ..idle
        };
        assert!(over(&policy, &ask(0, 100_000, 0), &spent).is_empty());
        // Amounts near the top of the range do not wrap around a limit.
        let full = Usage {
            fee_day_sat: u64::MAX,
            capacity_day_sat: u64::MAX,
            runs: 0,
        };
        assert_eq!(over(&policy, &ask(1, 1, 0), &full).len(), 2);
    }

    #[test]
    fn only_listed_peers_when_there_is_a_list() {
        let a = format!("02{}", "11".repeat(32));
        let b = format!("03{}", "22".repeat(32));
        let mut policy = policy();
        let with_peer = |peer: &str| Ask {
            peer: Some(peer.to_string()),
            ..ask(1_000, 0, 0)
        };
        assert!(over(&policy, &with_peer(&a), &Usage::default()).is_empty());
        policy.allowed_peers = vec![a.clone()];
        assert!(over(&policy, &with_peer(&a), &Usage::default()).is_empty());
        let reasons = over(&policy, &with_peer(&b), &Usage::default());
        assert!(reasons.iter().any(|r| r.contains("allowed_peers")));
        // A request that does not know its peer yet is held to the list when it opens.
        assert!(over(&policy, &ask(0, 100_000, 0), &Usage::default()).is_empty());
    }

    #[test]
    fn an_approval_covers_what_was_shown_and_no_more() {
        let shown = Ask {
            peer: Some("02aa".into()),
            ..ask(3_000, 0, 10_000)
        };
        assert!(shown.within(&shown));
        assert!(
            Ask {
                fee_sat: 2_999,
                ..shown.clone()
            }
            .within(&shown)
        );
        for more in [
            Ask {
                fee_sat: 3_001,
                ..shown.clone()
            },
            Ask {
                bond_sat: 10_001,
                ..shown.clone()
            },
            Ask {
                capacity_sat: 1,
                ..shown.clone()
            },
            Ask {
                peer: Some("02bb".into()),
                ..shown.clone()
            },
            Ask {
                peer: None,
                ..shown.clone()
            },
        ] {
            assert!(!more.within(&shown), "{more:?}");
        }
    }

    #[test]
    fn a_days_use_is_counted_from_the_grants_on_disk() {
        let store = store("usage");
        let now = 1_800_000_000;
        store
            .save_grant(&Grant::new(
                "buyer-01".into(),
                now - 100,
                ask(3_000, 0, 0),
                false,
            ))
            .expect("save");
        store
            .save_grant(&Grant::new(
                "ring-02".into(),
                now - 200,
                ask(0, 400_000, 5_000),
                true,
            ))
            .expect("save");
        // Older than a day: no longer counted.
        store
            .save_grant(&Grant::new(
                "buyer-03".into(),
                now - DAY_SECS - 1,
                ask(9_000, 0, 0),
                false,
            ))
            .expect("save");
        assert_eq!(store.used_since(now), Ok((3_000, 400_000)));
        // Read again from disk by a new handle, as after a restart.
        let again = PolicyStore::open(&store.dir).expect("open");
        assert_eq!(again.used_since(now), Ok((3_000, 400_000)));
        assert_eq!(
            again.grant("ring-02").expect("read").map(|g| g.approved),
            Some(true)
        );
        assert_eq!(again.grant("ring-99"), Ok(None));
        // A grant that cannot be read stops the count; it is not counted as nothing.
        std::fs::write(store.dir.join("grant-buyer-04.json"), "{").expect("write");
        std::fs::set_permissions(
            store.dir.join("grant-buyer-04.json"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("chmod");
        assert!(again.used_since(now).is_err());
        // Names that are not ours never become paths.
        assert!(again.grant("../mnemonic").is_err());
        assert!(again.parked("a/b").is_err());
    }

    #[test]
    fn parked_requests_are_kept_until_decided() {
        let store = store("parked");
        let parked = |approval: &str, at: u64| Parked {
            approval: approval.into(),
            at,
            method: "lease.take".into(),
            params: serde_json::json!({"capacity_sat": 600_000}),
            ask: ask(3_000, 0, 0),
            over: vec!["max_runs".into()],
        };
        store.park(&parked("bb", 20)).expect("park");
        store.park(&parked("aa", 30)).expect("park");
        let all = store.all_parked().expect("list");
        assert_eq!(
            all.iter().map(|p| p.approval.as_str()).collect::<Vec<_>>(),
            vec!["bb", "aa"],
            "oldest first"
        );
        assert_eq!(store.parked("aa").expect("read"), Some(parked("aa", 30)));
        store.unpark("aa").expect("unpark");
        store.unpark("aa").expect("twice is fine");
        assert_eq!(store.parked("aa"), Ok(None));
        assert_eq!(store.all_parked().expect("list").len(), 1);
    }

    fn nodes() -> (Node, Node) {
        let world = Arc::new(Mutex::new(World::default()));
        (
            Node {
                world: world.clone(),
                secret: secret(1),
                network: "regtest",
            },
            Node {
                world,
                secret: secret(2),
                network: "regtest",
            },
        )
    }

    #[tokio::test]
    async fn a_run_pays_what_it_was_granted_and_not_a_millisatoshi_more() {
        let (ours, theirs) = nodes();
        let store = store("guard-pay");
        let policy = policy();
        // A lease with a fee of 3000 sat and a bond of 1000.
        let grant = Grant::new("buyer-0a".into(), 10, ask(3_000, 0, 1_000), false);
        let guard = Guard::new(&ours, &store, &policy, grant);
        let upfront = theirs
            .add_invoice(2_000_000, 600, "")
            .await
            .expect("invoice");
        let bond = theirs
            .add_invoice(1_000_000, 600, "")
            .await
            .expect("invoice");
        let rest = theirs
            .add_invoice(1_000_000, 600, "")
            .await
            .expect("invoice");
        let extra = theirs.add_invoice(1, 600, "").await.expect("invoice");
        for invoice in [&upfront, &bond, &rest] {
            guard
                .pay_invoice(invoice, 10, 5)
                .await
                .expect("within the grant");
        }
        // Paying the same invoice again is the same payment, not a new one.
        guard.pay_invoice(&bond, 10, 5).await.expect("again");
        let refused = guard.pay_invoice(&extra, 10, 5).await.expect_err("over");
        assert!(refused.to_string().contains("policy"), "{refused}");
        // The refusal happened before the node was asked.
        let hash = Bolt11Invoice::from_str(&extra)
            .expect("invoice")
            .payment_hash()
            .to_string();
        assert!(ours.payment_state(&hash).await.is_err());
        // What was paid is on disk, for the next step and the next start.
        let kept = guard.into_grant();
        assert_eq!(kept.paid.len(), 3);
        assert_eq!(store.grant("buyer-0a").expect("read"), Some(kept));
    }

    #[tokio::test]
    async fn a_payment_that_failed_gives_its_share_back() {
        let (ours, theirs) = nodes();
        let store = store("guard-failed");
        let policy = policy();
        let grant = Grant::new("buyer-0b".into(), 10, ask(1_000, 0, 0), false);
        let guard = Guard::new(&ours, &store, &policy, grant);
        let first = theirs
            .add_invoice(1_000_000, 600, "")
            .await
            .expect("invoice");
        let second = theirs
            .add_invoice(1_000_000, 600, "")
            .await
            .expect("invoice");
        guard.pay_invoice(&first, 10, 5).await.expect("pay");
        // While the first stands, a second of the same size is over the grant.
        assert!(guard.pay_invoice(&second, 10, 5).await.is_err());
        // The node now says the first failed: the invoice that replaces it may be paid.
        let hash = Bolt11Invoice::from_str(&first)
            .expect("invoice")
            .payment_hash()
            .to_string();
        ours.world.lock().expect("world").payments.insert(
            hash,
            PaymentState::Failed {
                reason: "expired".into(),
            },
        );
        guard
            .pay_invoice(&second, 10, 5)
            .await
            .expect("replacement");
    }

    #[tokio::test]
    async fn a_run_opens_the_capacity_it_was_granted_to_peers_that_are_allowed() {
        let (ours, theirs) = nodes();
        let store = store("guard-open");
        let mut policy = policy();
        let open = |capacity_sat: u64, peer: &str| OpenRequest {
            remote_pubkey: peer.to_string(),
            remote_addr: None,
            capacity_sat,
            announce: true,
            feerate_sat_vb: 2,
        };
        let stranger = format!("03{}", "33".repeat(32));

        let grant = Grant::new("ring-0c".into(), 10, ask(0, 500_000, 0), false);
        let guard = Guard::new(&ours, &store, &policy, grant);
        assert!(
            guard
                .open_channel(&open(500_001, &theirs.id()))
                .await
                .is_err()
        );
        guard
            .open_channel(&open(500_000, &theirs.id()))
            .await
            .expect("as granted");
        // The grant is used up: no second channel.
        assert!(guard.open_channel(&open(1, &theirs.id())).await.is_err());
        assert_eq!(ours.world.lock().expect("world").channels.len(), 1);
        assert_eq!(guard.into_grant().opened_sat, 500_000);

        // With a list of peers, a node outside it gets no channel, whatever was granted.
        policy.allowed_peers = vec![theirs.id()];
        let grant = Grant::new("ring-0d".into(), 10, ask(0, 500_000, 0), true);
        let guard = Guard::new(&ours, &store, &policy, grant);
        let refused = guard
            .open_channel(&open(100_000, &stranger))
            .await
            .expect_err("not listed");
        assert!(refused.to_string().contains("allowed_peers"), "{refused}");
        guard
            .open_channel(&open(100_000, &theirs.id()))
            .await
            .expect("listed");
        // A run that was granted no payments makes none.
        let invoice = theirs.add_invoice(1_000, 600, "").await.expect("invoice");
        assert!(guard.pay_invoice(&invoice, 10, 5).await.is_err());
    }
}
