//! The checks a member makes before it pays a ring's bond.
//!
//! A bond is the one payment in a ring that goes to the instance: a hold invoice on the
//! instance's own node, cancelled when our channel is there and taken when it is not. Before
//! paying, the member checks the invoice itself, against what it already knows:
//!
//! - the payee is the node the instance's own binding names (read from the relays and verified
//!   before the ring was created or joined), so a bond can only go to the instance's node;
//! - the amount is exactly the bond in the roster every member signed;
//! - it is for our network, has not expired, and cannot hold our payment for more blocks than
//!   [`mostro_core::bond::MAX_BOND_CLTV_DELTA`].
//!
//! What no check here can give: an instance that takes a bond it should have returned. That is
//! a trust the member places in the instance when it joins a ring with a bond, bounded by the
//! bond's size and by the member's own `max_bond_sat`.

use std::str::FromStr;

use lightning_invoice::Bolt11Invoice;
use mostro_core::bond::MAX_BOND_CLTV_DELTA;
use mostro_core::ring::RingTerms;

use crate::lease::currency_of;

/// Checks the instance's bond invoice for a ring with `terms`, at `now` (unix seconds).
/// `instance_node` is the instance's node key, 66 hex. Returns the invoice's payment hash.
pub fn check_bond_invoice(
    bolt11: &str,
    instance_node: &str,
    terms: &RingTerms,
    now: u64,
) -> Result<String, String> {
    let refuse = |why: &str| Err(why.to_string());
    let Ok(invoice) = Bolt11Invoice::from_str(bolt11) else {
        return refuse("the bond invoice is not a valid BOLT 11 invoice");
    };
    if terms.bond_sat == 0 {
        return refuse("this ring has no bond");
    }
    if invoice.get_payee_pub_key().to_string() != instance_node {
        return refuse("the bond invoice pays a node that is not the instance's");
    }
    let Some(bond_msat) = terms.bond_sat.checked_mul(1_000) else {
        return refuse("the bond overflows");
    };
    if invoice.amount_milli_satoshis() != Some(bond_msat) {
        return refuse("the bond invoice's amount is not the ring's bond");
    }
    if Some(invoice.currency()) != currency_of(terms.network) {
        return refuse("the bond invoice is for another network");
    }
    if invoice.min_final_cltv_expiry_delta() > u64::from(MAX_BOND_CLTV_DELTA) {
        return refuse("the bond invoice could hold our payment for too many blocks");
    }
    let expires_at = invoice
        .duration_since_epoch()
        .as_secs()
        .saturating_add(invoice.expiry_time().as_secs());
    if expires_at <= now {
        return refuse("the bond invoice has expired");
    }
    Ok(invoice.payment_hash().to_string())
}

/// The payment hash of an invoice our own node just made.
pub fn payment_hash_of(bolt11: &str) -> Result<String, String> {
    Bolt11Invoice::from_str(bolt11)
        .map(|invoice| invoice.payment_hash().to_string())
        .map_err(|_| "our node returned something that is not a BOLT 11 invoice".to_string())
}
