//! The Core Lightning backend: `lightningd`'s JSON-RPC socket (`lightning-rpc`, the one
//! `lightning-cli` uses), with the [`hold`](https://github.com/BoltzExchange/hold) plugin for hold
//! invoices. Measured against a real node in `spikes/cln-m3/RESULTS.md`.
//!
//! - **The socket is the whole node.** Whoever can open it can do anything `lightning-cli` can;
//!   there is no narrower credential for a local client. The agent runs as the node's user, or
//!   as one that user lets in.
//! - **Hold invoices need the `hold` plugin** (`holdinvoice`, `listholdinvoices`,
//!   `settleholdinvoice`, `cancelholdinvoice`). Without it those four methods fail, and the node
//!   can still buy a lease or sit in a ring. Through the plugin's RPC a hold invoice always
//!   locks for [`HOLD_PLUGIN_CLTV_DELTA`] blocks, expires after an hour and has no description:
//!   a request for a longer lock is refused (and
//!   [`LightningBackend::max_hold_cltv_delta`] says so beforehand); the expiry and the
//!   description asked for are not used. The plugin gives a held payment back 3 blocks before
//!   it would expire.
//! - **Payments** are started with `pay` on a connection of its own and followed with
//!   `listpays`: `pay` answers only when the payment is over, which for a hold invoice is when
//!   the payee settles.
//! - **Confirmations** of a channel are counted from its short channel id and the node's own
//!   height; a channel that has no short channel id yet counts as unconfirmed.
//! - **`graph_channels` is not supported**: the node's graph names channels by short channel
//!   id, never by funding outpoint. Only an instance needs it.
//! - Core Lightning follows the chain by polling: for a few seconds after a block its height,
//!   and what depends on it, lags.
//!
//! Every RPC has a deadline (except the `pay` that is left running). Errors carry the node's
//! own text only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use cln_rpc::ClnRpc;
use serde_json::{Value, json};

use crate::hexutil::{hash32, node_key};
use crate::{
    BackendError, ChannelView, ClosedChannel, Closer, GraphChannel, HoldInvoiceRequest, HoldState,
    InvoiceStatus, LightningBackend, NodeInfo, OpenRequest, PaymentState,
};

/// Deadline for one RPC.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a payment is looked at while waiting for it.
const POLL: Duration = Duration::from_millis(500);
/// The lock time the `hold` plugin's `holdinvoice` gives every invoice, in blocks. It cannot be
/// set through the plugin's RPC; each invoice made is checked against what was asked.
pub const HOLD_PLUGIN_CLTV_DELTA: u32 = 80;
/// JSON-RPC's "invalid params": how the node answers a node id it cannot parse.
const INVALID_PARAMS: i32 = -32602;

/// One call on a connection of its own, as `lightning-cli` makes it.
async fn call_on(socket: &Path, method: &str, params: Value) -> Result<Value, BackendError> {
    let mut rpc = ClnRpc::new(socket).await.map_err(|_| {
        BackendError::Unavailable("Core Lightning's RPC socket cannot be opened".into())
    })?;
    rpc.call_raw::<Value, Value>(method, &params)
        .await
        .map_err(|e| match e.code {
            // No code: the socket or the answer failed, not the request.
            None => BackendError::Unavailable(e.message),
            Some(INVALID_PARAMS) => {
                BackendError::Rejected(format!("invalid request: {}", e.message))
            }
            Some(_) => BackendError::Rejected(e.message),
        })
}

fn text<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a str, BackendError> {
    value[key]
        .as_str()
        .ok_or_else(|| BackendError::Unavailable(format!("Core Lightning's answer has no {what}")))
}

/// A millisatoshi amount, which older nodes wrote as `"123msat"`.
fn msat(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.strip_suffix("msat")?.parse().ok())
}

/// Channel states in which a lease or a ring can count on a channel; any other state is closing
/// or gone.
const LIVE_STATES: [&str; 5] = [
    "OPENINGD",
    "CHANNELD_AWAITING_LOCKIN",
    "DUALOPEND_AWAITING_LOCKIN",
    "CHANNELD_NORMAL",
    "CHANNELD_AWAITING_SPLICE",
];

/// Who closed a channel, from a `closer` field (`local` or `remote`, absent when unknown).
fn closer_of(value: &Value) -> Closer {
    match value.as_str() {
        Some("local") => Closer::Local,
        Some("remote") => Closer::Remote,
        _ => Closer::Unknown,
    }
}

