use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod math;
pub mod state;

declare_id!("7ZsoAuFYBBtTHt3jeCd8wWZvKcp7sqxEqAPscSNFte1n");

#[program]
pub mod coinflip {}
