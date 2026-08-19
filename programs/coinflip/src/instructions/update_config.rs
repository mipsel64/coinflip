use anchor_lang::prelude::*;

use crate::{constants::CONFIG_SEED, errors::CoinflipError, state::Config};

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = admin @ CoinflipError::OwnerMismatch,
    )]
    pub config: Account<'info, Config>,
}

pub(crate) fn handle(
    ctx: Context<UpdateConfig>,
    new_admin: Option<Pubkey>,
    new_fee_bps: Option<u16>,
    new_refund_timeout_slots: Option<u64>,
) -> Result<()> {
    let config = &mut ctx.accounts.config;
    if let Some(fee_bps) = new_fee_bps {
        Config::validate_fee(fee_bps)?;
        config.fee_bps = fee_bps;
    }
    // Rotation is authorized by the OLD admin (has_one checks pre-update state).
    if let Some(admin) = new_admin {
        require!(admin != Pubkey::default(), CoinflipError::InvalidAuthority);
        config.admin = admin;
    }
    if let Some(slots) = new_refund_timeout_slots {
        Config::validate_timeout(slots)?;
        config.refund_timeout_slots = slots;
    }
    Ok(())
}
