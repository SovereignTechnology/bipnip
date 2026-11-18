//! The checks made before a bond is paid, and the one way it is paid. Used by a ring member
//! and by a lease's buyer.
//!
//! A bond is the one payment that goes to the instance: a hold invoice on the instance's own
//! node, cancelled when we did our part and taken when we did not. Before paying, the agent
//! checks the invoice itself, against what it already knows:
//!
//! - the payee is the node the instance's own binding names (read from the relays and verified
//!   before the ring was created or joined, or the lease taken), so a bond can only go to the
//!   instance's node;
//! - the amount is exactly the bond in the roster every member signed, or in the order the
//!   buyer took;
//! - it is for our network, has not expired (where the caller has a clock), and cannot hold
//!   our payment for more blocks than [`mostro_core::bond::MAX_BOND_CLTV_DELTA`].
//!
//! What no check here can give: an instance that takes a bond it should have returned. That is
//! a trust placed in the instance by joining a ring or taking a lease with a bond, bounded by
//! the bond's size and by the agent's own `max_bond_sat`.

use std::str::FromStr;

use bipnip_ln::{BackendError, LightningBackend, PaymentState};
use lightning_invoice::Bolt11Invoice;
use mostro_core::bond::{MAX_BOND_CLTV_DELTA, routing_reserve_sat};
use mostro_core::node_binding::Network;
use serde::{Deserialize, Serialize};

use crate::lease::currency_of;

/// How long one look at the node waits for a bond payment to lock at the instance.
const BOND_PAY_WAIT_SECS: u32 = 5;

/// Our bond: the instance's invoice, checked, and whether our payment for it is out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BondPayment {
    /// The instance's hold invoice.
    pub invoice: String,
    /// Its payment hash, 64 hex.
    pub payment_hash: String,
    /// True once our node has a payment out for it.
    pub sent: bool,
}

/// Our claim on a bond we are owed: an invoice on our own node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BondClaim {
    /// Our invoice.
    pub invoice: String,
    /// Its payment hash, 64 hex.
    pub payment_hash: String,
    /// Unix seconds after which it can no longer be paid; 0 when the caller keeps no clock.
    pub expires_at: u64,
    /// True once our node shows it paid.
    pub paid: bool,
}

/// Checks the instance's invoice for a bond of `bond_sat` on `network`. `instance_node` is the
/// instance's node key, 66 hex. With `now` (unix seconds) an expired invoice is refused too;
/// without it, an expired invoice is simply one our node cannot pay. Returns the payment hash.
pub fn check_bond_invoice(
    bolt11: &str,
    instance_node: &str,
    bond_sat: u64,
    network: Network,
    now: Option<u64>,
) -> Result<String, String> {
    let refuse = |why: &str| Err(why.to_string());
    let Ok(invoice) = Bolt11Invoice::from_str(bolt11) else {
        return refuse("the bond invoice is not a valid BOLT 11 invoice");
    };
    if bond_sat == 0 {
        return refuse("there is no bond to pay here");
    }
    if invoice.get_payee_pub_key().to_string() != instance_node {
        return refuse("the bond invoice pays a node that is not the instance's");
    }
    let Some(bond_msat) = bond_sat.checked_mul(1_000) else {
        return refuse("the bond overflows");
    };
    if invoice.amount_milli_satoshis() != Some(bond_msat) {
        return refuse("the bond invoice's amount is not the agreed bond");
    }
    if Some(invoice.currency()) != currency_of(network) {
        return refuse("the bond invoice is for another network");
    }
    if invoice.min_final_cltv_expiry_delta() > u64::from(MAX_BOND_CLTV_DELTA) {
        return refuse("the bond invoice could hold our payment for too many blocks");
    }
    let expires_at = invoice
        .duration_since_epoch()
        .as_secs()
        .saturating_add(invoice.expiry_time().as_secs());
    if now.is_some_and(|now| expires_at <= now) {
        return refuse("the bond invoice has expired");
    }
    Ok(invoice.payment_hash().to_string())
}

/// Pays a bond whose invoice passed [`check_bond_invoice`], unless a payment for it is already
/// out: our node is asked first, so a restart, or a stop between paying and saving, never pays
/// twice. Marks the bond sent once a payment is out. Returns why the payment failed, if it did;
/// it is tried again on the next look.
pub async fn pay_bond(
    backend: &dyn LightningBackend,
    bond: &mut BondPayment,
    bond_sat: u64,
) -> Result<Option<String>, BackendError> {
    if bond.sent {
        return Ok(None);
    }
    // `None` only when our node says plainly it has no payment for this hash.
    let known = backend.payment_lookup(&bond.payment_hash).await?;
    let state = match known {
        None | Some(PaymentState::Failed { .. }) => {
            let max_fee_msat = routing_reserve_sat(bond_sat).max(10).saturating_mul(1_000);
            backend
                .pay_invoice(&bond.invoice, max_fee_msat, BOND_PAY_WAIT_SECS)
                .await?
        }
        Some(state) => state,
    };
    match state {
        PaymentState::Failed { reason } => Ok(Some(reason)),
        PaymentState::InFlight | PaymentState::Succeeded { .. } => {
            bond.sent = true;
            Ok(None)
        }
    }
}

/// The payment hash of an invoice our own node just made.
pub fn payment_hash_of(bolt11: &str) -> Result<String, String> {
    Bolt11Invoice::from_str(bolt11)
        .map(|invoice| invoice.payment_hash().to_string())
        .map_err(|_| "our node returned something that is not a BOLT 11 invoice".to_string())
}
