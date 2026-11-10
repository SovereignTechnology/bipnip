//! An ldk-node 0.7.1 node on regtest for the M0 spike (driven by `run.sh`):
//! 1. writes its node id and a `sign_message` vector,
//! 2. waits for an inbound channel from the trusted LND peer,
//! 3. creates a hold invoice on a payment hash it is given (`receive_for_hash`),
//! 4. on `PaymentClaimable` for that hash, claims it with the preimage it is given
//!    (`claim_for_hash`), and exits 0 once `PaymentReceived` arrives.
//!
//! Usage: ldk-spike <dir> <bitcoind host:port> <rpc user> <rpc pass> <listen host:port>
//!                  <trusted peer pubkey> <payment hash hex> <preimage hex> <message>

use std::str::FromStr;
use std::time::{Duration, Instant};

use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::bitcoin::Network;
use ldk_node::config::{AnchorChannelsConfig, Config};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
use ldk_node::lightning_types::payment::{PaymentHash, PaymentPreimage};
use ldk_node::{Builder, Event};

fn hex32(s: &str) -> [u8; 32] {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect();
    bytes.try_into().expect("32 bytes")
}

fn write(path: &str, text: &str) {
    std::fs::write(path, text).expect("write");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert_eq!(a.len(), 10, "usage: see the file header");
    let (dir, rpc, user, pass, listen) = (&a[1], &a[2], &a[3], &a[4], &a[5]);
    let trusted = PublicKey::from_str(&a[6]).expect("trusted pubkey");
    let payment_hash = PaymentHash(hex32(&a[7]));
    let preimage = PaymentPreimage(hex32(&a[8]));
    assert_eq!(
        sha256::Hash::hash(&preimage.0).to_byte_array(),
        payment_hash.0,
        "preimage does not match hash"
    );
    let message = &a[9];

    let mut config = Config::default();
    config.anchor_channels_config = Some(AnchorChannelsConfig {
        trusted_peers_no_reserve: vec![trusted],
        ..AnchorChannelsConfig::default()
    });
    let mut builder = Builder::from_config(config);
    builder.set_network(Network::Regtest);
    builder.set_storage_dir_path(format!("{dir}/data"));
    let (host, port) = rpc.rsplit_once(':').expect("host:port");
    builder.set_chain_source_bitcoind_rpc(
        host.to_string(),
        port.parse().expect("port"),
        user.clone(),
        pass.clone(),
    );
    builder
        .set_listening_addresses(vec![SocketAddress::from_str(listen).expect("listen addr")])
        .expect("listening addresses");
    let node = builder.build().expect("build node");
    node.start().expect("start node");

    let node_id = node.node_id().to_string();
    let signature = node.sign_message(message.as_bytes());
    write(&format!("{dir}/node_id"), &node_id);
    write(
        &format!("{dir}/vector.json"),
        &serde_json::json!({"impl": "ldk-node-0.7.1", "msg": message, "sig": signature, "pubkey": node_id})
            .to_string(),
    );
    eprintln!("ldk-spike: node {node_id} up, waiting for a channel");

    let deadline = Instant::now() + Duration::from_secs(900);
    let mut invoice_written = false;
    loop {
        assert!(Instant::now() < deadline, "ldk-spike: timed out");
        let event = match node.next_event() {
            Some(e) => e,
            None => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        eprintln!("ldk-spike: event {event:?}");
        match event {
            Event::ChannelReady { .. } if !invoice_written => {
                let description = Bolt11InvoiceDescription::Direct(
                    Description::new("bipnip spike: hold invoice on an external hash".into())
                        .expect("description"),
                );
                let invoice = node
                    .bolt11_payment()
                    .receive_for_hash(2_000_000, &description, 3600, payment_hash)
                    .expect("receive_for_hash");
                write(&format!("{dir}/invoice"), &invoice.to_string());
                invoice_written = true;
            }
            Event::PaymentClaimable { payment_hash: h, claimable_amount_msat, .. } => {
                assert_eq!(h, payment_hash, "claimable payment for another hash");
                write(&format!("{dir}/claimable"), &claimable_amount_msat.to_string());
                node.bolt11_payment()
                    .claim_for_hash(payment_hash, claimable_amount_msat, preimage)
                    .expect("claim_for_hash");
            }
            Event::PaymentReceived { payment_hash: h, amount_msat, .. } => {
                assert_eq!(h, payment_hash, "received payment for another hash");
                write(&format!("{dir}/received"), &amount_msat.to_string());
                node.event_handled().expect("event handled");
                break;
            }
            _ => {}
        }
        node.event_handled().expect("event handled");
    }
    node.stop().expect("stop");
    eprintln!("ldk-spike: hold invoice on an external hash claimed and received");
}
