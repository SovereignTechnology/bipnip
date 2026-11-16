//! The agent's keys, derived the way every Mostro client derives them, so a mnemonic restores the
//! same identity and trades in bipnip, mostro-cli, mostrix or the Mostro app.
//!
//! NIP-06 path `m/44'/1237'/38383'/0/N`: `N = 0` is the identity key (reputation); `N ≥ 1` are
//! per-trade keys, one per order, so trades cannot be linked to each other by key.
//!
//! The mnemonic is the one secret behind all of them. [`Wallet`] never prints it, wipes it when
//! dropped, and nothing in this module logs.

use mostro_core::prelude::NOSTR_ORDER_EVENT_KIND;
use nostr::nips::nip06::FromMnemonic;
use nostr_sdk::prelude::Keys;
use zeroize::Zeroize;

/// Largest trade index: BIP-32 non-hardened children stop below 2^31.
pub const MAX_TRADE_INDEX: u32 = (1 << 31) - 1;

/// Why keys could not be derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Not a valid BIP-39 English mnemonic. The words are never echoed.
    Mnemonic,
    /// Index 0 is the identity key, and indexes stop at [`MAX_TRADE_INDEX`].
    TradeIndex(u32),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Mnemonic => write!(f, "not a valid BIP-39 mnemonic"),
            KeyError::TradeIndex(i) => {
                write!(f, "trade index {i} is outside 1..={MAX_TRADE_INDEX}")
            }
        }
    }
}

impl std::error::Error for KeyError {}

/// A validated mnemonic and the keys that come from it.
pub struct Wallet {
    mnemonic: String,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wallet(<mnemonic redacted>)")
    }
}

impl Drop for Wallet {
    fn drop(&mut self) {
        self.mnemonic.zeroize();
    }
}

fn derive(mnemonic: &str, index: u32) -> Result<Keys, KeyError> {
    Keys::from_mnemonic_advanced(
        mnemonic,
        None,
        Some(u32::from(NOSTR_ORDER_EVENT_KIND)),
        Some(0),
        Some(index),
    )
    .map_err(|_| KeyError::Mnemonic)
}

impl Wallet {
    /// Takes ownership of a mnemonic after checking it derives keys (words and checksum valid).
    /// Surrounding whitespace is ignored.
    pub fn from_mnemonic(mut mnemonic: String) -> Result<Self, KeyError> {
        let trimmed = mnemonic.split_whitespace().collect::<Vec<_>>().join(" ");
        mnemonic.zeroize();
        match derive(&trimmed, 0) {
            Ok(_) => Ok(Wallet { mnemonic: trimmed }),
            Err(e) => {
                let mut trimmed = trimmed;
                trimmed.zeroize();
                Err(e)
            }
        }
    }

    /// The identity key (`N = 0`): what reputation attaches to.
    pub fn identity(&self) -> Result<Keys, KeyError> {
        derive(&self.mnemonic, 0)
    }

    /// The key for trade `index` (`1..=MAX_TRADE_INDEX`).
    pub fn trade(&self, index: u32) -> Result<Keys, KeyError> {
        if index == 0 || index > MAX_TRADE_INDEX {
            return Err(KeyError::TradeIndex(index));
        }
        derive(&self.mnemonic, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NIP-06's own test vector (path m/44'/1237'/0'/0/0).
    const NIP06_MNEMONIC: &str =
        "leader monkey parrot ring guide accident before fence cannon height naive bean";
    const NIP06_SECRET: &str = "7f7ff03d123792d6ac594bfa67bf6d0c0ab55b6b1fdb6249303fe861f1ccba9a";

    #[test]
    fn the_derivation_family_matches_nip06() {
        let keys = Keys::from_mnemonic_advanced(NIP06_MNEMONIC, None, Some(0), Some(0), Some(0))
            .expect("derives");
        assert_eq!(keys.secret_key().to_secret_hex(), NIP06_SECRET);
    }

    #[test]
    fn identity_and_trade_keys_use_the_mostro_account() {
        let wallet = Wallet::from_mnemonic(NIP06_MNEMONIC.to_string()).expect("wallet");
        let identity = wallet.identity().expect("identity");
        // Not the NIP-06 default account: Mostro's account is 38383.
        assert_ne!(identity.secret_key().to_secret_hex(), NIP06_SECRET);
        let expected =
            Keys::from_mnemonic_advanced(NIP06_MNEMONIC, None, Some(38383), Some(0), Some(0))
                .expect("derives");
        assert_eq!(identity.public_key(), expected.public_key());

        let (t1, t2) = (wallet.trade(1).expect("t1"), wallet.trade(2).expect("t2"));
        assert_ne!(t1.public_key(), identity.public_key());
        assert_ne!(t1.public_key(), t2.public_key());
        assert_eq!(
            t1.public_key(),
            wallet.trade(1).expect("t1 again").public_key()
        );
    }

    #[test]
    fn trade_index_bounds() {
        let wallet = Wallet::from_mnemonic(NIP06_MNEMONIC.to_string()).expect("wallet");
        assert_eq!(wallet.trade(0).err(), Some(KeyError::TradeIndex(0)));
        assert!(wallet.trade(MAX_TRADE_INDEX).is_ok());
        assert_eq!(
            wallet.trade(MAX_TRADE_INDEX + 1).err(),
            Some(KeyError::TradeIndex(MAX_TRADE_INDEX + 1))
        );
    }

    #[test]
    fn bad_mnemonics_are_refused_without_echoing_them() {
        let bad = "leader monkey parrot ring guide accident before fence cannon height naive naive";
        let err = Wallet::from_mnemonic(bad.to_string()).expect_err("refused");
        assert_eq!(err, KeyError::Mnemonic);
        assert!(!err.to_string().contains("monkey"));
        assert!(Wallet::from_mnemonic(String::new()).is_err());
    }

    #[test]
    fn whitespace_is_normalised_and_debug_is_redacted() {
        let spaced = format!("  {}\n", NIP06_MNEMONIC.replace(' ', "  "));
        let a = Wallet::from_mnemonic(spaced).expect("wallet");
        let b = Wallet::from_mnemonic(NIP06_MNEMONIC.to_string()).expect("wallet");
        assert_eq!(
            a.identity().expect("a").public_key(),
            b.identity().expect("b").public_key()
        );
        let shown = format!("{a:?}");
        assert!(!shown.contains("leader") && shown.contains("redacted"));
    }
}
