use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    events::GameCancelled,
    state::{Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct CancelGame<'info> {
    #[account(mut)]
    pub host: Signer<'info>,
    #[account(
        mut,
        close = host,
        has_one = host @ CoinflipError::OwnerMismatch,
    )]
    pub game: Box<Account<'info, Game>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any host-owned account of the game mint (liveness: the recorded one may
    /// have been closed since create).
    #[account(
        mut,
        constraint = host_token_account.owner == game.host @ CoinflipError::OwnerMismatch,
        constraint = host_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<CancelGame>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::Open)?;

    let game_key = ctx.accounts.game.key();
    let seeds: &[&[&[u8]]] = &[&[
        ESCROW_SEED,
        game_key.as_ref(),
        &[ctx.accounts.game.escrow_bump],
    ]];

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.host_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            seeds,
        ),
        ctx.accounts.escrow.amount,
        ctx.accounts.mint.decimals,
    )?;
    token_interface::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        CloseAccount {
            account: ctx.accounts.escrow.to_account_info(),
            destination: ctx.accounts.host.to_account_info(),
            authority: ctx.accounts.escrow.to_account_info(),
        },
        seeds,
    ))?;

    ctx.accounts.game.state = GameState::Cancelled.into();
    emit_cpi!(GameCancelled {
        game: game_key,
        host: ctx.accounts.game.host,
        mint: ctx.accounts.game.token_mint,
        amount: ctx.accounts.game.amount,
    });
    Ok(())
}
