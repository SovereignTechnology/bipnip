//! The LDK backend: an [LDK Server](https://github.com/lightningdevkit/ldk-server) next to the
//! agent, over its gRPC API (`ldk-server-client`). The agent stays beside the node, as with LND;
//! it does not embed one.
//!
//! What LDK Server gives and what it does not, as measured against a real one
//! (`spikes/ldk-m3/RESULTS.md`):
//!
//! - **Paying, channels, signing** map onto the trait directly. An outbound payment is kept
//!   under its payment hash, so its state is a lookup.
//! - **Hold invoices are not supported**, so an LDK node can buy a lease or sit in a ring but
//!   cannot sell a lease. The server can make an invoice on a caller's hash and claim it with
//!   the preimage, but it cannot yet do it safely for a seller:
//!   - the lock time cannot be set: its hold invoices ask for 42 blocks, and the node gives a
//!     held payment back about 39 blocks before it expires, which left 6 blocks when measured:
//!     not enough to open a channel, see it confirmed and settle;
//!   - whether a payment is held can only be learned from an event stream that is best-effort
//!     and never replayed. The payment list does show a held payment, but shows it the same
//!     way after the node has given it back, so it cannot answer "is it still held?";
//!   - an unpaid hold invoice cannot be cancelled.
//!
//!   A seller that opened a channel on such an answer could find the payment gone. Rather
//!   than offer that, the hold methods answer [`BackendError::Unsupported`]. Two additions to
//!   LDK Server would lift this: a lock time on `Bolt11ReceiveForHash`, and a lookup of a held
//!   payment by its hash that says until which block it can be claimed.
//! - **`graph_node`** counts a node's public channels; their capacity is known only where the
//!   server's graph has it, or the channel is this node's own. Read the capacity as a floor.
//! - **`graph_channels` is not supported**: the server's graph names channels by short channel
//!   id, never by funding outpoint. Only an instance needs it.
//! - **Channel opens** take the node's own fee estimate; the request's fee rate is not used.
//!   The peer's address is required.
//!
//! Every RPC has a deadline. Errors carry the server's own text only; the macaroon and
//! invoices are never put in one.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::{LdkServerError, LdkServerErrorCode};
use ldk_server_client::ldk_server_grpc::api::open_channel_request::Amount;
use ldk_server_client::ldk_server_grpc::api::{
    Bolt11ReceiveRequest, Bolt11SendRequest, ConnectPeerRequest, DecodeInvoiceRequest,
    GetNodeInfoRequest, GetPaymentDetailsRequest, GraphGetChannelRequest, GraphGetNodeRequest,
    ListChannelsRequest, ListPaymentsRequest, OpenChannelRequest, SignMessageRequest,
};
use ldk_server_client::ldk_server_grpc::types::bolt11_invoice_description::Kind as DescriptionKind;
use ldk_server_client::ldk_server_grpc::types::payment_kind::Kind as PaymentKindOf;
use ldk_server_client::ldk_server_grpc::types::{
    Bolt11InvoiceDescription, Network, Payment, PaymentDirection, PaymentStatus,
    RouteParametersConfig,
};
use ldk_server_client::{
    DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF, DEFAULT_MAX_PATH_COUNT,
    DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA,
};

use crate::hexutil::{hash32, node_key};
use crate::{
    BackendError, ChannelView, ClosedChannel, GraphChannel, HoldInvoiceRequest, HoldState,
    InvoiceStatus, LightningBackend, NodeInfo, OpenRequest, PaymentState,
};

/// Deadline for one RPC.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for a new channel's funding outpoint to show in the channel list.
const FUNDING_WAIT: Duration = Duration::from_secs(60);
/// How often a payment or a new channel is looked at while waiting for it.
const POLL: Duration = Duration::from_millis(500);
/// Most pages of the payment list read when looking for an inbound payment.
const MAX_PAYMENT_PAGES: usize = 50;

