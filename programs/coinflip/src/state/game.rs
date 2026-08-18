use anchor_lang::prelude::*;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use static_assertions::const_assert_eq;

use crate::errors::CoinflipError;

/// Discriminant 0 must be the zeroed-account default meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum GameState {
    Open = 0,
    AwaitingRandomness = 1,
    Settled = 2,
    Cancelled = 3,
    Refunded = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum Side {
    Heads = 0,
    Tails = 1,
}

impl Side {
    pub fn from_randomness(randomness: &[u8; 64]) -> Self {
        if randomness[0] & 1 == 0 {
            Side::Heads
        } else {
            Side::Tails
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Game {
    pub version: u8,
    /// GameState as u8 — an account byte is untrusted input, convert via state().
    pub state: u8,
    /// Side as u8.
    pub host_side: u8,
    pub escrow_bump: u8,
    pub host: Pubkey,
    /// Pubkey::default() until joined.
    pub joiner: Pubkey,
    pub token_mint: Pubkey,
    /// Per-player stake in base units.
    pub amount: u64,
    pub host_token_account: Pubkey,
    pub joiner_token_account: Pubkey,
    pub joined_at_slot: u64,
    pub _reserved: [u8; 64],
}

const_assert_eq!(
    Game::INIT_SPACE,
    1 + 1 + 1 + 1 + 32 + 32 + 32 + 8 + 32 + 32 + 8 + 64
);

impl Game {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn state(&self) -> Result<GameState> {
        GameState::try_from(self.state).map_err(|_| error!(CoinflipError::InvalidGameState))
    }

    pub fn host_side(&self) -> Result<Side> {
        Side::try_from(self.host_side).map_err(|_| error!(CoinflipError::InvalidSide))
    }

    pub fn require_state(&self, expected: GameState) -> Result<()> {
        require!(self.state()? == expected, CoinflipError::InvalidGameState);
        Ok(())
    }

    /// Returns (winner, winner_token_account) for the flipped outcome.
    pub fn winner(&self, outcome: Side) -> Result<(Pubkey, Pubkey)> {
        if outcome == self.host_side()? {
            Ok((self.host, self.host_token_account))
        } else {
            Ok((self.joiner, self.joiner_token_account))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_bytes_round_trip() {
        for s in [
            GameState::Open,
            GameState::AwaitingRandomness,
            GameState::Settled,
            GameState::Cancelled,
            GameState::Refunded,
        ] {
            assert_eq!(GameState::try_from(u8::from(s)).unwrap(), s);
        }
        assert!(GameState::try_from(5u8).is_err());
        assert!(Side::try_from(2u8).is_err());
    }

    #[test]
    fn outcome_from_randomness_parity() {
        let mut r = [0u8; 64];
        assert_eq!(Side::from_randomness(&r), Side::Heads);
        r[0] = 1;
        assert_eq!(Side::from_randomness(&r), Side::Tails);
        r[0] = 0xFE;
        assert_eq!(Side::from_randomness(&r), Side::Heads);
    }

    #[test]
    fn winner_mapping() {
        let host = Pubkey::new_unique();
        let joiner = Pubkey::new_unique();
        let host_ta = Pubkey::new_unique();
        let joiner_ta = Pubkey::new_unique();
        let mut game = Game {
            version: 1,
            state: GameState::AwaitingRandomness.into(),
            host_side: Side::Heads.into(),
            escrow_bump: 255,
            host,
            joiner,
            token_mint: Pubkey::new_unique(),
            amount: 5,
            host_token_account: host_ta,
            joiner_token_account: joiner_ta,
            joined_at_slot: 0,
            _reserved: [0; 64],
        };
        assert_eq!(game.winner(Side::Heads).unwrap(), (host, host_ta));
        assert_eq!(game.winner(Side::Tails).unwrap(), (joiner, joiner_ta));
        game.host_side = Side::Tails.into();
        assert_eq!(game.winner(Side::Tails).unwrap(), (host, host_ta));
    }
}
