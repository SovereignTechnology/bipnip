//! A liquidity ring, from one member's side (`docs/nips/liquidity-rings.md`): every member opens
//! one channel to the next and receives one from the previous. No money changes hands.

pub mod run;

#[cfg(test)]
mod tests;
