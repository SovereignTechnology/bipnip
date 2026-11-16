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
//! ```

use std::path::PathBuf;
use std::str::FromStr;

use mostro_core::node_binding::Network;
use serde::Deserialize;

/// Name of the mnemonic file inside `data_dir`.
pub const MNEMONIC_FILE: &str = "mnemonic";

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
        Ok(Config {
            network,
            data_dir: raw.data_dir,
            relays: raw.relays,
            lnd: raw.lnd,
        })
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
