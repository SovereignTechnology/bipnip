//! `bipnip-agent`: runs next to a node runner's Lightning node, holds the mnemonic and the node
//! credentials (`docs/decisions/0002-instance-and-agent-roles.md`). For now a command at a time;
//! the long-running daemon and its unix-socket API come next.
//!
//! ```text
//! bipnip-agent --config FILE init              create the data dir and a new mnemonic file (600)
//! bipnip-agent --config FILE whoami            show the public keys (and the node, if configured)
//! bipnip-agent --config FILE bind [--revoke] [--publish]
//!                                              make the node binding event; --publish sends it
//! bipnip-agent --config FILE verify-binding NODE_PUBKEY
//!                                              read a node's bindings from the relays and resolve them
//! bipnip-agent --config FILE lease offer --min-capacity SAT --max-capacity SAT --duration BLOCKS
//!         --fee-base SAT --fee-ppm PPM --feerate SAT_PER_VB [--upfront PERCENT] [--confs N]
//!         [--private] [--expires-in SECS] [--cltv-delta BLOCKS] [--poll SECS]
//!                                              offer liquidity and see the lease through
//! bipnip-agent --config FILE lease take ORDER_ID --capacity SAT [--node-address HOST:PORT]
//!         [--max-fee-msat MSAT] [--max-cltv-delta BLOCKS] [--poll SECS]
//!                                              take an order and see the lease through
//! bipnip-agent --config FILE lease resume [--poll SECS]
//!                                              carry on with stored leases after a restart
//! bipnip-agent --config FILE lease list        show the stored leases
//! bipnip-agent --config FILE ring create --capacity SAT --slots N --feerate SAT_PER_VB
//!         [--window SECS] [--expires-in SECS] [--bond SAT] [--private]
//!         [--node-address HOST:PORT] [--poll SECS]
//!                                              propose a ring, take its first slot, see it through
//! bipnip-agent --config FILE ring join ORDER_ID --feerate SAT_PER_VB [--node-address HOST:PORT]
//!         [--poll SECS]                        take a slot of a ring and see it through
//! bipnip-agent --config FILE ring resume [--poll SECS]
//!                                              carry on with stored rings after a restart
//! bipnip-agent --config FILE ring list         show the stored rings
//! ```
//!
//! The `lease` and `ring` commands need an `[instance]` section and a node. A ring command runs
//! until the ring is over: every channel confirmed, or its window ended.
//!
//! The `lease` commands need an `[instance]` section and a node. They run until the lease is
//! finished: active when all of the fee was upfront (the default), otherwise until the rest is
//! paid at the end of the lease. A lease interrupted before that is picked up by `lease resume`.
//!
//! Nothing here prints the mnemonic or any credential.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bipnip_agent::binding::make_binding_event;
use bipnip_agent::config::Config;
use bipnip_agent::keys::Wallet;
use bipnip_agent::lease::cmd::{self, OfferParams, Session, TakeParams};
use bipnip_agent::lease::run::{BuyerConfig, SellerConfig};
use bipnip_agent::lease::{DEFAULT_MAX_CLTV_DELTA, DEFAULT_SELLER_CLTV_DELTA};
use bipnip_agent::ring::cmd::{self as ring_cmd, CreateParams, JoinParams};
use bipnip_agent::{relay, secretfile};
use bipnip_ln::LightningBackend;
use mostro_core::node_binding::{Action, latest_for_node};
use nostr_sdk::prelude::ToBech32;

fn usage() -> String {
    "usage: bipnip-agent --config FILE <init | whoami | bind [--revoke] [--publish] | \
     verify-binding NODE_PUBKEY | lease <offer | take ORDER_ID | resume | list> [options] | \
     ring <create | join ORDER_ID | resume | list> [options]> \
     (the options are listed at the top of the agent's source and in the README)"
        .to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_config(path: &PathBuf) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Config::parse(&text)
}

fn load_wallet(config: &Config) -> Result<Wallet, String> {
    let path = config.mnemonic_path();
    let words = secretfile::read_secret(&path)
        .map_err(|e| format!("mnemonic file {}: {e} (run `init` first?)", path.display()))?;
    Wallet::from_mnemonic(words).map_err(|e| format!("mnemonic file {}: {e}", path.display()))
}

