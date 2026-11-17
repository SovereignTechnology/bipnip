//! `ldk-probe`: one ldk-server call per invocation, printed as it came back. `run.sh` strings
//! the calls into scenarios against a real LND node and reads the answers. Nothing is asserted
//! here: the point is to see what the server says.
//!
//! Environment: `LDK_ADDR` (host:port of the gRPC service), `LDK_CERT` (its tls.crt), `LDK_MAC`
//! (the file holding its hex admin macaroon). Regtest throwaway values only.

use std::time::Duration;

use ldk_server_client::client::LdkServerClient;
use ldk_server_client::ldk_server_grpc::api::open_channel_request::Amount;
use ldk_server_client::ldk_server_grpc::api::{
    Bolt11ClaimForIdRequest, Bolt11FailForIdRequest, Bolt11ReceiveForHashRequest,
    Bolt11ReceiveRequest, Bolt11SendRequest, ConnectPeerRequest, DecodeInvoiceRequest,
    GetBalancesRequest, GetNodeInfoRequest, GetPaymentDetailsRequest, GraphGetNodeRequest,
    ListChannelsRequest, ListPaymentsRequest, OnchainReceiveRequest, OpenChannelRequest,
    SignMessageRequest,
};
use ldk_server_client::ldk_server_grpc::types::bolt11_invoice_description::Kind;
use ldk_server_client::ldk_server_grpc::types::{Bolt11InvoiceDescription, RouteParametersConfig};
use ldk_server_client::{
    DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF, DEFAULT_MAX_PATH_COUNT,
    DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA,
};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("missing env {k}"))
}

fn description(text: &str) -> Option<Bolt11InvoiceDescription> {
    Some(Bolt11InvoiceDescription {
        kind: Some(Kind::Direct(text.to_string())),
    })
}

/// Prints an answer, or the error with its code, on one line each way.
macro_rules! show {
    ($what:expr, $call:expr) => {
        match $call.await {
            Ok(answer) => println!("OK {} {:?}", $what, answer),
            Err(e) => println!(
                "ERR {} code={:?} message={:?}",
                $what, e.error_code, e.message
            ),
        }
    };
}

