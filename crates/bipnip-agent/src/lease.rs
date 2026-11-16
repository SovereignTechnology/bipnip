//! A liquidity lease, from each side (`docs/decisions/0003-non-custodial-lease-payment.md`,
//! `docs/nips/liquidity-leases.md`).
//!
//! The buyer generates the preimage and gives out only its hash. The seller's own node creates a
//! hold invoice on that hash; the buyer checks the invoice and pays (the payment is locked, not
//! settled). The seller opens the channel only once the payment is locked. The buyer's own node
//! then checks the channel, and only a passed check lets the preimage out.
//!
//! The two orderings that protect the money are enforced by types, not by convention:
//! - [`BuyerLease::release`] needs a [`VerifiedChannel`], which only
//!   [`BuyerLease::verify_channel`] can make, from the buyer's own node's view of the channel;
//! - [`SellerLease::open`] refuses unless the seller's own node reports the hold invoice accepted.
//!
//! Nothing here logs. The preimage is wiped on drop and never appears in `Debug`. It leaves memory
//! in two ways only: through [`BuyerLease::release`], and into the owner-only file
//! [`BuyerLease::save_new`] writes so that a restarted agent can still finish the lease.

pub mod link;
pub mod run;
pub mod store;

use std::path::Path;
use std::str::FromStr;

use bipnip_ln::{
    BackendError, ChannelView, HoldInvoiceRequest, HoldState, LightningBackend, OpenRequest,
    PaymentState, hexutil,
};
use lightning_invoice::{Bolt11Invoice, Currency};
use mostro_core::lease::{LeaseContract, LeaseError, LeaseTerms, MAX_HOLD_SECS};
use mostro_core::node_binding::Network;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::secretfile;
use store::StoreError;

/// Largest `min_final_cltv_expiry_delta` a buyer accepts on the upfront invoice by default: about
/// two days of blocks, in line with the 48-hour hold limit.
pub const DEFAULT_MAX_CLTV_DELTA: u32 = 288;
/// The `min_final_cltv_expiry_delta` a seller asks for by default: one day of blocks, room to
/// open the channel and for the buyer to wait for confirmations.
pub const DEFAULT_SELLER_CLTV_DELTA: u32 = 144;
/// Blocks a seller needs on top of the buyer's confirmation wait: time to open the channel, for
/// its first confirmation, and for the node to settle before it gives a held payment back. A hold
/// invoice with less room could expire after the channel is open and before the preimage arrives.
pub const SELLER_SETTLE_MARGIN_BLOCKS: u32 = 36;

/// Why a lease step was refused.
#[derive(Debug)]
pub enum LeaseStepError {
    /// The terms or the contract are invalid.
    Contract(LeaseError),
    /// The node could not be asked, or refused.
    Node(BackendError),
    /// This agent's node is not the party the contract names.
    NotOurLease(&'static str),
    /// The seller's invoice does not match the contract; the reason says how.
    Invoice(String),
    /// The seller's node does not hold the buyer's payment yet (or any more).
    NotLocked(HoldState),
    /// The channel the buyer's node sees does not match the contract; the reason says how.
    Channel(String),
    /// The preimage offered does not hash to the contract's payment hash.
    WrongPreimage,
    /// The hold invoice would not outlast the buyer's confirmation wait.
    HoldWindow {
        /// The final CLTV delta asked for.
        cltv_delta: u32,
        /// The smallest delta this contract needs.
        needed: u32,
    },
    /// The operating system gave no randomness.
    Randomness,
}

impl std::fmt::Display for LeaseStepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseStepError::Contract(e) => write!(f, "{e}"),
            LeaseStepError::Node(e) => write!(f, "{e}"),
            LeaseStepError::NotOurLease(why) => write!(f, "not our lease: {why}"),
            LeaseStepError::Invoice(why) => write!(f, "the seller's invoice is refused: {why}"),
            LeaseStepError::NotLocked(state) => {
                write!(
                    f,
                    "the buyer's payment is not locked (invoice state {state:?})"
                )
            }
            LeaseStepError::Channel(why) => write!(f, "the channel is refused: {why}"),
            LeaseStepError::WrongPreimage => write!(f, "the preimage does not match the contract"),
            LeaseStepError::HoldWindow { cltv_delta, needed } => write!(
                f,
                "a hold invoice of {cltv_delta} blocks could expire before the preimage arrives; \
                 this contract needs at least {needed}"
            ),
            LeaseStepError::Randomness => write!(f, "no randomness available"),
        }
    }
}