fn server_error(e: LdkServerError) -> BackendError {
    match e.error_code {
        // The server itself failed, or it could not be reached: the call may be tried again.
        LdkServerErrorCode::InternalServerError | LdkServerErrorCode::InternalError => {
            BackendError::Unavailable(e.message)
        }
        _ => BackendError::Rejected(e.message),
    }
}

async fn call<T, F>(fut: F) -> Result<T, BackendError>
where
    F: Future<Output = Result<T, LdkServerError>>,
{
    match tokio::time::timeout(RPC_TIMEOUT, fut).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(e)) => Err(server_error(e)),
        Err(_) => Err(BackendError::Unavailable(
            "LDK Server did not answer in time".into(),
        )),
    }
}

fn bolt11_preimage(payment: &Payment) -> Option<&str> {
    match payment.kind.as_ref()?.kind.as_ref()? {
        PaymentKindOf::Bolt11(bolt11) => bolt11.preimage.as_deref(),
        _ => None,
    }
}

fn bolt11_hash(payment: &Payment) -> Option<&str> {
    match payment.kind.as_ref()?.kind.as_ref()? {
        PaymentKindOf::Bolt11(bolt11) => Some(&bolt11.hash),
        _ => None,
    }
}

/// Unix seconds now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn is_status(payment: &Payment, status: PaymentStatus) -> bool {
    payment.status == status as i32
}

/// Why this backend holds no payments (the module's text has the measurements).
const NO_HOLD: &str = "LDK Server cannot yet hold a payment safely for a seller: its lock time \
                       is fixed and short, and a held payment cannot be looked up";

/// An LDK Server reached over gRPC.
pub struct LdkBackend {
    client: LdkServerClient,
}

impl LdkBackend {
    /// A client for the LDK Server at `address` (`host:port`, no scheme), with the server's TLS
    /// certificate (`<storage_dir>/tls.crt`) and a macaroon file (hex, as the server writes
    /// it). Nothing is sent yet: a wrong address shows up on the first call.
    pub fn connect(address: &str, tls_cert: &Path, macaroon: &Path) -> Result<Self, BackendError> {
        let setup = |what: &str| BackendError::Unavailable(format!("LDK Server setup: {what}"));
        let cert =
            std::fs::read(tls_cert).map_err(|_| setup("the TLS certificate cannot be read"))?;
        let macaroon =
            std::fs::read_to_string(macaroon).map_err(|_| setup("the macaroon cannot be read"))?;
        // The client's own errors here name the certificate or the macaroon's form, not its
        // contents; they are still not passed on.
        let client = LdkServerClient::new(address.to_string(), macaroon.trim().to_string(), &cert)
            .map_err(|_| setup("the certificate or the macaroon is not usable"))?;
        Ok(LdkBackend { client })
    }

    async fn payment(&self, payment_id: &str) -> Result<Option<Payment>, BackendError> {
        let request = GetPaymentDetailsRequest {
            payment_id: payment_id.to_string(),
        };
        Ok(call(self.client.get_payment_details(request))
            .await?
            .payment)
    }