/// `txid:vout` split into its parts.
fn outpoint_parts(outpoint: &str) -> Result<(&str, u64), BackendError> {
    outpoint
        .rsplit_once(':')
        .and_then(|(txid, vout)| Some((txid, vout.parse().ok()?)))
        .filter(|(txid, _)| txid.len() == 64 && txid.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| BackendError::Rejected("a funding outpoint must be txid:vout".into()))
}

/// Unix seconds now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The block a short channel id (`block x tx x output`) names.
fn scid_block(scid: &str) -> Option<u32> {
    scid.split('x').next()?.parse().ok()
}

/// A Core Lightning node reached over its JSON-RPC socket.
pub struct ClnBackend {
    socket: PathBuf,
    labels: AtomicU64,
}

impl ClnBackend {
    /// A backend for the node whose RPC socket is at `rpc_socket`
    /// (`<lightning-dir>/<network>/lightning-rpc`). Nothing is opened yet: a wrong path shows
    /// up on the first call.
    pub fn connect(rpc_socket: &Path) -> Self {
        ClnBackend {
            socket: rpc_socket.to_path_buf(),
            labels: AtomicU64::new(0),
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        match tokio::time::timeout(RPC_TIMEOUT, call_on(&self.socket, method, params)).await {
            Ok(answer) => answer,
            Err(_) => Err(BackendError::Unavailable(
                "Core Lightning did not answer in time".into(),
            )),
        }
    }

    /// The node's record of an outbound payment, if it has one.
    async fn pay_record(&self, payment_hash: &str) -> Result<Option<PaymentState>, BackendError> {
        let answer = self
            .rpc("listpays", json!({"payment_hash": payment_hash}))
            .await?;
        let Some(pay) = answer["pays"].as_array().and_then(|pays| pays.last()) else {
            return Ok(None);
        };
        Ok(Some(match pay["status"].as_str() {
            Some("complete") => match pay["preimage"].as_str() {
                Some(preimage) => PaymentState::Succeeded {
                    preimage: preimage.to_string(),
                },
                None => PaymentState::InFlight,
            },
            Some("failed") => PaymentState::Failed {
                reason: "the payment failed".into(),
            },
            _ => PaymentState::InFlight,
        }))
    }

    async fn height(&self) -> Result<u32, BackendError> {
        let info = self.rpc("getinfo", json!({})).await?;
        let height = info["blockheight"].as_u64().unwrap_or(0);
        Ok(u32::try_from(height).unwrap_or(u32::MAX))
    }
}

#[async_trait]
impl LightningBackend for ClnBackend {
    async fn node_info(&self) -> Result<NodeInfo, BackendError> {
        let info = self.rpc("getinfo", json!({})).await?;
        let height = info["blockheight"].as_u64().unwrap_or(0);
        Ok(NodeInfo {
            node_pubkey: text(&info, "id", "node id")?.to_string(),
            network: text(&info, "network", "network")?.to_string(),
            block_height: u32::try_from(height).unwrap_or(u32::MAX),
            // The node says so itself while it is behind.
            synced: info.get("warning_bitcoind_sync").is_none()
                && info.get("warning_lightningd_sync").is_none(),
        })
    }

    async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
        let message = std::str::from_utf8(msg).map_err(|_| {
            BackendError::Rejected("Core Lightning signs text only; the message is not text".into())
        })?;
        let answer = self.rpc("signmessage", json!({"message": message})).await?;
        Ok(text(&answer, "zbase", "signature")?.to_string())
    }

