use anchor_lang::prelude::*;

#[event]
pub struct GameCreated {
    pub game: Pubkey,
    pub host: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub host_side: u8,
}

#[event]
pub struct GameJoined {
    pub game: Pubkey,
    pub joiner: Pubkey,
    pub vrf_request: Pubkey,
}

#[event]
pub struct GameSettled {
    pub game: Pubkey,
    pub winner: Pubkey,
    pub mint: Pubkey,
    pub outcome: u8,
    pub pot: u64,
    pub fee: u64,
}

#[event]
pub struct GameCancelled {
    pub game: Pubkey,
}

#[event]
pub struct GameRefunded {
    pub game: Pubkey,
}
