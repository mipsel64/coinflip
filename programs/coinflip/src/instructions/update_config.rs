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

pub fn handle(
    ctx: Context<UpdateConfig>,
    new_admin: Option<Pubkey>,
    new_treasury: Option<Pubkey>,
    new_fee_bps: Option<u16>,
    new_refund_timeout_slots: Option<u64>,
) -> Result<()> {
    let config = &mut ctx.accounts.config;
    if let Some(fee_bps) = new_fee_bps {
        Config::validate_fee(fee_bps)?;
        config.fee_bps = fee_bps;
    }
    if let Some(admin) = new_admin {
        config.admin = admin;
    }
    if let Some(treasury) = new_treasury {
        config.treasury = treasury;
    }
    if let Some(slots) = new_refund_timeout_slots {
        config.refund_timeout_slots = slots;
    }
    Ok(())
}