    fn max_hold_cltv_delta(&self) -> Option<u32> {
        Some(HOLD_PLUGIN_CLTV_DELTA)
    }

    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError> {
        hash32(&req.payment_hash, "payment hash")?;
        if req.cltv_delta > HOLD_PLUGIN_CLTV_DELTA {
            return Err(BackendError::Rejected(format!(
                "the hold plugin locks a payment for {HOLD_PLUGIN_CLTV_DELTA} blocks; {} were \
                 asked for",
                req.cltv_delta
            )));
        }
        let made = self
            .rpc(
                "holdinvoice",
                json!({"payment_hash": req.payment_hash, "amount": req.amount_msat}),
            )
            .await?;
        let bolt11 = text(&made, "bolt11", "invoice")?.to_string();
        // What the plugin gave is what counts, not what it is known to give: a shorter lock
        // than asked for is not what the caller agreed to.
        let decoded = self.rpc("decode", json!({"string": bolt11})).await?;
        let lock = decoded["min_final_cltv_expiry"].as_u64().unwrap_or(0);
        if lock < u64::from(req.cltv_delta) {
            let _ = self
                .rpc(
                    "cancelholdinvoice",
                    json!({"payment_hash": req.payment_hash}),
                )
                .await;
            return Err(BackendError::Rejected(format!(
                "the hold plugin made an invoice that locks for {lock} blocks; {} were asked for",
                req.cltv_delta
            )));
        }
        Ok(bolt11)
    }

    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError> {
        hash32(payment_hash, "payment hash")?;
        let answer = self
            .rpc("listholdinvoices", json!({"payment_hash": payment_hash}))
            .await?;
        let Some(invoice) = answer["holdinvoices"]
            .as_array()
            .and_then(|all| all.first())
        else {
            return Err(BackendError::Rejected(
                "Core Lightning's hold plugin reported no such invoice".into(),
            ));
        };
        match invoice["state"].as_str() {
            Some("unpaid") => Ok(HoldState::Open),
            Some("accepted") => Ok(HoldState::Accepted),
            Some("paid") => Ok(HoldState::Settled),
            Some("cancelled") => Ok(HoldState::Canceled),
            _ => Err(BackendError::Unavailable(
                "the hold plugin reported a state this backend does not know".into(),
            )),
        }
    }

    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError> {
        hash32(preimage, "preimage")?;
        self.rpc("settleholdinvoice", json!({"preimage": preimage}))
            .await
            .map(|_| ())
    }

    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError> {
        hash32(payment_hash, "payment hash")?;
        self.rpc("cancelholdinvoice", json!({"payment_hash": payment_hash}))
            .await
            .map(|_| ())
    }

    async fn pay_invoice(
        &self,
        bolt11: &str,
        max_fee_msat: u64,
        wait_secs: u32,
    ) -> Result<PaymentState, BackendError> {
        let decoded = self.rpc("decode", json!({"string": bolt11})).await?;
        let payment_hash = text(&decoded, "payment_hash", "payment hash")?.to_string();
        // By the trait's rule, paying again while the payment is under way or once it is made
        // starts nothing and answers where it stands. (The node itself refuses the first and
        // answers the second.)
        match self.pay_record(&payment_hash).await? {
            Some(PaymentState::Failed { .. }) | None => {}
            Some(standing) => return Ok(standing),
        }
        // `pay` answers only when the payment is over; for a hold invoice that is when the
        // payee settles. It is left running on its own connection and followed from here.
        let socket = self.socket.clone();
        let params = json!({"bolt11": bolt11, "maxfee": max_fee_msat});
        let paying = tokio::spawn(async move { call_on(&socket, "pay", params).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait_secs));
        loop {
            if paying.is_finished() {
                // Over, one way or the other: the node's record says which. A refusal with no
                // payment made (no route, not enough funds) is passed on.
                let outcome = paying.await;
                return match (self.pay_record(&payment_hash).await?, outcome) {
                    (Some(state), _) => Ok(state),
                    (None, Ok(Err(refusal))) => Err(refusal),
                    (None, _) => Err(BackendError::Unavailable(
                        "Core Lightning made no record of the payment".into(),
                    )),
                };
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
        self.pay_record(payment_hash)
            .await?
            .ok_or_else(|| BackendError::Rejected("Core Lightning reported no such payment".into()))
    }

    async fn payment_lookup(
        &self,
        payment_hash: &str,
    ) -> Result<Option<PaymentState>, BackendError> {
        hash32(payment_hash, "payment hash")?;
        // `listpays` answers an empty list for a hash this node never paid.
        self.pay_record(payment_hash).await
    }

    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        description: &str,
    ) -> Result<String, BackendError> {
        // A label must be new; the time and a counter make it so.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let label = format!(
            "bipnip-{now}-{}",
            self.labels.fetch_add(1, Ordering::Relaxed)
        );
        let answer = self
            .rpc(
                "invoice",
                json!({
                    "amount_msat": amount_msat,
                    "label": label,
                    "description": description,
                    "expiry": expiry_secs,
                }),
            )
            .await?;
        Ok(text(&answer, "bolt11", "invoice")?.to_string())
    }

    async fn invoice_state(&self, bolt11: &str) -> Result<InvoiceStatus, BackendError> {
        let answer = self
            .rpc("listinvoices", json!({"invstring": bolt11}))
            .await?;
        let Some(invoice) = answer["invoices"].as_array().and_then(|all| all.first()) else {
            return Err(BackendError::Rejected(
                "Core Lightning reported no such invoice".into(),
            ));
        };
        match invoice["status"].as_str() {
            Some("paid") => Ok(InvoiceStatus::Paid),
            Some("expired") => Ok(InvoiceStatus::Expired),
            Some("unpaid") => match invoice["expires_at"].as_u64() {
                Some(at) if unix_now() >= at => Ok(InvoiceStatus::Expired),
                _ => Ok(InvoiceStatus::Unpaid),
            },
            _ => Err(BackendError::Unavailable(
                "Core Lightning reported an invoice state this backend does not know".into(),
            )),
        }
    }

