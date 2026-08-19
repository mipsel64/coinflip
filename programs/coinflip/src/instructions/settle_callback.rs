use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use orao_solana_vrf_cb::{
    state::{client::Client, network_state::NetworkState, request::RequestAccount},
    CB_CLIENT_ACCOUNT_SEED, CB_CONFIG_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameSettled,
    instructions::settlement::{execute_settlement, treasury_ata, SettlementAccounts},
    state::{Config, Game},
};

#[event_cpi]
#[derive(Accounts)]
pub struct SettleCallback<'info> {
    /// Only the ORAO VRF program can produce this PDA's signature — that
    /// signature IS the proof the randomness is genuine.
    #[account(
        signer,
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
        // Belt-and-suspenders: the seeds (which include crate::ID) already pin this.
        constraint = client.program == crate::ID @ CoinflipError::UnauthorizedVrfClient,
    )]
    pub client: Box<Account<'info, Client>>,
    /// The registered state PDA (ORAO passes it writable).
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [CB_CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = network_state.bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    /// Seed binding: this must be THE request for this game.
    #[account(
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = request.bump,
    )]
    pub request: Box<Account<'info, RequestAccount>>,
    // ---- our accounts, in join_game's RemainingAccount order ----
    #[account(mut, close = host)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: rent receiver, must be the game's host.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    /// Address-pinned, not owner-constrained like the fallback's: this account
    /// list was frozen into the request at join time, so anything else here
    /// means the oracle is not replaying our own callback.
    /// Note: pays the recorded ADDRESS even if the player SetAuthority'd it
    /// away; the fallback would instead reject it and require the ATA.
    #[account(mut, address = game.host_token_account @ CoinflipError::InvalidPayoutAccount)]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = game.joiner_token_account @ CoinflipError::InvalidPayoutAccount)]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// ATA-pinned against the compile-time treasury, exactly like
    /// settle_fallback: the destination is fixed for the program's lifetime and
    /// derived the same way on both paths, so the account list frozen into the
    /// request at join time and a later fallback crank can never disagree about
    /// where the fee goes.
    #[account(
        mut,
        constraint = treasury_token_account.key()
            == treasury_ata(&game.token_mint, &token_program.key())
            @ CoinflipError::InvalidPayoutAccount,
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

pub(crate) fn handle(ctx: Context<SettleCallback>) -> Result<()> {
    let randomness = ctx
        .accounts
        .request
        .fulfilled()
        .ok_or(CoinflipError::RandomnessNotFulfilled)?
        .randomness;

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