#[cfg(feature = "lnd")]
async fn backend(config: &Config) -> Result<Box<dyn LightningBackend>, String> {
    let lnd = config
        .lnd
        .as_ref()
        .ok_or("no Lightning node configured (add an [lnd] section)")?;
    let node = bipnip_ln::lnd::LndBackend::connect(&lnd.address, &lnd.tls_cert, &lnd.macaroon)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Box::new(node))
}

#[cfg(not(feature = "lnd"))]
async fn backend(config: &Config) -> Result<Box<dyn LightningBackend>, String> {
    match config.lnd {
        Some(_) => Err("this build has no LND support (build with --features lnd)".into()),
        None => Err("no Lightning node configured (add an [lnd] section)".into()),
    }
}

fn npub(keys: &nostr_sdk::prelude::Keys) -> String {
    keys.public_key()
        .to_bech32()
        .unwrap_or_else(|_| keys.public_key().to_hex())
}

fn init(config: &Config) -> Result<(), String> {
    secretfile::create_private_dir(&config.data_dir)
        .map_err(|e| format!("{}: {e}", config.data_dir.display()))?;
    let path = config.mnemonic_path();
    let wallet = Wallet::generate().map_err(|e| e.to_string())?;
    secretfile::create_secret(&path, wallet.expose_mnemonic())
        .map_err(|e| format!("mnemonic file {}: {e}", path.display()))?;
    println!("created {}", path.display());
    println!(
        "Back this file up offline: it is the only copy of your keys, and it also restores your \
         identity in any Mostro client."
    );
    whoami_keys(&wallet)
}

fn whoami_keys(wallet: &Wallet) -> Result<(), String> {
    let identity = wallet.identity().map_err(|e| e.to_string())?;
    let liquidity = wallet.liquidity().map_err(|e| e.to_string())?;
    println!("fiat identity (Mostro):   {}", npub(&identity));
    println!("liquidity identity:       {}", npub(&liquidity));
    Ok(())
}

async fn whoami(config: &Config) -> Result<(), String> {
    whoami_keys(&load_wallet(config)?)?;
    if config.lnd.is_some() {
        let info = backend(config)
            .await?
            .node_info()
            .await
            .map_err(|e| e.to_string())?;
        println!(
            "node:                     {} ({}, height {}, {})",
            info.node_pubkey,
            info.network,
            info.block_height,
            if info.synced { "synced" } else { "not synced" }
        );
    }
    Ok(())
}

async fn bind(config: &Config, revoke: bool, publish: bool) -> Result<(), String> {
    let wallet = load_wallet(config)?;
    let keys = wallet.liquidity().map_err(|e| e.to_string())?;
    let node = backend(config).await?;
    let action = if revoke { Action::Revoke } else { Action::Bind };
    let event = make_binding_event(node.as_ref(), &keys, action, config.network, now())
        .await
        .map_err(|e| e.to_string())?;
    println!("{}", event.as_json());
    if publish {
        let report = relay::publish(&event, &config.relays).await?;
        for url in &report.accepted {
            eprintln!("accepted by {url}");
        }
        for (url, why) in &report.failed {
            eprintln!("NOT accepted by {url}: {why}");
        }
    }
    Ok(())
}

async fn verify_binding(config: &Config, node_pubkey: &str) -> Result<(), String> {
    let node = secp256k1::PublicKey::from_slice(
        &bipnip_ln::hexutil::node_key(node_pubkey).map_err(|e| e.to_string())?,
    )
    .map_err(|_| "not a valid node public key".to_string())?;
    let statements = relay::fetch_statements(node_pubkey, &config.relays).await?;
    match latest_for_node(&node, statements.iter(), now()) {
        Some(s) if s.statement.action == Action::Bind && s.statement.network == config.network => {
            println!(
                "bound: node {node_pubkey} -> nostr {} on {} since {}",
                s.statement.nostr_pubkey,
                s.statement.network.as_str(),
                s.statement.created_at
            );
            Ok(())
        }
        Some(s) if s.statement.action == Action::Revoke => {
            println!("revoked at {}", s.statement.created_at);
            Err("the node's latest statement is a revocation".into())
        }
        Some(s) => Err(format!(
            "the node's binding is for {}, not {}",
            s.statement.network.as_str(),
            config.network.as_str()
        )),
        None => Err("no valid binding found on the configured relays".into()),
    }
}

