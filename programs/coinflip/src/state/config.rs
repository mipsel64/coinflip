use anchor_lang::prelude::*;
use static_assertions::const_assert_eq;

use crate::{
    constants::{MAX_FEE_BPS, MAX_REFUND_TIMEOUT_SLOTS, MIN_REFUND_TIMEOUT_SLOTS},
    errors::CoinflipError,
};

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub version: u8,
    pub bump: u8,
    /// Can call update_config.
    pub admin: Pubkey,
    /// Fee on the pot, in basis points. Capped at MAX_FEE_BPS.
    pub fee_bps: u16,
    /// Slots after join before refund_timeout is allowed.
    pub refund_timeout_slots: u64,
    pub _reserved: [u8; 64],
}

const_assert_eq!(Config::INIT_SPACE, 1 + 1 + 32 + 2 + 8 + 64);

impl Config {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn validate_fee(fee_bps: u16) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, CoinflipError::FeeTooHigh);
        Ok(())
    }

    pub fn validate_timeout(refund_timeout_slots: u64) -> Result<()> {
        require!(
            (MIN_REFUND_TIMEOUT_SLOTS..=MAX_REFUND_TIMEOUT_SLOTS).contains(&refund_timeout_slots),
            CoinflipError::InvalidTimeout
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use anchor_lang::AnchorSerialize;

    use super::*;

    #[test]
    fn fee_cap_boundary() {
        assert!(Config::validate_fee(1_000).is_ok());
        assert!(Config::validate_fee(1_001).is_err());
    }

    #[test]
    fn timeout_bounds() {
        assert!(Config::validate_timeout(17_999).is_err());
        assert!(Config::validate_timeout(18_000).is_ok());
        assert!(Config::validate_timeout(10_000_000).is_ok());
        assert!(Config::validate_timeout(10_000_001).is_err());
    }

    #[test]
    fn layout_is_pinned() {
        let config = Config {
            version: Config::LAYOUT_VERSION,
            bump: 255,
            admin: Pubkey::new_unique(),
            fee_bps: 100,
            refund_timeout_slots: 1_000,
            _reserved: [0; 64],
        };
        let bytes = config.try_to_vec().unwrap();
        assert_eq!(bytes.len(), Config::INIT_SPACE);
    }
}
