use anchor_lang::prelude::*;

use crate::{constants::CONFIG_SEED, state::Config};

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, Config>,
    pub system_program: Program<'info, System>,
}

pub fn handle(
    ctx: Context<InitializeConfig>,
    admin: Pubkey,
    treasury: Pubkey,
    fee_bps: u16,
    refund_timeout_slots: u64,
) -> Result<()> {
    Config::validate_fee(fee_bps)?;
    let config = &mut ctx.accounts.config;
    config.version = Config::LAYOUT_VERSION;
    config.bump = ctx.bumps.config;
    config.admin = admin;
    config.treasury = treasury;
    config.fee_bps = fee_bps;
    config.refund_timeout_slots = refund_timeout_slots;
    config._reserved = [0; 64];
    Ok(())
}