/// Command-line options of the form `--name value`, plus the switches named in `switches` and
/// any other words in order.
struct Options {
    values: HashMap<String, String>,
    switches: Vec<String>,
    words: Vec<String>,
}

impl Options {
    fn parse(args: &[String], switches: &[&str]) -> Result<Self, String> {
        let mut options = Options {
            values: HashMap::new(),
            switches: Vec::new(),
            words: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.strip_prefix("--") {
                None => options.words.push(arg.clone()),
                Some(name) if switches.contains(&name) => options.switches.push(name.to_string()),
                Some(name) => {
                    let value = args.next().ok_or(format!("--{name} needs a value"))?;
                    if options
                        .values
                        .insert(name.to_string(), value.clone())
                        .is_some()
                    {
                        return Err(format!("--{name} is given twice"));
                    }
                }
            }
        }
        Ok(options)
    }

    /// Takes the value of `--name`, if given.
    fn take<T: FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        match self.values.remove(name) {
            None => Ok(None),
            Some(text) => text
                .parse()
                .map(Some)
                .map_err(|_| format!("--{name}: {text:?} is not a valid value")),
        }
    }

    fn required<T: FromStr>(&mut self, name: &str) -> Result<T, String> {
        self.take(name)?.ok_or(format!("--{name} is required"))
    }

    fn or<T: FromStr>(&mut self, name: &str, default: T) -> Result<T, String> {
        Ok(self.take(name)?.unwrap_or(default))
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    /// Refuses options nobody took, so a typo is an error and not a silent default.
    fn done(self) -> Result<Vec<String>, String> {
        match self.values.keys().next() {
            Some(name) => Err(format!("unknown option --{name}")),
            None => Ok(self.words),
        }
    }
}

fn say(line: String) {
    println!("{line}");
}

async fn lease(config: &Config, args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        return Err(usage());
    };
    if command == "list" && rest.is_empty() {
        for line in cmd::list(config)? {
            println!("{line}");
        }
        return Ok(());
    }
    let mut options = Options::parse(rest, &["private"])?;
    let poll = Duration::from_secs(options.or("poll", 5u64)?.max(1));
    let wallet = load_wallet(config)?;
    let node = backend(config).await?;
    let session = Session {
        config,
        wallet: &wallet,
        backend: node.as_ref(),
    };
    match command.as_str() {
        "offer" => {
            let params = OfferParams {
                min_capacity_sat: options.required("min-capacity")?,
                max_capacity_sat: options.required("max-capacity")?,
                duration_blocks: options.required("duration")?,
                fee_base_sat: options.required("fee-base")?,
                fee_ppm: options.required("fee-ppm")?,
                // All upfront by default: the command then ends when the lease is active.
                upfront_percent: options.or("upfront", 100u8)?,
                min_confirmations: options.or("confs", 3u32)?,
                announce: !options.switch("private"),
                expires_in_secs: options.or("expires-in", 86_400u64)?,
            };
            let seller = SellerConfig {
                cltv_delta: options.or("cltv-delta", DEFAULT_SELLER_CLTV_DELTA)?,
                feerate_sat_vb: options.required("feerate")?,
            };
            if !options.done()?.is_empty() {
                return Err(usage());
            }
            if params.upfront_percent < 100 {
                eprintln!(
                    "note: part of the fee is due at the end of the lease; keep this running, or \
                     run `lease resume` later, so it can be billed then"
                );
            }
            let (mut run, mut link) = cmd::offer(&session, params, seller).await?;
            let result = cmd::run_seller(&session, &mut run, &mut link, poll, &mut say).await;
            link.close().await;
            result
        }
        "take" => {
            let capacity_sat = options.required("capacity")?;
            let node_address = options.take("node-address")?;
            let buyer = BuyerConfig {
                max_fee_msat: options.or("max-fee-msat", 10_000u64)?,
                max_cltv_delta: options.or("max-cltv-delta", DEFAULT_MAX_CLTV_DELTA)?,
            };
            let words = options.done()?;
            let [order_id] = words.as_slice() else {
                return Err(usage());
            };
            let params = TakeParams {
                order_id: order_id
                    .parse()
                    .map_err(|_| format!("{order_id:?} is not an order id"))?,
                capacity_sat,
                node_address,
            };
            let (mut run, mut link) = cmd::take(&session, params, buyer).await?;
            let result = cmd::run_buyer(&session, &mut run, &mut link, poll, &mut say).await;
            link.close().await;
            result
        }
        "resume" => {
            if !options.done()?.is_empty() {
                return Err(usage());
            }
            cmd::resume(&session, poll, &mut say).await
        }
        _ => Err(usage()),
    }
}