impl std::error::Error for LeaseStepError {}

impl From<LeaseError> for LeaseStepError {
    fn from(e: LeaseError) -> Self {
        LeaseStepError::Contract(e)
    }
}

impl From<BackendError> for LeaseStepError {
    fn from(e: BackendError) -> Self {
        LeaseStepError::Node(e)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hexutil::encode(&Sha256::digest(bytes))
}

fn currency_of(network: Network) -> Option<Currency> {
    match network {
        Network::Bitcoin => Some(Currency::Bitcoin),
        Network::Testnet | Network::Testnet4 => Some(Currency::BitcoinTestnet),
        Network::Signet => Some(Currency::Signet),
        Network::Regtest => Some(Currency::Regtest),
    }
}

/// Checks the seller's upfront invoice against the contract, before any money moves: the payee is
/// the seller's node, the payment hash is the buyer's, the amount is exactly the upfront share,
/// the network matches, and the expiry and final CLTV delta are within limits. The invoice's own
/// signature is checked by the parser.
pub fn check_upfront_invoice(
    bolt11: &str,
    contract: &LeaseContract,
    max_cltv_delta: u32,
) -> Result<(), LeaseStepError> {
    let refuse = |why: &str| Err(LeaseStepError::Invoice(why.to_string()));
    let invoice = match Bolt11Invoice::from_str(bolt11) {
        Ok(invoice) => invoice,
        Err(_) => return refuse("not a valid BOLT 11 invoice"),
    };
    if invoice.get_payee_pub_key().to_string() != contract.seller_node.to_string() {
        return refuse("it pays a different node than the seller");
    }
    if invoice.payment_hash().to_string() != contract.payment_hash {
        return refuse("its payment hash is not ours");
    }
    let upfront_msat = contract
        .terms
        .upfront_sat()?
        .checked_mul(1000)
        .ok_or(LeaseError::Fee("upfront overflow"))?;
    if invoice.amount_milli_satoshis() != Some(upfront_msat) {
        return refuse("its amount is not the agreed upfront share");
    }
    if Some(invoice.currency()) != currency_of(contract.network) {
        return refuse("it is for another network");
    }
    if invoice.expiry_time().as_secs() > u64::from(MAX_HOLD_SECS) {
        return refuse("it stays payable longer than 48 hours");
    }
    if invoice.min_final_cltv_expiry_delta() > u64::from(max_cltv_delta) {
        return refuse("it could hold our payment for too many blocks");
    }
    Ok(())
}

/// Checks a channel, as the buyer's own node reports it, against the contract and the funding
/// outpoint the seller announced.
pub fn check_channel(
    view: &ChannelView,
    contract: &LeaseContract,
    funding_outpoint: &str,
) -> Result<(), LeaseStepError> {
    let refuse = |why: String| Err(LeaseStepError::Channel(why));
    let terms = &contract.terms;
    if view.remote_pubkey != contract.seller_node.to_string() {
        return refuse("it is with a different node than the seller".into());
    }
    if view.funding_outpoint != funding_outpoint {
        return refuse("it is not the funding outpoint the seller reported".into());
    }
    if view.local_initiator {
        return refuse("we funded it ourselves".into());
    }
    if view.capacity_sat < terms.capacity_sat {
        return refuse(format!(
            "capacity {} sat is below the agreed {} sat",
            view.capacity_sat, terms.capacity_sat
        ));
    }
    if view.announced != terms.announce {
        return refuse(
            if terms.announce {
                "it is unannounced, the lease is for an announced channel"
            } else {
                "it is announced, the lease is for an unannounced channel"
            }
            .into(),
        );
    }
    if view.confirmations < terms.min_confirmations {
        return refuse(format!(
            "{} of {} confirmations",
            view.confirmations, terms.min_confirmations
        ));
    }
    if !view.active {
        return refuse("it is not active yet".into());
    }
    Ok(())
}

/// Proof that the buyer's own node saw the leased channel and it matched the contract. Only
/// [`BuyerLease::verify_channel`] creates one.
#[derive(Debug)]
pub struct VerifiedChannel {
    lease_id: String,
    funding_outpoint: String,
}

impl VerifiedChannel {
    /// The funding outpoint that was verified.
    pub fn funding_outpoint(&self) -> &str {
        &self.funding_outpoint
    }
}

/// First line of a stored buyer lease.
const BUYER_LEASE_HEADER: &str = "bipnip-buyer-lease:v1";

/// The buyer's side of one lease: the contract and the preimage only the buyer knows.
pub struct BuyerLease {
    contract: LeaseContract,
    preimage: [u8; 32],
}

impl std::fmt::Debug for BuyerLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BuyerLease({}, <preimage redacted>)", self.contract.id())
    }
}

