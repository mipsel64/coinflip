use anchor_lang::prelude::*;

/// Append-only: error codes are ABI.
#[error_code]
#[derive(PartialEq)]
pub enum CoinflipError {
    #[msg("fee_bps exceeds MAX_FEE_BPS")]
    FeeTooHigh, // 6000
    #[msg("game is not in the required state")]
    InvalidGameState, // 6001
    #[msg("invalid side value")]
    InvalidSide, // 6002
    #[msg("bet amount must be greater than zero")]
    ZeroAmount, // 6003
    #[msg("host cannot join their own game")]
    HostCannotJoin, // 6004
    #[msg("mint or token account does not match the expected value")]
    MintMismatch, // 6005
    #[msg("mint has an unsupported extension")]
    UnsupportedMintExtension, // 6006
    #[msg("randomness request is not fulfilled yet")]
    RandomnessNotFulfilled, // 6007
    #[msg("randomness already fulfilled; call settle_fallback instead")]
    AlreadyFulfilled, // 6008
    #[msg("callback caller is not the registered VRF client")]
    UnauthorizedVrfClient, // 6009
    #[msg("refund timeout has not been reached")]
    TimeoutNotReached, // 6010
    #[msg("numerical overflow")]
    NumericalOverflow, // 6011
    #[msg("account does not match the expected authority")]
    OwnerMismatch, // 6012
    #[msg("authority pubkey cannot be the default key")]
    InvalidAuthority, // 6013
    #[msg("refund timeout is out of bounds")]
    InvalidTimeout, // 6014
}
