use anchor_lang::prelude::*;
use static_assertions::const_assert_eq;

use crate::{constants::MAX_FEE_BPS, errors::CoinflipError};

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub version: u8,
    pub bump: u8,
    /// Can call update_config.
    pub admin: Pubkey,
    /// Authority whose token accounts receive fees.
    pub treasury: Pubkey,
    /// Fee on the pot, in basis points. Capped at MAX_FEE_BPS.
    pub fee_bps: u16,
    /// Slots after join before refund_timeout is allowed.
    pub refund_timeout_slots: u64,
    pub _reserved: [u8; 64],
}

const_assert_eq!(Config::INIT_SPACE, 1 + 1 + 32 + 32 + 2 + 8 + 64);

impl Config {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn validate_fee(fee_bps: u16) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, CoinflipError::FeeTooHigh);
        Ok(())
    }
}