impl Drop for BuyerLease {
    fn drop(&mut self) {
        self.preimage.zeroize();
    }
}

impl BuyerLease {
    /// Starts a lease with `seller_node` (66 hex) on the buyer's own node: draws a fresh preimage,
    /// and makes the contract with its hash, dated `now` (unix seconds).
    pub async fn start(
        backend: &dyn LightningBackend,
        seller_node: &str,
        network: Network,
        terms: LeaseTerms,
        now: u64,
    ) -> Result<Self, LeaseStepError> {
        let info = backend.node_info().await?;
        if info.network != network.as_str() {
            return Err(LeaseStepError::NotOurLease(
                "our node is on another network",
            ));
        }
        let key = |hex: &str, what: &'static str| {
            hexutil::node_key(hex)
                .ok()
                .and_then(|bytes| secp256k1::PublicKey::from_slice(&bytes).ok())
                .ok_or(LeaseStepError::NotOurLease(what))
        };
        let mut preimage = [0u8; 32];
        getrandom::fill(&mut preimage).map_err(|_| LeaseStepError::Randomness)?;
        let lease = BuyerLease {
            contract: LeaseContract {
                network,
                seller_node: key(seller_node, "the seller's node key is malformed")?,
                buyer_node: key(&info.node_pubkey, "our node's key is malformed")?,
                terms,
                payment_hash: sha256_hex(&preimage),
                created_at: now,
            },
            preimage,
        };
        lease.contract.validate()?;
        Ok(lease)
    }

    /// Writes the contract and the preimage to a new file only its owner can read. Nothing is
    /// ever overwritten: a lease has one preimage for life. This must happen before the take is
    /// sent, or a restart between the two leaves a paid lease nobody can release.
    pub fn save_new(&self, path: &Path) -> Result<(), StoreError> {
        let mut contents = format!(
            "{BUYER_LEASE_HEADER}\n{}\n{}\n",
            self.contract.text(),
            hexutil::encode(&self.preimage)
        );
        let written = secretfile::create_secret(path, &contents);
        contents.zeroize();
        written.map_err(StoreError::File)
    }

    /// Reads a lease [`BuyerLease::save_new`] wrote, refusing a file anyone else can read and a
    /// preimage that does not open the contract's payment hash.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        let mut contents = secretfile::read_secret(path).map_err(StoreError::File)?;
        let lease = Self::parse_stored(&contents);
        contents.zeroize();
        lease
    }

    fn parse_stored(contents: &str) -> Result<Self, StoreError> {
        let corrupt = |what: &str| StoreError::Corrupt(format!("buyer lease file: {what}"));
        let mut lines = contents.lines();
        if lines.next() != Some(BUYER_LEASE_HEADER) {
            return Err(corrupt("unknown format"));
        }
        let contract = lines
            .next()
            .and_then(|text| LeaseContract::parse(text).ok())
            .ok_or_else(|| corrupt("contract"))?;
        let mut bytes = lines
            .next()
            .and_then(|hex| hexutil::hash32(hex, "preimage").ok())
            .ok_or_else(|| corrupt("preimage"))?;
        let opens = sha256_hex(&bytes) == contract.payment_hash;
        let mut preimage = [0u8; 32];
        if opens && bytes.len() == preimage.len() {
            preimage.copy_from_slice(&bytes);
        }
        bytes.zeroize();
        if !opens || lines.next().is_some() {
            preimage.zeroize();
            return Err(corrupt("the preimage does not open the contract"));
        }
        Ok(BuyerLease { contract, preimage })
    }

    /// The contract to send to the seller. It carries the payment hash, never the preimage.
    pub fn contract(&self) -> &LeaseContract {
        &self.contract
    }

    /// Checks the seller's invoice against the contract and, only if it matches, starts paying it.
    /// The expected result is [`PaymentState::InFlight`]: the payment is locked at the seller's
    /// node until the preimage is released, and comes back if it never is.
    pub async fn pay_upfront(
        &self,
        backend: &dyn LightningBackend,
        bolt11: &str,
        max_fee_msat: u64,
        max_cltv_delta: u32,
    ) -> Result<PaymentState, LeaseStepError> {
        check_upfront_invoice(bolt11, &self.contract, max_cltv_delta)?;
        Ok(backend.pay_invoice(bolt11, max_fee_msat, 10).await?)
    }

    /// Where the upfront payment stands, from the buyer's node.
    pub async fn payment_state(
        &self,
        backend: &dyn LightningBackend,
    ) -> Result<PaymentState, LeaseStepError> {
        Ok(backend.payment_state(&self.contract.payment_hash).await?)
    }

    /// Asks the buyer's **own node** about its channels with the seller and checks the one at
    /// `funding_outpoint` against the contract. The seller's word is only used to pick which
    /// channel to look at.
    pub async fn verify_channel(
        &self,
        backend: &dyn LightningBackend,
        funding_outpoint: &str,
    ) -> Result<VerifiedChannel, LeaseStepError> {
        let seller = self.contract.seller_node.to_string();
        let channels = backend.channels_with(&seller).await?;
        let Some(view) = channels
            .iter()
            .find(|c| c.funding_outpoint == funding_outpoint)
        else {
            return Err(LeaseStepError::Channel(
                "our node has no channel at that funding outpoint".into(),
            ));
        };
        check_channel(view, &self.contract, funding_outpoint)?;
        Ok(VerifiedChannel {
            lease_id: self.contract.id(),
            funding_outpoint: funding_outpoint.to_string(),
        })
    }

    /// Hands out the preimage (64 hex) for the seller to settle with. Needs the proof that this
    /// lease's channel was verified; a proof from another lease is refused.
    pub fn release(&self, proof: &VerifiedChannel) -> Result<String, LeaseStepError> {
        if proof.lease_id != self.contract.id() {
            return Err(LeaseStepError::NotOurLease(
                "the channel proof belongs to another lease",
            ));
        }
        Ok(hexutil::encode(&self.preimage))
    }
}

