use anchor_lang::prelude::*;

use crate::errors::CoinflipError;

#[event_cpi]
#[derive(Accounts)]
pub struct SettleCallback<'info> {
    /// CHECK: completed in the settle_callback task.
    pub client: AccountInfo<'info>,
}

pub(crate) fn handle(_ctx: Context<SettleCallback>) -> Result<()> {
    err!(CoinflipError::RandomnessNotFulfilled)
}
