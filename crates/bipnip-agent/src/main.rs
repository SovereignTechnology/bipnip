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
//! ```
//!
//! Nothing here prints the mnemonic or any credential.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bipnip_agent::binding::make_binding_event;
use bipnip_agent::config::Config;
use bipnip_agent::keys::Wallet;
use bipnip_agent::{relay, secretfile};
use bipnip_ln::LightningBackend;
use mostro_core::node_binding::{Action, latest_for_node};
use nostr_sdk::prelude::ToBech32;

fn usage() -> String {
    "usage: bipnip-agent --config FILE <init | whoami | bind [--revoke] [--publish] | verify-binding NODE_PUBKEY>"
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
