//! The LND backend (gRPC through `fedimint-tonic-lnd`, the client mostrod uses).
//!
//! - Every RPC has a deadline (the client sets none of its own).
//! - Hold-invoice state is read by lookup, never from a long-lived subscription, so an LND restart
//!   loses nothing (Mostro issue #1078).
//! - A payment that outlives `wait_secs` is reported `InFlight`, never cancelled: dropping the
//!   stream does not stop LND from completing it, and callers follow it by hash.
//! - Errors carry LND's own status text only; no credential or invoice is ever put in one.
//!
//! A macaroon for this backend needs: `info:read`, `message:write` (signmessage), `invoices:read`,
//! `invoices:write`, `offchain:read`, `offchain:write`, `peers:write`, `onchain:write` (opens).

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use fedimint_tonic_lnd::Client;
use fedimint_tonic_lnd::invoicesrpc::{AddHoldInvoiceRequest, CancelInvoiceMsg, SettleInvoiceMsg};
use fedimint_tonic_lnd::lnrpc::channel_point::FundingTxid;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use fedimint_tonic_lnd::lnrpc::{
    ConnectPeerRequest, GetInfoRequest, Initiator, Invoice, LightningAddress, ListChannelsRequest,
    NodeInfoRequest, OpenChannelRequest, Payment, PaymentHash, PendingChannelsRequest,
    SignMessageRequest,
};
use fedimint_tonic_lnd::routerrpc::{SendPaymentRequest, TrackPaymentRequest};
use fedimint_tonic_lnd::tonic::{Code, Status};

use crate::hexutil::{self, hash32, node_key};
use crate::{
    BackendError, ChannelView, HoldInvoiceRequest, HoldState, LightningBackend, NodeInfo,
    OpenRequest, PaymentState,
};

/// Deadline for an ordinary RPC.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for a channel open (peer negotiation and funding broadcast).
const OPEN_TIMEOUT: Duration = Duration::from_secs(120);
/// Seconds LND keeps trying routes for a payment.
const ROUTE_TIMEOUT_SECS: i32 = 60;

/// A connection to one LND node.
#[derive(Clone)]
pub struct LndBackend {
    client: Client,
}

fn status_error(s: Status) -> BackendError {
    match s.code() {
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Aborted => {
            BackendError::Unavailable(s.message().to_string())
        }
        _ => BackendError::Rejected(s.message().to_string()),
    }
}

async fn call<T, F>(limit: Duration, fut: F) -> Result<T, BackendError>
where
    F: Future<Output = Result<fedimint_tonic_lnd::tonic::Response<T>, Status>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(resp)) => Ok(resp.into_inner()),
        Ok(Err(status)) => Err(status_error(status)),
        Err(_) => Err(BackendError::Unavailable(
            "LND did not answer in time".into(),
        )),
    }
}

fn to_i64(v: u64, what: &'static str) -> Result<i64, BackendError> {
    i64::try_from(v).map_err(|_| BackendError::Rejected(format!("{what} is out of range")))
}

/// LND's network name, in the names bindings use (`mainnet` is `bitcoin`).
pub fn network_name(lnd: &str) -> String {
    match lnd {
        "mainnet" => "bitcoin".to_string(),
        other => other.to_string(),
    }
}

/// Confirmations of an open channel from its short channel id (block height in the top 3 bytes).
/// Alias ids of zero-conf channels carry heights above the chain tip and count as 0.
pub fn confirmations_from_scid(chan_id: u64, tip: u32) -> u32 {
    let height = (chan_id >> 40) as u32;
    if chan_id == 0 || height == 0 || height > tip {
        0
    } else {
        tip - height + 1
    }
}

/// `txid:vout` from LND's channel point (its txid bytes are in internal, reversed order).
fn outpoint(txid: Option<FundingTxid>, vout: u32) -> Result<String, BackendError> {
    let txid = match txid {
        Some(FundingTxid::FundingTxidStr(s)) => s,
        Some(FundingTxid::FundingTxidBytes(mut b)) => {
            b.reverse();
            hexutil::encode(&b)
        }
        None => {
            return Err(BackendError::Rejected(
                "LND returned no funding txid".into(),
            ));
        }
    };
    Ok(format!("{txid}:{vout}"))
}

