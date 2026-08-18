use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("7ZsoAuFYBBtTHt3jeCd8wWZvKcp7sqxEqAPscSNFte1n");

#[program]
pub mod coinflip {
    use super::*;

    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        admin: Pubkey,
        treasury: Pubkey,
        fee_bps: u16,
        refund_timeout_slots: u64,
    ) -> Result<()> {
        instructions::initialize_config::handle(ctx, admin, treasury, fee_bps, refund_timeout_slots)
    }

    pub fn update_config(
        ctx: Context<UpdateConfig>,
        new_admin: Option<Pubkey>,
        new_treasury: Option<Pubkey>,
        new_fee_bps: Option<u16>,
        new_refund_timeout_slots: Option<u64>,
    ) -> Result<()> {
        instructions::update_config::handle(
            ctx,
            new_admin,
            new_treasury,
            new_fee_bps,
            new_refund_timeout_slots,
        )
    }

    pub fn create_game(ctx: Context<CreateGame>, side: u8, amount: u64) -> Result<()> {
        instructions::create_game::handle(ctx, side, amount)
    }
}