#[tokio::main]
async fn main() {
    let cert = std::fs::read(env("LDK_CERT")).expect("tls.crt");
    let macaroon = std::fs::read_to_string(env("LDK_MAC")).expect("macaroon");
    let c =
        LdkServerClient::new(env("LDK_ADDR"), macaroon.trim().to_string(), &cert).expect("client");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| {
        args.get(i)
            .cloned()
            .unwrap_or_else(|| panic!("missing argument {i}"))
    };
    match args.first().map(String::as_str) {
        Some("info") => show!("info", c.get_node_info(GetNodeInfoRequest {})),
        // Just the node id, for scripts.
        Some("id") => {
            let info = c.get_node_info(GetNodeInfoRequest {}).await.expect("info");
            println!("{}", info.node_id);
        }
        Some("balances") => show!("balances", c.get_balances(GetBalancesRequest {})),
        Some("spendable") => {
            let b = c
                .get_balances(GetBalancesRequest {})
                .await
                .expect("balances");
            println!("{}", b.spendable_onchain_balance_sats);
        }
        Some("address") => {
            let a = c
                .onchain_receive(OnchainReceiveRequest {})
                .await
                .expect("address");
            println!("{}", a.address);
        }
        Some("sign") => show!(
            "sign",
            c.sign_message(SignMessageRequest {
                message: arg(1).into_bytes().into(),
            })
        ),
        Some("hold") => {
            let request = Bolt11ReceiveForHashRequest {
                amount_msat: Some(arg(2).parse().expect("msat")),
                description: description("bipnip ldk probe: hold"),
                expiry_secs: 3600,
                payment_hash: arg(1),
            };
            match c.bolt11_receive_for_hash(request).await {
                Ok(answer) => {
                    println!("INVOICE {}", answer.invoice);
                    show!(
                        "decode",
                        c.decode_invoice(DecodeInvoiceRequest {
                            invoice: answer.invoice.clone(),
                        })
                    );
                }
                Err(e) => println!("ERR hold code={:?} message={:?}", e.error_code, e.message),
            }
        }
        Some("invoice") => show!(
            "invoice",
            c.bolt11_receive(Bolt11ReceiveRequest {
                amount_msat: Some(arg(1).parse().expect("msat")),
                description: description("bipnip ldk probe: plain"),
                expiry_secs: 3600,
            })
        ),
        Some("details") => show!(
            "details",
            c.get_payment_details(GetPaymentDetailsRequest { payment_id: arg(1) })
        ),
        Some("payments") => show!("payments", c.list_payments(ListPaymentsRequest::default())),
        // Every payment record that carries this payment hash, through every page of the list.
        Some("find") => {
            let hash = arg(1);
            let (mut page_token, mut pages, mut seen) = (None, 0u32, 0usize);
            loop {
                let request = ListPaymentsRequest { page_token };
                let page = match c.list_payments(request).await {
                    Ok(page) => page,
                    Err(e) => {
                        println!("ERR find code={:?} message={:?}", e.error_code, e.message);
                        break;
                    }
                };
                pages += 1;
                seen += page.payments.len();
                for payment in &page.payments {
                    if format!("{payment:?}").contains(&format!("hash: \"{hash}\"")) {
                        println!(
                            "FOUND id={} direction={} status={} amount_msat={:?}",
                            payment.payment_id,
                            payment.direction,
                            payment.status,
                            payment.amount_msat
                        );
                    }
                }
                page_token = page.next_page_token;
                if page_token.is_none() {
                    break;
                }
            }
            println!("SEARCHED {seen} payments in {pages} pages");
        }
        Some("claim") => show!(
            "claim",
            c.bolt11_claim_for_id(Bolt11ClaimForIdRequest {
                payment_id: arg(1),
                claimable_amount_msat: args.get(3).map(|a| a.parse().expect("msat")),
                preimage: arg(2),
            })
        ),
        Some("fail") => show!(
            "fail",
            c.bolt11_fail_for_id(Bolt11FailForIdRequest { payment_id: arg(1) })
        ),
        Some("pay") => show!(
            "pay",
            c.bolt11_send(Bolt11SendRequest {
                invoice: arg(1),
                amount_msat: None,
                route_parameters: Some(RouteParametersConfig {
                    max_total_routing_fee_msat: Some(arg(2).parse().expect("msat")),
                    max_total_cltv_expiry_delta: DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA,
                    max_path_count: DEFAULT_MAX_PATH_COUNT,
                    max_channel_saturation_power_of_half:
                        DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF,
                }),
            })
        ),
        Some("connect") => show!(
            "connect",
            c.connect_peer(ConnectPeerRequest {
                node_pubkey: arg(1),
                address: arg(2),
                persist: false,
            })
        ),
        Some("open") => show!(
            "open",
            c.open_channel(OpenChannelRequest {
                node_pubkey: arg(1),
                address: arg(2),
                push_to_counterparty_msat: None,
                channel_config: None,
                announce_channel: arg(4) == "announced",
                disable_counterparty_reserve: false,
                amount: Some(Amount::ChannelAmountSats(arg(3).parse().expect("sats"))),
            })
        ),
        Some("channels") => match c.list_channels(ListChannelsRequest {}).await {
            Ok(list) => {
                println!("CHANNELS {}", list.channels.len());
                for ch in list.channels {
                    println!(
                        "CHANNEL peer={} funding={:?} value={} outbound={} announced={} \
                         confirmations={:?} required={:?} ready={} usable={} user_id={} scid={:?}",
                        ch.counterparty_node_id,
                        ch.funding_txo.map(|o| format!("{}:{}", o.txid, o.vout)),
                        ch.channel_value_sats,
                        ch.is_outbound,
                        ch.is_announced,
                        ch.confirmations,
                        ch.confirmations_required,
                        ch.is_channel_ready,
                        ch.is_usable,
                        ch.user_channel_id,
                        ch.short_channel_id,
                    );
                }
            }
            Err(e) => println!(
                "ERR channels code={:?} message={:?}",
                e.error_code, e.message
            ),
        },
        Some("graph") => show!(
            "graph",
            c.graph_get_node(GraphGetNodeRequest { node_id: arg(1) })
        ),
        // Prints every event the server sends, one per line, until killed.
        Some("events") => loop {
            match c.subscribe_events().await {
                Ok(mut stream) => {
                    println!("SUBSCRIBED");
                    while let Some(message) = stream.next_message().await {
                        match message {
                            Ok(event) => println!("EVENT {event:?}"),
                            Err(e) => {
                                println!(
                                    "ERR event code={:?} message={:?}",
                                    e.error_code, e.message
                                );
                                break;
                            }
                        }
                    }
                    println!("STREAM ENDED");
                }
                Err(e) => println!(
                    "ERR subscribe code={:?} message={:?}",
                    e.error_code, e.message
                ),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        },
        _ => panic!("unknown command {args:?}"),
    }
}