fn payment_state(p: &Payment) -> PaymentState {
    match PaymentStatus::try_from(p.status) {
        Ok(PaymentStatus::Succeeded) => PaymentState::Succeeded {
            preimage: p.payment_preimage.clone(),
        },
        Ok(PaymentStatus::Failed) => PaymentState::Failed {
            reason: format!("failure reason {}", p.failure_reason),
        },
        _ => PaymentState::InFlight,
    }
}

impl LndBackend {
    /// Connects to LND's gRPC at `address` (`https://host:port`) with its TLS certificate and a
    /// macaroon file. The connection is lazy: a wrong address shows up on the first call.
    pub async fn connect(
        address: &str,
        tls_cert: &Path,
        macaroon: &Path,
    ) -> Result<Self, BackendError> {
        // rustls needs a process-wide provider; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = fedimint_tonic_lnd::connect(
            address.to_string(),
            tls_cert.to_path_buf(),
            macaroon.to_path_buf(),
        )
        .await
        .map_err(|e| BackendError::Unavailable(format!("LND connection setup: {e}")))?;
        Ok(LndBackend { client })
    }

    async fn tip(&self) -> Result<u32, BackendError> {
        Ok(self.node_info().await?.block_height)
    }
}

#[async_trait]
impl LightningBackend for LndBackend {
    async fn node_info(&self) -> Result<NodeInfo, BackendError> {
        let mut c = self.client.clone();
        let info = call(RPC_TIMEOUT, c.lightning().get_info(GetInfoRequest {})).await?;
        let network = info
            .chains
            .first()
            .map(|c| network_name(&c.network))
            .unwrap_or_default();
        Ok(NodeInfo {
            node_pubkey: info.identity_pubkey,
            network,
            block_height: info.block_height,
            synced: info.synced_to_chain && info.synced_to_graph,
        })
    }