/// The seller's side of one lease.
#[derive(Debug, Clone)]
pub struct SellerLease {
    contract: LeaseContract,
}

impl SellerLease {
    /// Accepts a buyer's contract (canonical text) if it is valid and names this node as the
    /// seller on its network. Whether the terms are ones the seller offered is the caller's check.
    pub async fn accept(
        backend: &dyn LightningBackend,
        contract_text: &str,
    ) -> Result<Self, LeaseStepError> {
        let contract = LeaseContract::parse(contract_text)?;
        let info = backend.node_info().await?;
        if info.network != contract.network.as_str() {
            return Err(LeaseStepError::NotOurLease(
                "our node is on another network",
            ));
        }
        if info.node_pubkey != contract.seller_node.to_string() {
            return Err(LeaseStepError::NotOurLease(
                "it names another node as the seller",
            ));
        }
        Ok(SellerLease { contract })
    }

    /// A lease read back from this agent's own state: the contract was accepted before it was
    /// stored, so only its form is checked again.
    pub(crate) fn restore(contract_text: &str) -> Result<Self, LeaseError> {
        Ok(SellerLease {
            contract: LeaseContract::parse(contract_text)?,
        })
    }

    /// The contract.
    pub fn contract(&self) -> &LeaseContract {
        &self.contract
    }