    async fn connect(&self, remote_pubkey: &str, remote_addr: &str) -> Result<(), BackendError> {
        node_key(remote_pubkey)?;
        let Some((host, port)) = remote_addr.rsplit_once(':') else {
            return Err(BackendError::Rejected(
                "a peer's address must be host:port".into(),
            ));
        };
        let port: u16 = port
            .parse()
            .map_err(|_| BackendError::Rejected("a peer's address must be host:port".into()))?;
        self.rpc(
            "connect",
            json!({"id": remote_pubkey, "host": host, "port": port}),
        )
        .await
        .map(|_| ())
    }

    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError> {
        node_key(&req.remote_pubkey)?;
        if let Some(address) = &req.remote_addr {
            self.connect(&req.remote_pubkey, address).await?;
        }
        // Core Lightning takes a fee rate per 1000 vbytes.
        let feerate = format!("{}perkb", req.feerate_sat_vb.saturating_mul(1000));
        let answer = self
            .rpc(
                "fundchannel",
                json!({
                    "id": req.remote_pubkey,
                    "amount": req.capacity_sat,
                    "feerate": feerate,
                    "announce": req.announce,
                }),
            )
            .await?;
        let txid = text(&answer, "txid", "funding transaction")?;
        let Some(output) = answer["outnum"].as_u64() else {
            return Err(BackendError::Unavailable(
                "Core Lightning's answer has no funding output".into(),
            ));
        };
        Ok(format!("{txid}:{output}"))
    }

    async fn channels_with(&self, remote_pubkey: &str) -> Result<Vec<ChannelView>, BackendError> {
        node_key(remote_pubkey)?;
        let answer = self
            .rpc("listpeerchannels", json!({"id": remote_pubkey}))
            .await?;
        let height = self.height().await?;
        let channels = answer["channels"].as_array().cloned().unwrap_or_default();
        Ok(channels
            .iter()
            .filter_map(|channel| {
                // A channel that is closing or gone is not one a lease or a ring can count on.
                let state = channel["state"].as_str()?;
                if !LIVE_STATES.contains(&state) {
                    return None;
                }
                let txid = channel["funding_txid"].as_str()?;
                let output = channel["funding_outnum"].as_u64()?;
                let confirmations = channel["short_channel_id"]
                    .as_str()
                    .and_then(scid_block)
                    .map_or(0, |block| height.saturating_add(1).saturating_sub(block));
                Some(ChannelView {
                    remote_pubkey: remote_pubkey.to_string(),
                    funding_outpoint: format!("{txid}:{output}"),
                    capacity_sat: msat(&channel["total_msat"])? / 1000,
                    local_initiator: channel["opener"].as_str() == Some("local"),
                    announced: channel["private"].as_bool() == Some(false),
                    confirmations,
                    active: state == "CHANNELD_NORMAL"
                        && channel["peer_connected"].as_bool() == Some(true),
                })
            })
            .collect())
    }

    async fn closed_channel(
        &self,
        remote_pubkey: &str,
        funding_outpoint: &str,
    ) -> Result<Option<ClosedChannel>, BackendError> {
        node_key(remote_pubkey)?;
        let (txid, vout) = outpoint_parts(funding_outpoint)?;
        let funded_at = |channel: &&Value| {
            channel["funding_txid"].as_str() == Some(txid)
                && channel["funding_outnum"].as_u64() == Some(vout)
        };
        // A channel that is still closing is listed with its peer, in a closing state.
        let peer = self
            .rpc("listpeerchannels", json!({"id": remote_pubkey}))
            .await?;
        if let Some(channel) = peer["channels"]
            .as_array()
            .and_then(|all| all.iter().find(funded_at))
        {
            let state = channel["state"].as_str().unwrap_or_default();
            if LIVE_STATES.contains(&state) {
                return Ok(None);
            }
            return Ok(Some(ClosedChannel {
                closer: closer_of(&channel["closer"]),
                close_height: None,
            }));
        }
        // Once its outputs are settled it moves to the closed list, which gives no height.
        let closed = self
            .rpc("listclosedchannels", json!({"id": remote_pubkey}))
            .await?;
        Ok(closed["closedchannels"]
            .as_array()
            .and_then(|all| all.iter().find(funded_at))
            .map(|channel| ClosedChannel {
                closer: closer_of(&channel["closer"]),
                close_height: None,
            }))
    }

