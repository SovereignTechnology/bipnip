//! Prints a fresh throwaway Nostr key for the instance in the end-to-end run, as
//! `<nsec> <public key hex>`. For regtest runs inside the test VM only.

use nostr_sdk::prelude::{Keys, ToBech32};

fn main() {
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32().expect("bech32");
    println!("{nsec} {}", keys.public_key().to_hex());
}
