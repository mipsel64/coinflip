use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use orao_solana_vrf_cb::{
    state::{client::Client, request::RequestAccount},
    CB_CLIENT_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameSettled,
    instructions::settlement::{execute_settlement, require_payout_account, SettlementAccounts},
    state::{Config, Game},
};

#[event_cpi]
#[derive(Accounts)]
pub struct SettleFallback<'info> {
    /// Permissionless fee-payer slot; carries no authority.
    pub cranker: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
    )]
    pub client: Box<Account<'info, Client>>,
    /// Seed binding: this must be THE request for this game.
    #[account(
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = request.bump,
    )]
    pub request: Box<Account<'info, RequestAccount>>,
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
    #[account(
        mut,
        constraint = treasury_token_account.owner == config.treasury
            @ CoinflipError::OwnerMismatch,
        constraint = treasury_token_account.mint == game.token_mint
            @ CoinflipError::MintMismatch,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        constraint = token_program.key() == *mint.to_account_info().owner
            @ CoinflipError::MintMismatch,
    )]
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<SettleFallback>) -> Result<()> {
    let randomness = ctx
        .accounts
        .request
        .fulfilled()
        .ok_or(CoinflipError::RandomnessNotFulfilled)?
        .randomness;

    // A cranker chooses these accounts, so the struct's owner+mint constraints
    // are not enough: pin each side to its recorded account or its ATA.
    let game = &ctx.accounts.game;
    let token_program = ctx.accounts.token_program.key();
    require_payout_account(
        &ctx.accounts.host_token_account,
        game.host_token_account,
        game.host,
        game.token_mint,
        token_program,
    )?;
    require_payout_account(
        &ctx.accounts.joiner_token_account,
        game.joiner_token_account,
        game.joiner,
        game.token_mint,
        token_program,
    )?;

    let outcome = execute_settlement(
        SettlementAccounts {
            game: &mut ctx.accounts.game,
            escrow: &ctx.accounts.escrow,
            mint: &ctx.accounts.mint,
            host_token_account: &ctx.accounts.host_token_account,
            joiner_token_account: &ctx.accounts.joiner_token_account,
            treasury_token_account: &ctx.accounts.treasury_token_account,
            token_program: &ctx.accounts.token_program,
            host: &ctx.accounts.host,
        },
        &randomness,
    )?;

    emit_cpi!(GameSettled {
        game: ctx.accounts.game.key(),
        winner: outcome.winner,
        mint: ctx.accounts.game.token_mint,
        outcome: outcome.outcome,
        pot: outcome.pot,
        fee: outcome.fee,
    });
    Ok(())
}