    async fn graph_node(&self, node_pubkey: &str) -> Result<Option<(u32, u64)>, BackendError> {
        node_key(node_pubkey)?;
        let listed = match self
            .rpc("listchannels", json!({"source": node_pubkey}))
            .await
        {
            Ok(answer) => answer,
            // Hex of the right length that is no point on the curve: the node refuses to look
            // it up. It is in nobody's graph.
            Err(BackendError::Rejected(why)) if why.starts_with("invalid request") => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let channels = listed["channels"].as_array().cloned().unwrap_or_default();
        if channels.is_empty() {
            // A node the graph knows without a public channel, or one it has never heard of.
            let nodes = self.rpc("listnodes", json!({"id": node_pubkey})).await?;
            let known = nodes["nodes"].as_array().is_some_and(|all| !all.is_empty());
            return Ok(known.then_some((0, 0)));
        }
        let capacity_sat = channels
            .iter()
            .filter_map(|channel| msat(&channel["amount_msat"]))
            .fold(0u64, |sum, amount| sum.saturating_add(amount / 1000));
        let count = u32::try_from(channels.len()).unwrap_or(u32::MAX);
        Ok(Some((count, capacity_sat)))
    }

    async fn graph_channels(&self, node_pubkey: &str) -> Result<Vec<GraphChannel>, BackendError> {
        node_key(node_pubkey)?;
        Err(BackendError::Unsupported(
            "Core Lightning's graph names channels by short channel id, not by funding outpoint",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outpoints_and_closers_are_read_strictly() {
        let txid = "ab".repeat(32);
        assert_eq!(outpoint_parts(&format!("{txid}:1")), Ok((txid.as_str(), 1)));
        let (no_vout, bad_vout) = (format!("{txid}:"), format!("{txid}:x"));
        for bad in [
            "",
            ":1",
            "ab:1",
            no_vout.as_str(),
            bad_vout.as_str(),
            txid.as_str(),
        ] {
            assert!(outpoint_parts(bad).is_err(), "{bad}");
        }
        assert_eq!(closer_of(&json!("local")), Closer::Local);
        assert_eq!(closer_of(&json!("remote")), Closer::Remote);
        assert_eq!(closer_of(&Value::Null), Closer::Unknown);
        assert_eq!(closer_of(&json!("mutual")), Closer::Unknown);
    }

    #[test]
    fn amounts_and_short_channel_ids_are_read_in_both_forms() {
        assert_eq!(msat(&json!(1_500_000)), Some(1_500_000));
        assert_eq!(msat(&json!("1500000msat")), Some(1_500_000));
        assert_eq!(msat(&json!("1500000")), None);
        assert_eq!(msat(&json!(null)), None);
        assert_eq!(scid_block("108x1x0"), Some(108));
        assert_eq!(scid_block("x1x0"), None);
        assert_eq!(scid_block(""), None);
    }

    #[test]
    fn arguments_are_checked_before_the_socket_is_touched() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(checked_before_the_socket());
    }

    async fn checked_before_the_socket() {
        // No socket at this path: a call that reached it would say "unavailable".
        let node = ClnBackend::connect(Path::new("/nonexistent/lightning-rpc"));
        for refused in [
            node.hold_invoice_state("zz").await.map(|_| ()),
            node.settle_hold_invoice("abc").await,
            node.cancel_hold_invoice("").await,
            node.channels_with("02abc").await.map(|_| ()),
            node.graph_node("nope").await.map(|_| ()),
            node.connect(&format!("02{}", "11".repeat(32)), "no-port")
                .await,
            node.add_hold_invoice(&HoldInvoiceRequest {
                payment_hash: "11".repeat(32),
                amount_msat: 1_000,
                expiry_secs: 60,
                cltv_delta: HOLD_PLUGIN_CLTV_DELTA + 1,
                description: String::new(),
            })
            .await
            .map(|_| ()),
        ] {
            assert!(
                matches!(refused, Err(BackendError::Rejected(_))),
                "{refused:?}"
            );
        }
        assert!(matches!(
            node.node_info().await,
            Err(BackendError::Unavailable(_))
        ));
        assert!(matches!(
            node.graph_channels(&format!("02{}", "11".repeat(32))).await,
            Err(BackendError::Unsupported(_))
        ));
        assert_eq!(node.max_hold_cltv_delta(), Some(HOLD_PLUGIN_CLTV_DELTA));
    }
}
