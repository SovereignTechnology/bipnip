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
//! [instance]                          # the bipnip instance leases run on
//! pubkey = "npub1…"                   # its Nostr key, npub or hex
//! pow = 0                             # proof of work it asks for, in bits
//! ```

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
/// Most proof of work an instance may ask for: above this a message would take hours to make.
pub const MAX_POW_BITS: u8 = 32;

/// The instance leases run on.
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
    #[serde(default)]
    pow: u8,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    network: String,
    data_dir: PathBuf,
    #[serde(default)]
    relays: Vec<String>,
    lnd: Option<LndConfig>,
    instance: Option<RawInstance>,
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
    /// The instance leases run on, if one is configured.
    pub instance: Option<InstanceConfig>,
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
        let instance = match raw.instance {
            None => None,
            Some(raw) => {
                if raw.pow > MAX_POW_BITS {
                    return Err(format!(
                        "config: instance.pow is {} bits; at most {MAX_POW_BITS}",
                        raw.pow
                    ));
                }
                let pubkey = PublicKey::parse(&raw.pubkey)
                    .map_err(|_| "config: instance.pubkey must be an npub or 64 hex".to_string())?;
                Some(InstanceConfig {
                    pubkey,
                    pow: raw.pow,
                })
            }
        };
        Ok(Config {
            network,
            data_dir: raw.data_dir,
            relays: raw.relays,
            lnd: raw.lnd,
            instance,
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
    fn parses_the_instance() {
        let key = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let text = format!("{GOOD}\n[instance]\npubkey = \"{key}\"\npow = 12\n");
        let c = Config::parse(&text).expect("parses");
        let instance = c.instance.clone().expect("instance");
        assert_eq!(instance.pubkey.to_hex(), key);
        assert_eq!(instance.pow, 12);
        assert_eq!(c.leases_dir(), PathBuf::from("/tmp/bipnip/leases"));
        assert!(Config::parse(GOOD).expect("parses").instance.is_none());
        for (bad, why) in [
            (text.replace(key, "not-a-key"), "pubkey"),
            (text.replace("pow = 12", "pow = 33"), "too much work"),
            (text.replace("pow = 12", "secret = \"x\""), "unknown key"),
        ] {
            assert!(Config::parse(&bad).is_err(), "{why}");
        }
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
