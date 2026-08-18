use anchor_lang::prelude::*;

use crate::{constants::BPS_DENOMINATOR, errors::CoinflipError};

/// Protocol fee on the whole pot, rounded DOWN (in the winner's favor —
/// explicit decision recorded in the design spec).
pub fn fee_amount(pot: u64, fee_bps: u16) -> Result<u64> {
    let fee = (pot as u128)
        .checked_mul(fee_bps as u128)
        .ok_or(CoinflipError::NumericalOverflow)?
        / BPS_DENOMINATOR as u128;
    u64::try_from(fee).map_err(|_| error!(CoinflipError::NumericalOverflow))
}

pub fn pot_amount(stake: u64) -> Result<u64> {
    stake
        .checked_mul(2)
        .ok_or_else(|| error!(CoinflipError::NumericalOverflow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fee_is_one_percent_of_10_sol() {
        // 10 SOL pot, 1% fee => 0.1 SOL fee (the spec's example)
        assert_eq!(fee_amount(10_000_000_000, 100).unwrap(), 100_000_000);
    }

    #[test]
    fn fee_rounds_down_in_winners_favor() {
        assert_eq!(fee_amount(99, 100).unwrap(), 0);
        assert_eq!(fee_amount(199, 100).unwrap(), 1);
    }

    #[test]
    fn pot_overflow_is_an_error() {
        assert!(pot_amount(u64::MAX).is_err());
        assert_eq!(pot_amount(5).unwrap(), 10);
    }

    proptest! {
        #[test]
        fn fee_never_exceeds_pot(pot in 0u64.., bps in 0u16..=10_000) {
            prop_assert!(fee_amount(pot, bps).unwrap() <= pot);
        }

        #[test]
        fn fee_is_monotonic_in_bps(pot in 0u64.., bps in 0u16..1_000) {
            prop_assert!(fee_amount(pot, bps).unwrap() <= fee_amount(pot, bps + 1).unwrap());
        }
    }
}
