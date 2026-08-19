use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use orao_solana_vrf::{state::RandomnessV2, RANDOMNESS_ACCOUNT_SEED};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    events::GameRefunded,
    instructions::settlement::require_payout_account,
    state::{Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct RefundTimeout<'info> {
    /// Permissionless fee-payer slot; carries no authority.
    pub cranker: Signer<'info>,
    /// Seed binding: this must be THE request for this game. The seed lives in
    /// ORAO's global request namespace, so `game.vrf_seed` is the whole binding.
    /// The bump comes from the game (recorded at join), so this is one hash
    /// rather than a search whose cost depends on the seed.
    #[account(
        seeds = [RANDOMNESS_ACCOUNT_SEED, game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf::ID,
        bump = game.request_bump,
    )]
    pub request: Box<Account<'info, RandomnessV2>>,
    #[account(mut, close = host)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: rent receiver, must be the game's host.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    /// Any host-owned account of the game mint (liveness: recorded one may be closed).
    #[account(
        mut,
        constraint = host_token_account.owner == game.host @ CoinflipError::OwnerMismatch,
        constraint = host_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any joiner-owned account of the game mint.
    #[account(
        mut,
        constraint = joiner_token_account.owner == game.joiner @ CoinflipError::OwnerMismatch,
        constraint = joiner_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        constraint = token_program.key() == *mint.to_account_info().owner
            @ CoinflipError::MintMismatch,
    )]
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<RefundTimeout>) -> Result<()> {
    ctx.accounts
        .game
        .require_state(GameState::AwaitingRandomness)?;
    // A fulfilled request must be settled on its outcome, never refunded.
    require!(
        ctx.accounts.request.fulfilled().is_none(),
        CoinflipError::AlreadyFulfilled
    );
    // The game's own snapshot, not live config: an admin lowering the timeout
    // later must not open an early, informed refund on a game already in flight.
    let deadline = ctx
        .accounts
        .game
        .joined_at_slot
        .checked_add(ctx.accounts.game.refund_timeout_slots)
        .ok_or(CoinflipError::NumericalOverflow)?;
    require!(
        Clock::get()?.slot > deadline,
        CoinflipError::TimeoutNotReached
    );
    // Permissionless cranker: refunds may only land on recorded-or-ATA
    // destinations (same rule as settle; see settlement.rs).
    require_payout_account(
        &ctx.accounts.host_token_account,
        ctx.accounts.game.host_token_account,
        ctx.accounts.game.host,
        ctx.accounts.game.token_mint,
        ctx.accounts.token_program.key(),
    )?;
    require_payout_account(
        &ctx.accounts.joiner_token_account,
        ctx.accounts.game.joiner_token_account,
        ctx.accounts.game.joiner,
        ctx.accounts.game.token_mint,
        ctx.accounts.token_program.key(),
    )?;

    let game_key = ctx.accounts.game.key();
    let seeds: &[&[&[u8]]] = &[&[
        ESCROW_SEED,
        game_key.as_ref(),
        &[ctx.accounts.game.escrow_bump],
    ]];
    let decimals = ctx.accounts.mint.decimals;

    // Host gets their stake back; the joiner gets the rest (their stake plus
    // any donated dust, so the escrow always drains to zero).
    let host_refund = ctx.accounts.game.amount;
    let joiner_refund = ctx
        .accounts
        .escrow
        .amount
        .checked_sub(host_refund)
        .ok_or(CoinflipError::NumericalOverflow)?;

    // Checks-effects-interactions: the state write precedes the transfers even
    // though it is unobservable here (the game account is closed anyway).
    ctx.accounts.game.state = GameState::Refunded.into();

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
        host_refund,
        decimals,
    )?;
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.joiner_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            seeds,
        ),
        joiner_refund,
        decimals,
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

    emit_cpi!(GameRefunded {
        game: game_key,
        host: ctx.accounts.game.host,
        joiner: ctx.accounts.game.joiner,
        mint: ctx.accounts.game.token_mint,
        host_refund,
        joiner_refund,
    });
    Ok(())
}