    /// The inbound payment for `payment_hash`, if the server has a record of one: looked up by
    /// the hash, which is the payment id of an invoice the server made, and else found in the
    /// payment list.
    async fn inbound(&self, payment_hash: &str) -> Result<Option<Payment>, BackendError> {
        let inbound = |payment: &Payment| payment.direction == PaymentDirection::Inbound as i32;
        if let Some(payment) = self.payment(payment_hash).await? {
            if inbound(&payment) {
                return Ok(Some(payment));
            }
        }
        let mut page_token = None;
        for _ in 0..MAX_PAYMENT_PAGES {
            let page = call(
                self.client
                    .list_payments(ListPaymentsRequest { page_token }),
            )
            .await?;
            if let Some(found) = page
                .payments
                .into_iter()
                .find(|payment| inbound(payment) && bolt11_hash(payment) == Some(payment_hash))
            {
                return Ok(Some(found));
            }
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => return Ok(None),
            }
        }
        Err(BackendError::Unavailable(
            "LDK Server's payment list is longer than this backend reads".into(),
        ))
    }

    /// The state of an outbound payment record.
    fn outbound_state(payment: &Payment) -> PaymentState {
        if is_status(payment, PaymentStatus::Succeeded) {
            match bolt11_preimage(payment) {
                Some(preimage) => PaymentState::Succeeded {
                    preimage: preimage.to_string(),
                },
                // Paid, and the record does not carry the preimage yet: not done for a caller
                // that needs it.
                None => PaymentState::InFlight,
            }
        } else if is_status(payment, PaymentStatus::Failed) {
            PaymentState::Failed {
                reason: "the payment failed".into(),
            }
        } else {
            PaymentState::InFlight
        }
    }
}

#[async_trait]
impl LightningBackend for LdkBackend {
    async fn node_info(&self) -> Result<NodeInfo, BackendError> {
        let info = call(self.client.get_node_info(GetNodeInfoRequest {})).await?;
        let network = match Network::from_i32(info.network) {
            Some(Network::Bitcoin) => "bitcoin",
            Some(Network::Testnet) => "testnet",
            Some(Network::Testnet4) => "testnet4",
            Some(Network::Signet) => "signet",
            Some(Network::Regtest) => "regtest",
            None => {
                return Err(BackendError::Rejected(
                    "LDK Server reported a network this backend does not know".into(),
                ));
            }
        };
        Ok(NodeInfo {
            node_pubkey: info.node_id,
            network: network.to_string(),
            block_height: info.current_best_block.map_or(0, |best| best.height),
            // The server has no "synced" flag: it has synced its wallets at least once.
            synced: info.latest_lightning_wallet_sync_timestamp.is_some()
                && info.latest_onchain_wallet_sync_timestamp.is_some(),
        })
    }