async fn ring(config: &Config, args: &[String]) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        return Err(usage());
    };
    if command == "list" && rest.is_empty() {
        for line in ring_cmd::list(config)? {
            println!("{line}");
        }
        return Ok(());
    }
    let mut options = Options::parse(rest, &["private"])?;
    let poll = Duration::from_secs(options.or("poll", 5u64)?.max(1));
    let wallet = load_wallet(config)?;
    let node = backend(config).await?;
    let session = Session {
        config,
        wallet: &wallet,
        backend: node.as_ref(),
    };
    match command.as_str() {
        "create" => {
            let params = CreateParams {
                capacity_sat: options.required("capacity")?,
                slots: options.required("slots")?,
                announce: !options.switch("private"),
                // Two days to open, as LN+ rings give; a day to fill.
                open_window_secs: options.or("window", 172_800u64)?,
                bond_sat: options.or("bond", 0u64)?,
                expires_in_secs: options.or("expires-in", 86_400u64)?,
                node_address: options.take("node-address")?,
                feerate_sat_vb: options.required("feerate")?,
            };
            if !options.done()?.is_empty() {
                return Err(usage());
            }
            let (mut ring, mut link) = ring_cmd::create(&session, params).await?;
            let result = ring_cmd::run(&session, &mut ring, &mut link, poll, &mut say).await;
            link.close().await;
            result
        }
        "join" => {
            let node_address = options.take("node-address")?;
            let feerate_sat_vb = options.required("feerate")?;
            let words = options.done()?;
            let [order_id] = words.as_slice() else {
                return Err(usage());
            };
            let params = JoinParams {
                order_id: order_id
                    .parse()
                    .map_err(|_| format!("{order_id:?} is not an order id"))?,
                node_address,
                feerate_sat_vb,
            };
            let (mut ring, mut link) = ring_cmd::join(&session, params).await?;
            let result = ring_cmd::run(&session, &mut ring, &mut link, poll, &mut say).await;
            link.close().await;
            result
        }
        "resume" => {
            if !options.done()?.is_empty() {
                return Err(usage());
            }
            ring_cmd::resume(&session, poll, &mut say).await
        }
        _ => Err(usage()),
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (config_path, rest) = match args.as_slice() {
        [flag, path, rest @ ..] if flag == "--config" => (PathBuf::from(path), rest),
        _ => return Err(usage()),
    };
    let config = load_config(&config_path)?;
    match rest {
        [cmd] if cmd == "init" => init(&config),
        [cmd] if cmd == "whoami" => whoami(&config).await,
        [cmd, flags @ ..] if cmd == "bind" => {
            if flags.iter().any(|f| f != "--revoke" && f != "--publish") {
                return Err(usage());
            }
            let has = |name: &str| flags.iter().any(|f| f == name);
            bind(&config, has("--revoke"), has("--publish")).await
        }
        [cmd, node] if cmd == "verify-binding" => verify_binding(&config, node).await,
        [cmd, rest @ ..] if cmd == "lease" => lease(&config, rest).await,
        [cmd, rest @ ..] if cmd == "ring" => ring(&config, rest).await,
        _ => Err(usage()),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("bipnip-agent: {e}");
        std::process::exit(1);
    }
}
