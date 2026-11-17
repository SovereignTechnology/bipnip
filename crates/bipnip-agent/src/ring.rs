//! A liquidity ring, from one member's side (`docs/nips/liquidity-rings.md`): every member opens
//! one channel to the next and receives one from the previous. No money changes hands, except a
//! bond when the ring asks for one ([`bond`]).

pub mod bond;
pub mod cmd;
pub mod link;
pub mod run;
pub mod store;

#[cfg(test)]
mod tests;
