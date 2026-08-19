use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("7ZsoAuFYBBtTHt3jeCd8wWZvKcp7sqxEqAPscSNFte1n");

/// Protocol fee destination. Compile-time, not admin-rotatable: rotating it is
/// a program upgrade. Per the playbook, a feature flag may change constants and
/// IDs only, never logic — the `local` variant is the committed test fixture
/// keypair (`tests/fixtures/treasury-local.json`).
pub mod treasury {
    use super::*;

    #[cfg(feature = "local")]
    pub const ID: Pubkey = Pubkey::from_str_const("9wR75bCR1bo68BygzHkgJ3N735u5TmGsVhzjRrFzNUtJ");

    #[cfg(not(feature = "local"))]
    pub const ID: Pubkey = Pubkey::from_str_const("BUs86uMPdNMJ9SiFijb4TABpFduhaEqqESs96pTGadsN");
}

#[program]
pub mod coinflip {
    use super::*;

    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        admin: Pubkey,
        fee_bps: u16,
        refund_timeout_slots: u64,
    ) -> Result<()> {
        instructions::initialize_config::handle(ctx, admin, fee_bps, refund_timeout_slots)
    }

    pub fn update_config(
        ctx: Context<UpdateConfig>,
        new_admin: Option<Pubkey>,
        new_fee_bps: Option<u16>,
        new_refund_timeout_slots: Option<u64>,
    ) -> Result<()> {
        instructions::update_config::handle(ctx, new_admin, new_fee_bps, new_refund_timeout_slots)
    }

    pub fn create_game(
        ctx: Context<CreateGame>,
        side: u8,
        amount: u64,
        max_bond: u64,
    ) -> Result<()> {
        instructions::create_game::handle(ctx, side, amount, max_bond)
    }

    pub fn cancel_game(ctx: Context<CancelGame>) -> Result<()> {
        instructions::cancel_game::handle(ctx)
    }

    pub fn join_game(ctx: Context<JoinGame>, nonce: u64, max_vrf_fee: u64) -> Result<()> {
        instructions::join_game::handle(ctx, nonce, max_vrf_fee)
    }

    pub fn settle(ctx: Context<Settle>) -> Result<()> {
        instructions::settle::handle(ctx)
    }

    pub fn refund_timeout(ctx: Context<RefundTimeout>) -> Result<()> {
        instructions::refund_timeout::handle(ctx)
    }
}
