//! The agent's configuration file (TOML). It names files that hold secrets; it never holds one.
//!
//! ```toml
//! network = "regtest"                 # bitcoin | testnet | testnet4 | signet | regtest
//! data_dir = "/home/me/.bipnip"       # holds the owner-only mnemonic file
//! relays = ["wss://relay.example"]
//!
//! [lnd]
//! address = "https://127.0.0.1:10009"
//! tls_cert = "/home/me/.lnd/tls.cert"
//! macaroon = "/home/me/.lnd/bipnip.macaroon"
//!
//! [[instance]]                        # a bipnip instance to trade on; may appear more than once
//! pubkey = "npub1…"                   # its Nostr key, npub or hex
//! pow = 0                             # optional: proof of work, in bits. Left out, it is read
//!                                     # from the instance's own info event.
//!
//! [policy]                            # what the daemon may do unasked; every limit defaults to 0
//! max_fee_per_lease_sat = 0
//! max_fee_per_day_sat = 0
//! max_capacity_per_day_sat = 0
//! max_bond_sat = 0
//! max_runs = 0
//! allowed_peers = []                  # node keys (66 hex); empty means any
//! ```
//!
//! A single `[instance]` table is read as one `[[instance]]`.

use std::path::PathBuf;
use std::str::FromStr;

use mostro_core::node_binding::Network;
use nostr_sdk::prelude::PublicKey;
use serde::Deserialize;

/// Name of the mnemonic file inside `data_dir`.
pub const MNEMONIC_FILE: &str = "mnemonic";
/// Name of the directory inside `data_dir` that holds lease runs.
pub const LEASES_DIR: &str = "leases";
/// Name of the directory inside `data_dir` that holds ring runs.
pub const RINGS_DIR: &str = "rings";
/// Name of the directory inside `data_dir` that holds what the daemon granted and what waits
/// for approval.
pub const POLICY_DIR: &str = "policy";
/// Name of the lock file inside `data_dir`: whoever holds it is the one process driving runs.
pub const LOCK_FILE: &str = "agent.lock";
/// Name of the daemon's socket inside `data_dir`.
pub const SOCKET_FILE: &str = "agent.sock";
/// Most proof of work an instance may ask for: above this a message would take hours to make.
pub const MAX_POW_BITS: u8 = 32;

/// An instance as the configuration names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceEntry {
    /// The instance's Nostr key.
    pub pubkey: PublicKey,
    /// Proof of work to put on messages, in bits, when the configuration fixes it. Otherwise
    /// it is read from the instance's info event.
    pub pow: Option<u8>,
}

/// An instance to talk to, with its proof of work settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceConfig {
    /// The instance's Nostr key.
    pub pubkey: PublicKey,
    /// Proof of work the instance asks for on messages, in bits.
    pub pow: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInstance {
    pubkey: String,
    pow: Option<u8>,
}

/// `[instance]` (one table) or `[[instance]]` (several).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawInstances {
    One(RawInstance),
    Many(Vec<RawInstance>),
}

/// How to reach an LND node.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LndConfig {
    /// gRPC address, `https://host:port`.
    pub address: String,
    /// LND's TLS certificate.
    pub tls_cert: PathBuf,
    /// A macaroon limited to what the agent needs (see `bipnip_ln::lnd`).
    pub macaroon: PathBuf,
}

/// Limits on what the agent does with the node's money without being asked again.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// The most a buyer's whole fee for one lease may be, upfront and completion together, in
    /// satoshi.
    #[serde(default)]
    pub max_fee_per_lease_sat: u64,
    /// The most all leases taken within a day may cost in fees, in satoshi.
    #[serde(default)]
    pub max_fee_per_day_sat: u64,
    /// The most capacity this agent may put into channels it opens within a day (leases sold,
    /// rings), in satoshi.
    #[serde(default)]
    pub max_capacity_per_day_sat: u64,
    /// The largest bond the agent will lock at an instance, in satoshi. 0, the default, means
    /// it joins nothing that asks for a bond.
    #[serde(default)]
    pub max_bond_sat: u64,
    /// How many runs may be under way at once.
    #[serde(default)]
    pub max_runs: u32,
    /// The only nodes this agent opens channels to or leases from (66 hex each); empty means
    /// any node.
    #[serde(default)]
    pub allowed_peers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    network: String,
    data_dir: PathBuf,
    #[serde(default)]
    relays: Vec<String>,
    lnd: Option<LndConfig>,
    instance: Option<RawInstances>,
    #[serde(default)]
    policy: PolicyConfig,
}