    async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
        let mut c = self.client.clone();
        let req = SignMessageRequest {
            msg: msg.to_vec(),
            ..Default::default()
        };
        Ok(call(RPC_TIMEOUT, c.lightning().sign_message(req))
            .await?
            .signature)
    }

    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError> {
        let hash = hash32(&req.payment_hash, "payment hash")?;
        let mut c = self.client.clone();
        let msg = AddHoldInvoiceRequest {
            memo: req.description.clone(),
            hash,
            value_msat: to_i64(req.amount_msat, "amount")?,
            expiry: i64::from(req.expiry_secs),
            cltv_expiry: u64::from(req.cltv_delta),
            ..Default::default()
        };
        Ok(call(RPC_TIMEOUT, c.invoices().add_hold_invoice(msg))
            .await?
            .payment_request)
    }

    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError> {
        let r_hash = hash32(payment_hash, "payment hash")?;
        let mut c = self.client.clone();
        let req = PaymentHash {
            r_hash,
            ..Default::default()
        };
        let invoice = call(RPC_TIMEOUT, c.lightning().lookup_invoice(req)).await?;
        Ok(match InvoiceState::try_from(invoice.state) {
            Ok(InvoiceState::Open) => HoldState::Open,
            Ok(InvoiceState::Accepted) => HoldState::Accepted,
            Ok(InvoiceState::Settled) => HoldState::Settled,
            Ok(InvoiceState::Canceled) => HoldState::Canceled,
            Err(_) => return Err(BackendError::Rejected("unknown invoice state".into())),
        })
    }

    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        let preimage = hash32(preimage, "preimage")?;
        let mut c = self.client.clone();
        call(
            RPC_TIMEOUT,
            c.invoices().settle_invoice(SettleInvoiceMsg { preimage }),
        )
        .await?;
        Ok(())
    }

    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError> {
        let payment_hash = hash32(payment_hash, "payment hash")?;
        let mut c = self.client.clone();
        call(
            RPC_TIMEOUT,
            c.invoices()
                .cancel_invoice(CancelInvoiceMsg { payment_hash }),
        )
        .await?;
        Ok(())
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        max_fee_msat: u64,
        wait_secs: u32,
    ) -> Result<PaymentState, BackendError> {
        let mut c = self.client.clone();
        let req = SendPaymentRequest {
            payment_request: bolt11.to_string(),
            timeout_seconds: ROUTE_TIMEOUT_SECS,
            fee_limit_msat: to_i64(max_fee_msat, "fee limit")?,
            ..Default::default()
        };
        let mut stream = call(RPC_TIMEOUT, c.router().send_payment_v2(req)).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait_secs));
        loop {
            match tokio::time::timeout_at(deadline, stream.message()).await {
                // Still unresolved when the caller's wait ran out: LND keeps the payment going.
                Err(_) => return Ok(PaymentState::InFlight),
                Ok(Err(status)) => return Err(status_error(status)),
                Ok(Ok(None)) => return Ok(PaymentState::InFlight),
                Ok(Ok(Some(payment))) => match payment_state(&payment) {
                    PaymentState::InFlight => continue,
                    done => return Ok(done),
                },
            }
        }
    }

    async fn payment_state(&self, payment_hash: &str) -> Result<PaymentState, BackendError> {
        let payment_hash = hash32(payment_hash, "payment hash")?;
        let mut c = self.client.clone();
        let req = TrackPaymentRequest {
            payment_hash,
            no_inflight_updates: false,
        };
        let mut stream = call(RPC_TIMEOUT, c.router().track_payment_v2(req)).await?;
        match tokio::time::timeout(RPC_TIMEOUT, stream.message()).await {
            Ok(Ok(Some(payment))) => Ok(payment_state(&payment)),
            Ok(Ok(None)) => Err(BackendError::Rejected(
                "LND reported no such payment".into(),
            )),
            Ok(Err(status)) => Err(status_error(status)),
            Err(_) => Err(BackendError::Unavailable(
                "LND did not answer in time".into(),
            )),
        }
    }

    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        description: &str,
    ) -> Result<String, BackendError> {
        let mut c = self.client.clone();
        let invoice = Invoice {
            memo: description.to_string(),
            value_msat: to_i64(amount_msat, "amount")?,
            expiry: i64::from(expiry_secs),
            ..Default::default()
        };
        Ok(call(RPC_TIMEOUT, c.lightning().add_invoice(invoice))
            .await?
            .payment_request)
    }

    async fn connect(&self, remote_pubkey: &str, remote_addr: &str) -> Result<(), BackendError> {
        node_key(remote_pubkey)?;
        let mut c = self.client.clone();
        let req = ConnectPeerRequest {
            addr: Some(LightningAddress {
                pubkey: remote_pubkey.to_string(),
                host: remote_addr.to_string(),
            }),
            perm: false,
            timeout: 20,
        };
        match call(RPC_TIMEOUT, c.lightning().connect_peer(req)).await {
            Ok(_) => Ok(()),
            // Being connected already is the state the caller asked for.
            Err(BackendError::Rejected(m)) if m.contains("already connected") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError> {
        let node_pubkey = node_key(&req.remote_pubkey)?;
        if let Some(addr) = &req.remote_addr {
            self.connect(&req.remote_pubkey, addr).await?;
        }
        let mut c = self.client.clone();
        let msg = OpenChannelRequest {
            node_pubkey,
            local_funding_amount: to_i64(req.capacity_sat, "capacity")?,
            private: !req.announce,
            sat_per_vbyte: req.feerate_sat_vb,
            ..Default::default()
        };
        let point = call(OPEN_TIMEOUT, c.lightning().open_channel_sync(msg)).await?;
        outpoint(point.funding_txid, point.output_index)
    }

    async fn channels_with(&self, remote_pubkey: &str) -> Result<Vec<ChannelView>, BackendError> {
        let peer = node_key(remote_pubkey)?;
        let tip = self.tip().await?;
        let mut c = self.client.clone();
        let open = call(
            RPC_TIMEOUT,
            c.lightning().list_channels(ListChannelsRequest {
                peer,
                ..Default::default()
            }),
        )
        .await?;
        let mut out: Vec<ChannelView> = open
            .channels
            .into_iter()
            .map(|ch| ChannelView {
                remote_pubkey: ch.remote_pubkey,
                funding_outpoint: ch.channel_point,
                capacity_sat: u64::try_from(ch.capacity).unwrap_or(0),
                local_initiator: ch.initiator,
                announced: !ch.private,
                confirmations: confirmations_from_scid(ch.chan_id, tip),
                active: ch.active,
            })
            .collect();
        let pending = call(
            RPC_TIMEOUT,
            c.lightning()
                .pending_channels(PendingChannelsRequest::default()),
        )
        .await?;
        for p in pending.pending_open_channels {
            let Some(ch) = p.channel else { continue };
            if ch.remote_node_pub != remote_pubkey {
                continue;
            }
            out.push(ChannelView {
                remote_pubkey: ch.remote_node_pub,
                funding_outpoint: ch.channel_point,
                capacity_sat: u64::try_from(ch.capacity).unwrap_or(0),
                local_initiator: ch.initiator == Initiator::Local as i32,
                announced: !ch.private,
                confirmations: 0,
                active: false,
            });
        }
        Ok(out)
    }

    async fn graph_node(&self, node_pubkey: &str) -> Result<Option<(u32, u64)>, BackendError> {
        node_key(node_pubkey)?;
        let mut c = self.client.clone();
        let req = NodeInfoRequest {
            pub_key: node_pubkey.to_string(),
            include_channels: false,
        };
        let limit = tokio::time::timeout(RPC_TIMEOUT, c.lightning().get_node_info(req)).await;
        match limit {
            Ok(Ok(resp)) => {
                let info = resp.into_inner();
                Ok(Some((
                    info.num_channels,
                    u64::try_from(info.total_capacity).unwrap_or(0),
                )))
            }
            Ok(Err(status)) if status.code() == Code::NotFound => Ok(None),
            Ok(Err(status)) => Err(status_error(status)),
            Err(_) => Err(BackendError::Unavailable(
                "LND did not answer in time".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_is_named_bitcoin() {
        assert_eq!(network_name("mainnet"), "bitcoin");
        assert_eq!(network_name("regtest"), "regtest");
        assert_eq!(network_name("signet"), "signet");
    }

    #[test]
    fn confirmations_come_from_the_scid_height() {
        let scid = |height: u64| (height << 40) | (1 << 16);
        assert_eq!(confirmations_from_scid(scid(100), 100), 1);
        assert_eq!(confirmations_from_scid(scid(100), 105), 6);
        assert_eq!(
            confirmations_from_scid(scid(16_000_001), 105),
            0,
            "zero-conf alias"
        );
        assert_eq!(confirmations_from_scid(0, 105), 0);
    }

    #[test]
    fn outpoints_use_display_txid_order() {
        let mut bytes = vec![0u8; 32];
        bytes[0] = 0xaa;
        bytes[31] = 0x01;
        let got = outpoint(Some(FundingTxid::FundingTxidBytes(bytes)), 1).expect("outpoint");
        assert!(got.starts_with("01") && got.ends_with("aa:1"), "{got}");
        assert!(outpoint(None, 0).is_err());
    }

    #[test]
    fn status_codes_map_to_retryable_or_not() {
        assert!(matches!(
            status_error(Status::unavailable("down")),
            BackendError::Unavailable(_)
        ));
        assert!(matches!(
            status_error(Status::invalid_argument("bad")),
            BackendError::Rejected(_)
        ));
    }

    #[test]
    fn payments_map_by_status() {
        let mut p = Payment {
            status: PaymentStatus::InFlight as i32,
            ..Default::default()
        };
        assert_eq!(payment_state(&p), PaymentState::InFlight);
        p.status = PaymentStatus::Succeeded as i32;
        p.payment_preimage = "ab".repeat(32);
        assert_eq!(
            payment_state(&p),
            PaymentState::Succeeded {
                preimage: "ab".repeat(32)
            }
        );
        p.status = PaymentStatus::Failed as i32;
        assert!(matches!(payment_state(&p), PaymentState::Failed { .. }));
    }
}
