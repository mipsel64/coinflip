use anchor_lang::prelude::*;

use crate::{constants::BPS_DENOMINATOR, errors::CoinflipError};

/// Protocol fee on the whole pot, rounded DOWN (in the winner's favor —
/// explicit decision recorded in the design spec).
pub fn fee_amount(pot: u64, fee_bps: u16) -> Result<u64> {
    require!(fee_bps <= BPS_DENOMINATOR, CoinflipError::NumericalOverflow);
    // cannot overflow: u64::MAX * u16::MAX < 2^81, far below u128::MAX
    let fee = (pot as u128)
        .checked_mul(fee_bps as u128)
        .ok_or_else(|| error!(CoinflipError::NumericalOverflow))?
        / BPS_DENOMINATOR as u128;
    u64::try_from(fee).map_err(|_| error!(CoinflipError::NumericalOverflow))
}

/// Nominal pot from a per-player stake. NOT the settlement pot: settlement
/// must read the escrow's actual token balance, which is authoritative.
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

    #[test]
    fn fee_scales_with_bps() {
        assert_eq!(fee_amount(1_000_000, 250).unwrap(), 25_000);
        assert_eq!(fee_amount(1_000_000, 1_000).unwrap(), 100_000); // MAX_FEE_BPS
        assert_eq!(fee_amount(1_000_000, 0).unwrap(), 0);
    }

    #[test]
    fn max_pot_boundary() {
        assert_eq!(fee_amount(u64::MAX, 10_000).unwrap(), u64::MAX); // exactly tight
        assert!(fee_amount(u64::MAX, 10_001).is_err()); // precondition guard fires
                                                        // small pot: try_from would NOT catch this, only the require! guard does
        assert!(fee_amount(100, 20_000).is_err());
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

        /// Pins floor division exactly: fee*10_000 <= pot*bps < (fee+1)*10_000
        #[test]
        fn fee_is_exact_floor_division(pot in prop_oneof![1u64..1_000_000u64, 0u64..], bps in 0u16..=10_000) {
            let fee = fee_amount(pot, bps).unwrap() as u128;
            let exact = (pot as u128) * (bps as u128);
            prop_assert!(fee * 10_000 <= exact);
            prop_assert!(exact < (fee + 1) * 10_000);
        }

        #[test]
        fn payout_plus_fee_equals_pot(pot in prop_oneof![1u64..1_000_000u64, 0u64..], bps in 0u16..=10_000) {
            let fee = fee_amount(pot, bps).unwrap();
            let payout = pot.checked_sub(fee).unwrap();
            prop_assert_eq!(payout + fee, pot);
        }
    }
}
