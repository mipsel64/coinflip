use anchor_lang::prelude::*;

#[event]
pub struct GameCreated {
    pub game: Pubkey,
    pub host: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub host_side: u8,
    /// Fee snapshot the game was created under.
    pub fee_bps: u16,
    /// Lamports the host bonded for a losing host's reimbursement of the
    /// joiner — the cap on what a joiner can be paid back (see `settle`).
    pub bond_lamports: u64,
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
    /// Lamports paid out of the host's bond to the joiner, because the host
    /// won and the loser owes only their stake. 0 when the joiner won.
    pub joiner_reimbursed: u64,
}

#[event]
pub struct GameCancelled {
    pub game: Pubkey,
    pub host: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct GameRefunded {
    pub game: Pubkey,
    pub host: Pubkey,
    pub joiner: Pubkey,
    pub mint: Pubkey,
    /// The host's stake, exactly.
    pub host_refund: u64,
    /// The joiner's stake plus whatever else the escrow held (donated dust).
    pub joiner_refund: u64,
}