/// The parsed, checked configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The network the node must be on.
    pub network: Network,
    /// Directory for the agent's own files.
    pub data_dir: PathBuf,
    /// Relays to publish to and read from (`ws://` or `wss://`).
    pub relays: Vec<String>,
    /// The LND node, if one is configured.
    pub lnd: Option<LndConfig>,
    /// The instances to trade on, in the order the configuration lists them.
    pub instances: Vec<InstanceEntry>,
    /// Spending limits.
    pub policy: PolicyConfig,
}

impl Config {
    /// Parses and checks a configuration. Unknown keys are errors, so a typo cannot silently
    /// leave a setting at its default.
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| format!("config: {}", e.message()))?;
        let network = Network::from_str(&raw.network).map_err(|_| {
            format!(
                "config: unknown network {:?} (bitcoin, testnet, testnet4, signet, regtest)",
                raw.network
            )
        })?;
        if !raw.data_dir.is_absolute() {
            return Err("config: data_dir must be an absolute path".into());
        }
        for relay in &raw.relays {
            if !(relay.starts_with("wss://") || relay.starts_with("ws://")) {
                return Err(format!(
                    "config: relay {relay:?} must start with wss:// or ws://"
                ));
            }
        }
        if let Some(lnd) = &raw.lnd {
            if !lnd.address.starts_with("https://") {
                return Err("config: lnd.address must start with https://".into());
            }
            if !lnd.tls_cert.is_absolute() || !lnd.macaroon.is_absolute() {
                return Err("config: lnd.tls_cert and lnd.macaroon must be absolute paths".into());
            }
        }
        let raw_instances = match raw.instance {
            None => Vec::new(),
            Some(RawInstances::One(one)) => vec![one],
            Some(RawInstances::Many(many)) => many,
        };
        let mut instances: Vec<InstanceEntry> = Vec::new();
        for raw in raw_instances {
            if raw.pow.is_some_and(|pow| pow > MAX_POW_BITS) {
                return Err(format!(
                    "config: instance.pow is {} bits; at most {MAX_POW_BITS}",
                    raw.pow.unwrap_or_default()
                ));
            }
            let pubkey = PublicKey::parse(&raw.pubkey)
                .map_err(|_| "config: instance.pubkey must be an npub or 64 hex".to_string())?;
            if instances.iter().any(|known| known.pubkey == pubkey) {
                return Err("config: the same instance is listed twice".into());
            }
            instances.push(InstanceEntry {
                pubkey,
                pow: raw.pow,
            });
        }
        let mut policy = raw.policy;
        for peer in &mut policy.allowed_peers {
            *peer = peer.to_ascii_lowercase();
            let key = bipnip_ln::hexutil::node_key(peer)
                .ok()
                .and_then(|bytes| secp256k1::PublicKey::from_slice(&bytes).ok());
            if key.is_none() {
                return Err(
                    "config: policy.allowed_peers must be node public keys (66 hex)".into(),
                );
            }
        }
        Ok(Config {
            network,
            data_dir: raw.data_dir,
            relays: raw.relays,
            lnd: raw.lnd,
            instances,
            policy,
        })
    }

    /// Where lease runs are stored.
    pub fn leases_dir(&self) -> PathBuf {
        self.data_dir.join(LEASES_DIR)
    }

    /// Where ring runs are stored.
    pub fn rings_dir(&self) -> PathBuf {
        self.data_dir.join(RINGS_DIR)
    }

    /// Where the mnemonic lives.
    pub fn mnemonic_path(&self) -> PathBuf {
        self.data_dir.join(MNEMONIC_FILE)
    }

    /// Where grants and parked requests are stored.
    pub fn policy_dir(&self) -> PathBuf {
        self.data_dir.join(POLICY_DIR)
    }

    /// The lock file of the data directory.
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join(LOCK_FILE)
    }

    /// The daemon's socket.
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join(SOCKET_FILE)
    }

    /// The configured instance with this key, if any.
    pub fn instance(&self, pubkey: &PublicKey) -> Option<&InstanceEntry> {
        self.instances.iter().find(|entry| entry.pubkey == *pubkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
network = "regtest"
data_dir = "/tmp/bipnip"
relays = ["wss://a.example", "ws://127.0.0.1:7777"]

[lnd]
address = "https://127.0.0.1:10009"
tls_cert = "/tmp/tls.cert"
macaroon = "/tmp/a.macaroon"
"#;

    #[test]
    fn parses_a_full_config() {
        let c = Config::parse(GOOD).expect("parses");
        assert_eq!(c.network, Network::Regtest);
        assert_eq!(c.relays.len(), 2);
        assert_eq!(c.mnemonic_path(), PathBuf::from("/tmp/bipnip/mnemonic"));
        assert_eq!(c.lnd.expect("lnd").address, "https://127.0.0.1:10009");
    }

    #[test]
    fn no_bond_is_locked_unless_the_config_sets_a_limit() {
        let c = Config::parse(GOOD).expect("parses");
        assert_eq!(c.policy.max_bond_sat, 0);
        let c =
            Config::parse(&format!("{GOOD}\n[policy]\nmax_bond_sat = 50000\n")).expect("parses");
        assert_eq!(c.policy.max_bond_sat, 50_000);
        assert!(Config::parse(&format!("{GOOD}\n[policy]\nmax_bond = 1\n")).is_err());
    }

    #[test]
    fn parses_the_instance() {
        let key = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let text = format!("{GOOD}\n[instance]\npubkey = \"{key}\"\npow = 12\n");
        let c = Config::parse(&text).expect("parses");
        let [instance] = c.instances.as_slice() else {
            panic!("one instance");
        };
        assert_eq!(instance.pubkey.to_hex(), key);
        assert_eq!(instance.pow, Some(12));
        assert_eq!(c.leases_dir(), PathBuf::from("/tmp/bipnip/leases"));
        assert!(Config::parse(GOOD).expect("parses").instances.is_empty());
        for (bad, why) in [
            (text.replace(key, "not-a-key"), "pubkey"),
            (text.replace("pow = 12", "pow = 33"), "too much work"),
            (text.replace("pow = 12", "secret = \"x\""), "unknown key"),
        ] {
            assert!(Config::parse(&bad).is_err(), "{why}");
        }
    }

    #[test]
    fn several_instances_and_a_proof_of_work_left_to_the_instance() {
        let a = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let b = "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
        let text = format!(
            "{GOOD}\n[[instance]]\npubkey = \"{a}\"\n\n[[instance]]\npubkey = \"{b}\"\npow = 8\n"
        );
        let c = Config::parse(&text).expect("parses");
        assert_eq!(c.instances.len(), 2);
        assert_eq!(c.instances[0].pow, None);
        assert_eq!(c.instances[1].pow, Some(8));
        let second = PublicKey::parse(b).expect("key");
        assert_eq!(c.instance(&second).map(|entry| entry.pow), Some(Some(8)));
        // The same instance twice is a mistake, not two instances.
        assert!(Config::parse(&text.replace(b, a)).is_err());
    }

    #[test]
    fn every_limit_is_zero_unless_set_and_peers_must_be_node_keys() {
        let c = Config::parse(GOOD).expect("parses");
        assert_eq!(c.policy, PolicyConfig::default());
        assert_eq!(c.policy.max_runs, 0);
        let node = format!(
            "02{}",
            "79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798"
        );
        let text = format!(
            "{GOOD}\n[policy]\nmax_fee_per_lease_sat = 5000\nmax_fee_per_day_sat = 20000\n\
             max_capacity_per_day_sat = 3000000\nmax_runs = 4\nallowed_peers = [\"{node}\"]\n"
        );
        let c = Config::parse(&text).expect("parses");
        assert_eq!(c.policy.max_fee_per_lease_sat, 5_000);
        assert_eq!(c.policy.max_fee_per_day_sat, 20_000);
        assert_eq!(c.policy.max_capacity_per_day_sat, 3_000_000);
        assert_eq!(c.policy.max_runs, 4);
        // Stored in lower case, so it compares equal to what a node reports.
        assert_eq!(c.policy.allowed_peers, vec![node.to_ascii_lowercase()]);
        assert!(Config::parse(&text.replace(&node, "02abcd")).is_err());
        assert!(Config::parse(&text.replace(&node, &"zz".repeat(33))).is_err());
        assert_eq!(c.socket_path(), PathBuf::from("/tmp/bipnip/agent.sock"));
        assert_eq!(c.lock_path(), PathBuf::from("/tmp/bipnip/agent.lock"));
    }

    #[test]
    fn the_node_is_optional() {
        let c = Config::parse("network = \"bitcoin\"\ndata_dir = \"/x\"\n").expect("parses");
        assert_eq!(c.network, Network::Bitcoin);
        assert!(c.lnd.is_none() && c.relays.is_empty());
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        for (bad, why) in [
            (
                GOOD.replace("network = \"regtest\"", "network = \"mainnet\""),
                "network",
            ),
            (
                GOOD.replace("data_dir = \"/tmp/bipnip\"", "data_dir = \"relative\""),
                "relative dir",
            ),
            (
                GOOD.replace("wss://a.example", "https://a.example"),
                "relay scheme",
            ),
            (
                GOOD.replace("https://127.0.0.1:10009", "127.0.0.1:10009"),
                "lnd scheme",
            ),
            (
                GOOD.replace("tls_cert = \"/tmp/tls.cert\"", "tls_cert = \"tls.cert\""),
                "relative cert",
            ),
            (GOOD.replace("relays =", "relay ="), "unknown key"),
            (format!("{GOOD}mnemonic = \"words\"\n"), "secret in config"),
        ] {
            assert!(Config::parse(&bad).is_err(), "{why}");
        }
    }
}