    async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
        let request = SignMessageRequest {
            message: msg.to_vec().into(),
        };
        Ok(call(self.client.sign_message(request)).await?.signature)
    }

    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError> {
        hash32(&req.payment_hash, "payment hash")?;
        Err(BackendError::Unsupported(NO_HOLD))
    }

    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError> {
        hash32(payment_hash, "payment hash")?;
        Err(BackendError::Unsupported(NO_HOLD))
    }

    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        hash32(preimage, "preimage")?;
        Err(BackendError::Unsupported(NO_HOLD))
    }

    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError> {
        hash32(payment_hash, "payment hash")?;
        Err(BackendError::Unsupported(NO_HOLD))
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        max_fee_msat: u64,
        wait_secs: u32,
    ) -> Result<PaymentState, BackendError> {
        let decoded = call(self.client.decode_invoice(DecodeInvoiceRequest {
            invoice: bolt11.to_string(),
        }))
        .await?;
        // An outbound payment is kept under its payment hash.
        let payment_hash = decoded.payment_hash;
        let request = Bolt11SendRequest {
            invoice: bolt11.to_string(),
            amount_msat: None,
            route_parameters: Some(RouteParametersConfig {
                max_total_routing_fee_msat: Some(max_fee_msat),
                max_total_cltv_expiry_delta: DEFAULT_MAX_TOTAL_CLTV_EXPIRY_DELTA,
                max_path_count: DEFAULT_MAX_PATH_COUNT,
                max_channel_saturation_power_of_half: DEFAULT_MAX_CHANNEL_SATURATION_POWER_OF_HALF,
            }),
        };
        if let Err(refusal) = call(self.client.bolt11_send(request)).await {
            // The node refuses a second send for a hash it is paying or has paid. By the
            // trait's rule that is not an error: the answer is where that payment stands. Any
            // other refusal, with no payment under way, is passed on.
            return match self.payment(&payment_hash).await? {
                Some(payment) if !is_status(&payment, PaymentStatus::Failed) => {
                    Ok(Self::outbound_state(&payment))
                }
                _ => Err(refusal),
            };
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait_secs));
        loop {
            if let Some(payment) = self.payment(&payment_hash).await? {
                match Self::outbound_state(&payment) {
                    PaymentState::InFlight => {}
                    done => return Ok(done),
                }
            }
            if tokio::time::Instant::now() >= deadline {
                // Still unresolved when the caller's wait ran out: the node keeps paying.
                return Ok(PaymentState::InFlight);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn payment_state(&self, payment_hash: &str) -> Result<PaymentState, BackendError> {
        hash32(payment_hash, "payment hash")?;
        match self.payment(payment_hash).await? {
            Some(payment) if payment.direction == PaymentDirection::Outbound as i32 => {
                Ok(Self::outbound_state(&payment))
            }
            _ => Err(BackendError::Rejected(
                "LDK Server reported no such payment".into(),
            )),
        }
    }

    async fn payment_lookup(
        &self,
        payment_hash: &str,
    ) -> Result<Option<PaymentState>, BackendError> {
        hash32(payment_hash, "payment hash")?;
        // An outbound payment is kept under its hash; a hash never paid has no record.
        Ok(self
            .payment(payment_hash)
            .await?
            .filter(|payment| payment.direction == PaymentDirection::Outbound as i32)
            .map(|payment| Self::outbound_state(&payment)))
    }

    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        description: &str,
    ) -> Result<String, BackendError> {
        let request = Bolt11ReceiveRequest {
            amount_msat: Some(amount_msat),
            description: Some(Bolt11InvoiceDescription {
                kind: Some(DescriptionKind::Direct(description.to_string())),
            }),
            expiry_secs,
        };
        Ok(call(self.client.bolt11_receive(request)).await?.invoice)
    }

    async fn invoice_state(&self, bolt11: &str) -> Result<InvoiceStatus, BackendError> {
        let decoded = call(self.client.decode_invoice(DecodeInvoiceRequest {
            invoice: bolt11.to_string(),
        }))
        .await?;
        let payment = self.inbound(&decoded.payment_hash).await?;
        if let Some(payment) = &payment {
            if is_status(payment, PaymentStatus::Succeeded) {
                return Ok(InvoiceStatus::Paid);
            }
            if is_status(payment, PaymentStatus::Failed) {
                return Ok(InvoiceStatus::Expired);
            }
        }
        // Unpaid, or no record yet: the invoice's own expiry decides.
        if unix_now() >= decoded.timestamp.saturating_add(decoded.expiry) {
            Ok(InvoiceStatus::Expired)
        } else {
            Ok(InvoiceStatus::Unpaid)
        }
    }

    async fn connect(&self, remote_pubkey: &str, remote_addr: &str) -> Result<(), BackendError> {
        node_key(remote_pubkey)?;
        let request = ConnectPeerRequest {
            node_pubkey: remote_pubkey.to_string(),
            address: remote_addr.to_string(),
            persist: false,
        };
        call(self.client.connect_peer(request)).await.map(|_| ())
    }

    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError> {
        node_key(&req.remote_pubkey)?;
        let Some(address) = req.remote_addr.clone() else {
            return Err(BackendError::Rejected(
                "an LDK node needs the peer's address to open a channel".into(),
            ));
        };
        let request = OpenChannelRequest {
            node_pubkey: req.remote_pubkey.clone(),
            address,
            push_to_counterparty_msat: None,
            channel_config: None,
            announce_channel: req.announce,
            disable_counterparty_reserve: false,
            amount: Some(Amount::ChannelAmountSats(req.capacity_sat)),
        };
        let opened = call(self.client.open_channel(request)).await?;
        // The answer names the channel, not its funding: that shows in the list once the
        // funding transaction is made.
        let deadline = tokio::time::Instant::now() + FUNDING_WAIT;
        loop {
            let channels = call(self.client.list_channels(ListChannelsRequest {})).await?;
            let funding = channels
                .channels
                .into_iter()
                .find(|channel| channel.user_channel_id == opened.user_channel_id)
                .and_then(|channel| channel.funding_txo);
            if let Some(outpoint) = funding {
                return Ok(format!("{}:{}", outpoint.txid, outpoint.vout));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BackendError::Unavailable(
                    "the channel was asked for, but its funding outpoint did not show in time"
                        .into(),
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn channels_with(&self, remote_pubkey: &str) -> Result<Vec<ChannelView>, BackendError> {
        node_key(remote_pubkey)?;
        let channels = call(self.client.list_channels(ListChannelsRequest {})).await?;
        Ok(channels
            .channels
            .into_iter()
            .filter(|channel| channel.counterparty_node_id == remote_pubkey)
            .filter_map(|channel| {
                // A channel whose funding is not made yet has nothing to name it by.
                let outpoint = channel.funding_txo?;
                Some(ChannelView {
                    remote_pubkey: channel.counterparty_node_id,
                    funding_outpoint: format!("{}:{}", outpoint.txid, outpoint.vout),
                    capacity_sat: channel.channel_value_sats,
                    local_initiator: channel.is_outbound,
                    announced: channel.is_announced,
                    confirmations: channel.confirmations.unwrap_or(0),
                    active: channel.is_usable,
                })
            })
            .collect())
    }

    async fn closed_channel(
        &self,
        remote_pubkey: &str,
        _funding_outpoint: &str,
    ) -> Result<Option<ClosedChannel>, BackendError> {
        node_key(remote_pubkey)?;
        // The server lists open channels only; a close is told by a best-effort event alone.
        Err(BackendError::Unsupported(
            "LDK Server keeps no record of closed channels",
        ))
    }
    async fn graph_node(&self, node_pubkey: &str) -> Result<Option<(u32, u64)>, BackendError> {
        node_key(node_pubkey)?;
        let request = GraphGetNodeRequest {
            node_id: node_pubkey.to_string(),
        };
        let node = match call(self.client.graph_get_node(request)).await {
            Ok(answer) => answer.node,
            // The server answers an unknown node with a refusal, not an empty result.
            Err(BackendError::Rejected(_)) => None,
            Err(e) => return Err(e),
        };
        let Some(node) = node else {
            return Ok(None);
        };
        // The server's graph learns a channel's capacity only from a chain lookup it does not
        // always make. For a channel that is this node's own, the channel list knows it.
        let own: HashMap<u64, u64> = call(self.client.list_channels(ListChannelsRequest {}))
            .await?
            .channels
            .into_iter()
            .filter_map(|channel| Some((channel.short_channel_id?, channel.channel_value_sats)))
            .collect();
        let mut capacity_sat = 0u64;
        for scid in &node.channels {
            let request = GraphGetChannelRequest {
                short_channel_id: *scid,
            };
            // A channel that left the graph between the two calls adds nothing.
            let in_graph = match call(self.client.graph_get_channel(request)).await {
                Ok(answer) => answer.channel.and_then(|channel| channel.capacity_sats),
                Err(_) => None,
            };
            let sats = in_graph.or_else(|| own.get(scid).copied()).unwrap_or(0);
            capacity_sat = capacity_sat.saturating_add(sats);
        }
        let count = u32::try_from(node.channels.len()).unwrap_or(u32::MAX);
        Ok(Some((count, capacity_sat)))
    }

    async fn graph_channels(&self, node_pubkey: &str) -> Result<Vec<GraphChannel>, BackendError> {
        node_key(node_pubkey)?;
        Err(BackendError::Unsupported(
            "LDK Server's graph names channels by short channel id, not by funding outpoint",
        ))
    }
}
