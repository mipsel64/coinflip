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
    pub fn from_byte(byte: u8) -> Result<Self> {
        Side::try_from(byte).map_err(|_| error!(CoinflipError::InvalidSide))
    }

    /// One bit of the fulfilled randomness decides the flip. ORAO's value is
    /// the XOR of a >=2/3 quorum of oracle ed25519 signatures, so byte 0's
    /// parity is uniform for honest oracles; a griefing last-responder could
    /// grind any bit equally, so hashing all 64 bytes would buy nothing.
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
    /// Fee snapshot from Config at create — settlement uses this, so admin
    /// fee changes never retro-apply to existing games.
    pub fee_bps: u16,
    pub host_token_account: Pubkey,
    pub joiner_token_account: Pubkey,
    pub joined_at_slot: u64,
    /// sha256("coinflip-vrf-seed", game, joiner), stored at join; the ORAO
    /// request PDA derives from it. Unpredictable pre-join, so the address
    /// cannot be grief-pre-funded.
    pub vrf_seed: [u8; 32],
    pub _reserved: [u8; 30],
}

const_assert_eq!(
    Game::INIT_SPACE,
    1 + 1 + 1 + 1 + 32 + 32 + 32 + 8 + 2 + 32 + 32 + 8 + 32 + 30
);

impl Game {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn state(&self) -> Result<GameState> {
        GameState::try_from(self.state).map_err(|_| error!(CoinflipError::InvalidGameState))
    }

    pub fn host_side(&self) -> Result<Side> {
        Side::from_byte(self.host_side)
    }

    pub fn require_state(&self, expected: GameState) -> Result<()> {
        require!(self.state()? == expected, CoinflipError::InvalidGameState);
        Ok(())
    }

    /// True when the flipped outcome matches the host's chosen side.
    ///
    /// Callers pick the payout account themselves from the game's recorded
    /// fields; this returns only the decision so no pubkey re-comparison
    /// happens in the payout path.
    pub fn winner_is_host(&self, outcome: Side) -> Result<bool> {
        Ok(outcome == self.host_side()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AnchorSerialize;

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

        // On-chain ABI: the IDL doesn't carry discriminant values, so pin them.
        assert_eq!(u8::from(GameState::Open), 0);
        assert_eq!(u8::from(GameState::AwaitingRandomness), 1);
        assert_eq!(u8::from(GameState::Settled), 2);
        assert_eq!(u8::from(GameState::Cancelled), 3);
        assert_eq!(u8::from(GameState::Refunded), 4);
        assert_eq!(u8::from(Side::Heads), 0);
        assert_eq!(u8::from(Side::Tails), 1);

        assert_eq!(Side::from_byte(0).unwrap(), Side::Heads);
        assert_eq!(Side::from_byte(1).unwrap(), Side::Tails);
        assert!(Side::from_byte(2).is_err());
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

    fn sample_game() -> Game {
        Game {
            version: Game::LAYOUT_VERSION,
            state: GameState::AwaitingRandomness.into(),
            host_side: Side::Heads.into(),
            escrow_bump: 255,
            host: Pubkey::new_unique(),
            joiner: Pubkey::new_unique(),
            token_mint: Pubkey::new_unique(),
            amount: 5,
            fee_bps: 100,
            host_token_account: Pubkey::new_unique(),
            joiner_token_account: Pubkey::new_unique(),
            joined_at_slot: 0,
            vrf_seed: [0; 32],
            _reserved: [0; 30],
        }
    }

    #[test]
    fn winner_mapping() {
        let mut game = sample_game();

        game.host_side = Side::Heads.into();
        assert!(game.winner_is_host(Side::Heads).unwrap());
        assert!(!game.winner_is_host(Side::Tails).unwrap());

        game.host_side = Side::Tails.into();
        assert!(!game.winner_is_host(Side::Heads).unwrap());
        assert!(game.winner_is_host(Side::Tails).unwrap());
    }

    #[test]
    fn layout_is_pinned() {
        let game = Game {
            version: Game::LAYOUT_VERSION,
            state: GameState::Settled.into(),
            host_side: Side::Tails.into(),
            escrow_bump: 254,
            host: Pubkey::new_unique(),
            joiner: Pubkey::new_unique(),
            token_mint: Pubkey::new_unique(),
            amount: 42,
            fee_bps: 100,
            host_token_account: Pubkey::new_unique(),
            joiner_token_account: Pubkey::new_unique(),
            joined_at_slot: 123,
            vrf_seed: [9; 32],
            _reserved: [7; 30],
        };
        let bytes = game.try_to_vec().unwrap();
        assert_eq!(bytes.len(), Game::INIT_SPACE); // borsh runtime == InitSpace
        assert_eq!(bytes[1], game.state); // crank memcmp offset 9 == 8 (discriminator) + 1 (version)
        assert_eq!(bytes[2], game.host_side);
        // fee_bps sits after 4 u8s + 3 pubkeys + amount: the money path reads it
        assert_eq!(&bytes[108..110], &game.fee_bps.to_le_bytes());
    }

    #[test]
    fn helper_error_paths() {
        let mut game = sample_game();
        game.state = 9;
        assert!(game.state().is_err());
        game.host_side = 7;
        assert!(game.host_side().is_err());

        let mut game = sample_game();
        game.state = GameState::Open.into();
        assert!(game.require_state(GameState::Open).is_ok());

        game.state = GameState::Settled.into();
        assert!(game.require_state(GameState::Open).is_err());
    }
}