    /// Has the seller's own node create the hold invoice for the upfront share, on the buyer's
    /// payment hash. The seller cannot settle it: it does not know the preimage.
    ///
    /// Refuses a `cltv_delta` below the contract's confirmation wait plus
    /// [`SELLER_SETTLE_MARGIN_BLOCKS`]: the payment would go back to the buyer while the seller is
    /// still waiting for the preimage, with the channel already open.
    pub async fn upfront_invoice(
        &self,
        backend: &dyn LightningBackend,
        cltv_delta: u32,
    ) -> Result<String, LeaseStepError> {
        let needed = self
            .contract
            .terms
            .min_confirmations
            .saturating_add(SELLER_SETTLE_MARGIN_BLOCKS);
        if cltv_delta < needed {
            return Err(LeaseStepError::HoldWindow { cltv_delta, needed });
        }
        let amount_msat = self
            .contract
            .terms
            .upfront_sat()?
            .checked_mul(1000)
            .ok_or(LeaseError::Fee("upfront overflow"))?;
        let request = HoldInvoiceRequest {
            payment_hash: self.contract.payment_hash.clone(),
            amount_msat,
            expiry_secs: MAX_HOLD_SECS,
            cltv_delta,
            description: format!("bipnip lease {}", &self.contract.id()[..16]),
        };
        Ok(backend.add_hold_invoice(&request).await?)
    }

    /// Whether the buyer's payment is locked at the seller's node.
    pub async fn payment_locked(
        &self,
        backend: &dyn LightningBackend,
    ) -> Result<bool, LeaseStepError> {
        let state = backend
            .hold_invoice_state(&self.contract.payment_hash)
            .await?;
        Ok(state == HoldState::Accepted)
    }

    /// Opens the leased channel to the buyer and returns the funding outpoint. Refuses unless the
    /// seller's own node reports the buyer's payment locked: no channel is funded on a promise.
    pub async fn open(
        &self,
        backend: &dyn LightningBackend,
        buyer_addr: Option<String>,
        feerate_sat_vb: u64,
    ) -> Result<String, LeaseStepError> {
        let state = backend
            .hold_invoice_state(&self.contract.payment_hash)
            .await?;
        if state != HoldState::Accepted {
            return Err(LeaseStepError::NotLocked(state));
        }
        let request = OpenRequest {
            remote_pubkey: self.contract.buyer_node.to_string(),
            remote_addr: buyer_addr,
            capacity_sat: self.contract.terms.capacity_sat,
            announce: self.contract.terms.announce,
            feerate_sat_vb,
        };
        Ok(backend.open_channel(&request).await?)
    }

    /// Settles the hold invoice with the preimage the buyer released, after checking it is this
    /// lease's preimage.
    pub async fn settle(
        &self,
        backend: &dyn LightningBackend,
        preimage: &str,
    ) -> Result<(), LeaseStepError> {
        let bytes =
            hexutil::hash32(preimage, "preimage").map_err(|_| LeaseStepError::WrongPreimage)?;
        if sha256_hex(&bytes) != self.contract.payment_hash {
            return Err(LeaseStepError::WrongPreimage);
        }
        Ok(backend.settle_hold_invoice(preimage).await?)
    }

    /// Cancels the hold invoice: the buyer's locked payment goes back at once.
    pub async fn cancel(&self, backend: &dyn LightningBackend) -> Result<(), LeaseStepError> {
        Ok(backend
            .cancel_hold_invoice(&self.contract.payment_hash)
            .await?)
    }
}

#[cfg(test)]
mod tests;
