use anchor_lang::prelude::*;
use orao_solana_vrf::state::RandomnessV2;

pub mod cancel_game;
pub mod create_game;
pub mod initialize_config;
pub mod join_game;
pub mod refund_timeout;
pub mod settle;
pub mod settlement;
pub mod update_config;

pub use cancel_game::*;
pub use create_game::*;
pub use initialize_config::*;
pub use join_game::*;
pub use refund_timeout::*;
pub use settle::*;
pub use update_config::*;

/// Rent for an ORAO request account at its FULFILLED size — Anchor's 8-byte
/// discriminator plus `RandomnessV2::FULFILLED_SIZE` (137 bytes today), which
/// is how ORAO itself sizes the account it shrinks to at fulfillment.
///
/// ORAO allocates the larger pending size at request time and returns the
/// freed rent to the request's client (our joiner) when it fulfills, so this
/// is the only part of that rent the joiner never gets back: `create_game`
/// bonds it and `join_game` records it.
pub(crate) fn fulfilled_request_rent() -> Result<u64> {
    Ok(Rent::get()?.minimum_balance(8 + RandomnessV2::FULFILLED_SIZE))
}
