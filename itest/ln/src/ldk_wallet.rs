//! What the harness needs from an LDK Server that the backend trait has no place for: an
//! on-chain address to fund, the spendable balance, and the node id.
//!
//! `ldk-wallet <address | spendable | id | channels | open NODE HOST:PORT SAT>`, with `LDK_ADDR`
//! (host:port), `LDK_CERT` (tls.crt) and
//! `LDK_MAC` (the hex macaroon file). Regtest throwaway values only.

use ldk_server_client::client::LdkServerClient;
use ldk_server_client::ldk_server_grpc::api::open_channel_request::Amount;
use ldk_server_client::ldk_server_grpc::api::{
    GetBalancesRequest, GetNodeInfoRequest, ListChannelsRequest, OnchainReceiveRequest,
    OpenChannelRequest,
};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("missing env {k}"))
}

#[tokio::main]
async fn main() {
    let cert = std::fs::read(env("LDK_CERT")).expect("tls.crt");
    let macaroon = std::fs::read_to_string(env("LDK_MAC")).expect("macaroon");
    let client =
        LdkServerClient::new(env("LDK_ADDR"), macaroon.trim().to_string(), &cert).expect("client");
    match std::env::args().nth(1).as_deref() {
        Some("address") => {
            let answer = client
                .onchain_receive(OnchainReceiveRequest {})
                .await
                .expect("address");
            println!("{}", answer.address);
        }
        Some("spendable") => {
            let answer = client
                .get_balances(GetBalancesRequest {})
                .await
                .expect("balances");
            println!("{}", answer.spendable_onchain_balance_sats);
        }
        Some("id") => {
            let answer = client
                .get_node_info(GetNodeInfoRequest {})
                .await
                .expect("info");
            println!("{}", answer.node_id);
        }
        // One line per channel: capacity in satoshi, who opened it, whether it can be used.
        Some("channels") => {
            let answer = client
                .list_channels(ListChannelsRequest {})
                .await
                .expect("channels");
            for channel in answer.channels {
                println!(
                    "{} {} {}",
                    channel.channel_value_sats,
                    if channel.is_outbound { "out" } else { "in" },
                    if channel.is_usable {
                        "usable"
                    } else {
                        "not-usable"
                    },
                );
            }
        }
        // A public channel to NODE at HOST:PORT of SAT, for the soak's payment mesh.
        Some("open") => {
            let arg = |n: usize| std::env::args().nth(n).expect("open NODE HOST:PORT SAT");
            let request = OpenChannelRequest {
                node_pubkey: arg(2),
                address: arg(3),
                push_to_counterparty_msat: None,
                channel_config: None,
                announce_channel: true,
                disable_counterparty_reserve: false,
                amount: Some(Amount::ChannelAmountSats(arg(4).parse().expect("SAT"))),
            };
            let opened = client.open_channel(request).await.expect("open");
            println!("{}", opened.user_channel_id);
        }
        other => panic!("ldk-wallet <address | spendable | id | channels | open>, not {other:?}"),
    }
}
