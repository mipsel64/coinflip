use anchor_lang::prelude::*;

use crate::{constants::CONFIG_SEED, errors::CoinflipError, state::Config};

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
    /// This program, whose ProgramData names the upgrade authority. The config
    /// PDA is a one-shot singleton, so whoever wins the race to initialize it
    /// picks the admin and the fee: gate that on the deployer.
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ CoinflipError::OwnerMismatch,
    )]
    pub program: Program<'info, crate::program::Coinflip>,
    #[account(
        constraint = program_data.upgrade_authority_address == Some(payer.key())
            @ CoinflipError::OwnerMismatch,
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handle(
    ctx: Context<InitializeConfig>,
    admin: Pubkey,
    fee_bps: u16,
    refund_timeout_slots: u64,
) -> Result<()> {
    Config::validate_fee(fee_bps)?;
    require!(admin != Pubkey::default(), CoinflipError::InvalidAuthority);
    Config::validate_timeout(refund_timeout_slots)?;
    let config = &mut ctx.accounts.config;
    config.version = Config::LAYOUT_VERSION;
    config.bump = ctx.bumps.config;
    config.admin = admin;
    config.fee_bps = fee_bps;
    config.refund_timeout_slots = refund_timeout_slots;
    config._reserved = [0; 64];
    Ok(())
}
