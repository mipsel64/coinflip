use anchor_lang::prelude::*;

#[constant]
pub const CONFIG_SEED: &[u8] = b"config";

#[constant]
pub const ESCROW_SEED: &[u8] = b"escrow";

/// Hard cap on the protocol fee: 10%.
#[constant]
pub const MAX_FEE_BPS: u16 = 1_000;

#[constant]
pub const BPS_DENOMINATOR: u16 = 10_000;
