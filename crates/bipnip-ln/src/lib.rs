//! Lightning backends behind one trait (`docs/decisions/0006-lightning-backend.md`).
//!
//! [`LightningBackend`] widens Mostro's `EscrowBackend` (hold invoices with the instance's own
//! preimage) with what bipnip's liquidity trades need: hold invoices **on a hash someone else
//! chose** (the lease buyer keeps the preimage), channel opens and inspection, graph lookups,
//! and the node's `signmessage` for node ↔ Nostr key bindings. Implementations for LND, Core
//! Lightning and LDK land in M1; this crate fixes the contract first.
//!
//! Keys and identifiers cross this boundary as hex strings, so the trait does not tie callers to
//! one secp256k1 version (rust-bitcoin 0.32 and rust-nostr 0.45 carry two).

use async_trait::async_trait;

/// Why a backend call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The node is unreachable or the transport failed; the call may be retried.
    Unavailable(String),
    /// The node refused the request (bad arguments, insufficient funds, unknown peer…).
    Rejected(String),
    /// This backend cannot do this at all (for example, hold invoices on CLN without the plugin).
    Unsupported(&'static str),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unavailable(e) => write!(f, "lightning node unavailable: {e}"),
            BackendError::Rejected(e) => write!(f, "lightning node rejected the request: {e}"),
            BackendError::Unsupported(what) => write!(f, "not supported by this backend: {what}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// The node's own identity and chain view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// Identity key, 66 lower-case hex (compressed).
    pub node_pubkey: String,
    /// `bitcoin`, `testnet`, `testnet4`, `signet` or `regtest`.
    pub network: String,
    /// Best block height the node has seen.
    pub block_height: u32,
    /// True when the node considers itself synced to chain and graph.
    pub synced: bool,
}

/// A hold invoice's state, as the node reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldState {
    /// Created, not paid.
    Open,
    /// An HTLC set is locked in and waits for settle or cancel.
    Accepted,
    /// Settled with the preimage.
    Settled,
    /// Cancelled, or expired; the payer is refunded.
    Canceled,
}

/// Parameters for a hold invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldInvoiceRequest {
    /// 32-byte payment hash, 64 lower-case hex.
    pub payment_hash: String,
    /// Amount in millisatoshi.
    pub amount_msat: u64,
    /// Seconds until the invoice expires unpaid.
    pub expiry_secs: u32,
    /// `min_final_cltv_expiry_delta`, bounding how long an accepted HTLC can be held.
    pub cltv_delta: u32,
    /// Invoice description (shown to the payer; no secrets).
    pub description: String,
}

/// A channel as the local node sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelView {
    /// Remote node's identity key, 66 lower-case hex.
    pub remote_pubkey: String,
    /// Funding outpoint `txid:vout`.
    pub funding_outpoint: String,
    /// Capacity in satoshi.
    pub capacity_sat: u64,
    /// True when the local node opened (funded) the channel.
    pub local_initiator: bool,
    /// True when the channel is announced to the network.
    pub announced: bool,
    /// Confirmations of the funding transaction (0 while pending).
    pub confirmations: u32,
    /// True when the channel is open and usable.
    pub active: bool,
}

/// A request to open a channel from the node's own wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// Remote node's identity key, 66 lower-case hex.
    pub remote_pubkey: String,
    /// `host:port` to connect to first, if not already connected.
    pub remote_addr: Option<String>,
    /// Channel capacity in satoshi.
    pub capacity_sat: u64,
    /// Announce the channel to the network.
    pub announce: bool,
    /// Fee rate for the funding transaction, sat/vbyte.
    pub feerate_sat_vb: u64,
}

/// Everything bipnip asks of a Lightning node (`docs/decisions/0006-lightning-backend.md`).
#[async_trait]
pub trait LightningBackend: Send + Sync {
    /// Identity, network and sync state.
    async fn node_info(&self) -> Result<NodeInfo, BackendError>;

    /// The node's `signmessage` over `msg`: zbase32 of `31 + recid ‖ compact sig` over
    /// `sha256d("Lightning Signed Message:" ‖ msg)` (Eclair-style hex is converted by the caller).
    async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError>;

    /// A hold invoice on a payment hash the caller chose (the preimage stays with whoever made
    /// the hash). Returns the BOLT 11 invoice.
    async fn add_hold_invoice(&self, req: &HoldInvoiceRequest) -> Result<String, BackendError>;

    /// Current state of the hold invoice for `payment_hash`. Implementations re-subscribe after
    /// a node restart instead of losing the invoice (Mostro issue #1078).
    async fn hold_invoice_state(&self, payment_hash: &str) -> Result<HoldState, BackendError>;

    /// Settles an accepted hold invoice with its 32-byte preimage (64 lower-case hex).
    async fn settle_hold_invoice(&self, preimage: &str) -> Result<(), BackendError>;

    /// Cancels a hold invoice, refunding the payer.
    async fn cancel_hold_invoice(&self, payment_hash: &str) -> Result<(), BackendError>;

    /// Pays a BOLT 11 invoice; returns the preimage once paid.
    async fn pay_invoice(&self, bolt11: &str, max_fee_msat: u64) -> Result<String, BackendError>;

    /// A plain BOLT 11 invoice for `amount_msat` (the node picks the preimage).
    async fn add_invoice(
        &self,
        amount_msat: u64,
        expiry_secs: u32,
        description: &str,
    ) -> Result<String, BackendError>;

    /// Connects to a peer.
    async fn connect(&self, remote_pubkey: &str, remote_addr: &str) -> Result<(), BackendError>;

    /// Opens a channel from the node's own wallet; returns the funding outpoint once broadcast.
    async fn open_channel(&self, req: &OpenRequest) -> Result<String, BackendError>;

    /// Pending and open channels with `remote_pubkey`.
    async fn channels_with(&self, remote_pubkey: &str) -> Result<Vec<ChannelView>, BackendError>;

    /// What the node's graph knows about another node: number of public channels and their
    /// total capacity in satoshi, or `None` if the node is not in the graph.
    async fn graph_node(&self, node_pubkey: &str) -> Result<Option<(u32, u64)>, BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_without_secrets() {
        let e = BackendError::Unsupported("hold invoices");
        assert_eq!(e.to_string(), "not supported by this backend: hold invoices");
    }

    #[test]
    fn trait_is_object_safe() {
        fn takes(_: &dyn LightningBackend) {}
        let _ = takes;
    }
}
