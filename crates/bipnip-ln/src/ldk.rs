//! The LDK backend: an [LDK Server](https://github.com/lightningdevkit/ldk-server) next to the
//! agent, over its gRPC API (`ldk-server-client`). The agent stays beside the node, as with LND;
//! it does not embed one.
//!
//! What LDK Server gives and what it does not, as measured against a real one
//! (`spikes/ldk-m3/RESULTS.md`):
//!
//! - **Paying, channels, signing** map onto the trait directly. An outbound payment is kept
//!   under its payment hash, so its state is a lookup.
//! - **Hold invoices** work, with limits a seller must know:
//!   - The server keeps no record of a hold invoice until an HTLC arrives for it. So an
//!     invoice nobody has paid and a hash nobody made an invoice for look the same: both read
//!     as [`HoldState::Open`].
//!   - A held payment has an id of its own, not its hash. This backend finds it by searching
//!     the payment list for the hash, and remembers what it found. Nothing here depends on the
//!     server's event stream, which is best-effort and never replayed.
//!   - An unpaid invoice cannot be cancelled at the node. A cancel is remembered by this
//!     process, and a payment that arrives later is failed when it is next looked at. If the
//!     process ends first, such a payment stays held until its deadline and is then given
//!     back by the node.
//!   - **The lock time cannot be set.** The node's hold invoices ask for a fixed
//!     `min_final_cltv_expiry_delta` (42 blocks when measured), and the node gives a held
//!     payment back about 39 blocks before it expires: a seller has only a few blocks to open,
//!     confirm and settle. A request for more blocks than the node gives is refused rather
//!     than quietly shortened. Until LDK Server lets the caller set it, selling leases from
//!     an LDK node is for regtest and short confirmations only.
//! - **`graph_node`** counts a node's public channels; their capacity is known only where the
//!   server's graph has it, or the channel is this node's own. Read the capacity as a floor.
//! - **`graph_channels` is not supported**: the server's graph names channels by short channel
//!   id, never by funding outpoint. Only an instance needs it.
//! - **Channel opens** take the node's own fee estimate; the request's fee rate is not used.
//!   The peer's address is required.
//!
//! Every RPC has a deadline. Errors carry the server's own text only; the macaroon and
//! invoices are never put in one.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::error::{LdkServerError, LdkServerErrorCode};
use ldk_server_client::ldk_server_grpc::api::open_channel_request::Amount;
use ldk_server_client::ldk_server_grpc::api::{
    Bolt11ClaimForIdRequest, Bolt11FailForIdRequest, Bolt11ReceiveForHashRequest,
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
use sha2::{Digest, Sha256};

use crate::hexutil::{self, hash32, node_key};
use crate::{
    BackendError, ChannelView, GraphChannel, HoldInvoiceRequest, HoldState, LightningBackend,
    NodeInfo, OpenRequest, PaymentState,
};

/// Deadline for one RPC.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for a new channel's funding outpoint to show in the channel list.
const FUNDING_WAIT: Duration = Duration::from_secs(60);
/// How often a payment or a new channel is looked at while waiting for it.
const POLL: Duration = Duration::from_millis(500);

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

/// The payment hash a BOLT 11 payment record carries, if it is one.
fn bolt11_hash(payment: &Payment) -> Option<&str> {
    match payment.kind.as_ref()?.kind.as_ref()? {
        PaymentKindOf::Bolt11(bolt11) => Some(bolt11.hash.as_str()),
        _ => None,
    }
}

fn bolt11_preimage(payment: &Payment) -> Option<&str> {
    match payment.kind.as_ref()?.kind.as_ref()? {
        PaymentKindOf::Bolt11(bolt11) => bolt11.preimage.as_deref(),
        _ => None,
    }
}

fn is_status(payment: &Payment, status: PaymentStatus) -> bool {
    payment.status == status as i32
}

/// An LDK Server reached over gRPC.
pub struct LdkBackend {
    client: LdkServerClient,
    /// The ids of held payments already found in the payment list, by payment hash.
    found: Mutex<HashMap<String, Vec<String>>>,
    /// Hold invoices cancelled before anyone paid them: a payment that still arrives is failed.
    cancelled: Mutex<HashSet<String>>,
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
        Ok(LdkBackend {
            client,
            found: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashSet::new()),
        })
    }

    async fn payment(&self, payment_id: &str) -> Result<Option<Payment>, BackendError> {
        let request = GetPaymentDetailsRequest {
            payment_id: payment_id.to_string(),
        };
        Ok(call(self.client.get_payment_details(request))
            .await?
            .payment)
    }

    /// Every inbound payment record for `payment_hash`: the ids found before, looked up again,
    /// and whatever else the payment list now holds for the hash.
    async fn inbound(&self, payment_hash: &str) -> Result<Vec<Payment>, BackendError> {
        let known: Vec<String> = self
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(payment_hash)
            .cloned()
            .unwrap_or_default();
        let mut records = Vec::new();
        for id in &known {
            if let Some(payment) = self.payment(id).await? {
                records.push(payment);
            }
        }
        // A settled payment needs no second look. Anything else may have gained a record: the
        // same invoice can be paid more than once.
        if records
            .iter()
            .any(|payment| is_status(payment, PaymentStatus::Succeeded))
        {
            return Ok(records);
        }
        let mut page_token = None;
        loop {
            let page = call(
                self.client
                    .list_payments(ListPaymentsRequest { page_token }),
            )
            .await?;
            for payment in page.payments {
                let ours = payment.direction == PaymentDirection::Inbound as i32
                    && bolt11_hash(&payment) == Some(payment_hash)
                    && !known.contains(&payment.payment_id);
                if ours {
                    records.push(payment);
                }
            }
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        if records.len() > known.len() {
            self.found
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    payment_hash.to_string(),
                    records.iter().map(|p| p.payment_id.clone()).collect(),
                );
        }
        Ok(records)
    }

    async fn fail(&self, payment_id: &str) -> Result<(), BackendError> {
        let request = Bolt11FailForIdRequest {
            payment_id: payment_id.to_string(),
        };
        call(self.client.bolt11_fail_for_id(request))
            .await
            .map(|_| ())
    }

    fn is_cancelled(&self, payment_hash: &str) -> bool {
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(payment_hash)
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
        let request = Bolt11ReceiveForHashRequest {
            amount_msat: Some(req.amount_msat),
            description: Some(Bolt11InvoiceDescription {
                kind: Some(DescriptionKind::Direct(req.description.clone())),
            }),
            expiry_secs: req.expiry_secs,
            payment_hash: req.payment_hash.clone(),
        };
        let invoice = call(self.client.bolt11_receive_for_hash(request))
            .await?
            .invoice;
        // The lock time is the node's, not ours to set. Fewer blocks than asked for is not
        // what the caller agreed to; the invoice is not handed out.
        let decoded = call(self.client.decode_invoice(DecodeInvoiceRequest {
            invoice: invoice.clone(),
        }))
        .await?;
        if decoded.min_final_cltv_expiry_delta < u64::from(req.cltv_delta) {
            return Err(BackendError::Rejected(format!(
                "this LDK node's hold invoices lock a payment for {} blocks; {} were asked for, \
                 and LDK Server has no way to set it",
                decoded.min_final_cltv_expiry_delta, req.cltv_delta
            )));
        }
        // A new invoice on a hash cancelled before is a new start.
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&req.payment_hash);
        Ok(invoice)
    }

    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError> {
        hash32(payment_hash, "payment hash")?;
        let records = self.inbound(payment_hash).await?;
        if records
            .iter()
            .any(|payment| is_status(payment, PaymentStatus::Succeeded))
        {
            return Ok(HoldState::Settled);
        }
        let held: Vec<&Payment> = records
            .iter()
            .filter(|payment| is_status(payment, PaymentStatus::Pending))
            .collect();
        if self.is_cancelled(payment_hash) {
            // Paid after it was cancelled: given back now.
            for payment in held {
                self.fail(&payment.payment_id).await?;
            }
            return Ok(HoldState::Canceled);
        }
        if !held.is_empty() {
            return Ok(HoldState::Accepted);
        }
        if records.is_empty() {
            // Not paid yet, or never made: the server cannot tell these apart.
            Ok(HoldState::Open)
        } else {
            Ok(HoldState::Canceled)
        }
    }

    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        let bytes = hash32(preimage, "preimage")?;
        let payment_hash = hexutil::encode(&Sha256::digest(&bytes));
        let records = self.inbound(&payment_hash).await?;
        let held: Vec<&Payment> = records
            .iter()
            .filter(|payment| is_status(payment, PaymentStatus::Pending))
            .collect();
        if held.is_empty() {
            // Settled before: a retry after a lost answer is fine. Otherwise there is nothing
            // to settle.
            return if records
                .iter()
                .any(|payment| is_status(payment, PaymentStatus::Succeeded))
            {
                Ok(())
            } else {
                Err(BackendError::Rejected(
                    "no payment is held for this preimage".into(),
                ))
            };
        }
        for payment in held {
            let request = Bolt11ClaimForIdRequest {
                payment_id: payment.payment_id.clone(),
                claimable_amount_msat: payment.amount_msat,
                preimage: preimage.to_string(),
            };
            call(self.client.bolt11_claim_for_id(request)).await?;
        }
        Ok(())
    }

    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError> {
        hash32(payment_hash, "payment hash")?;
        let records = self.inbound(payment_hash).await?;
        if records
            .iter()
            .any(|payment| is_status(payment, PaymentStatus::Succeeded))
        {
            return Err(BackendError::Rejected(
                "the invoice is settled; it cannot be cancelled".into(),
            ));
        }
        // Remembered first: a payment that arrives after this is failed when it is seen.
        self.cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(payment_hash.to_string());
        for payment in records
            .iter()
            .filter(|payment| is_status(payment, PaymentStatus::Pending))
        {
            self.fail(&payment.payment_id).await?;
        }
        Ok(())
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
