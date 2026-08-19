use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};
use orao_solana_vrf::{cpi as orao_cpi, program::OraoVrf, state::NetworkState};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameJoined,
    state::{Config, Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct JoinGame<'info> {
    /// Pays the VRF request fee and the request account's rent directly.
    #[account(mut)]
    pub joiner: Signer<'info>,
    /// Read-only snapshot source for `refund_timeout_slots`.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut)]
    pub game: Box<Account<'info, Game>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = joiner_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
        constraint = joiner_token_account.owner == joiner.key() @ CoinflipError::OwnerMismatch,
    )]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: the compile-time fee-destination authority.
    #[account(address = crate::treasury::ID @ CoinflipError::OwnerMismatch)]
    pub treasury: AccountInfo<'info>,
    /// Created here so settlement never has to: a cranker's transaction should
    /// not be the one paying rent for the protocol's fee account.
    #[account(
        init_if_needed,
        payer = joiner,
        associated_token::mint = mint,
        associated_token::authority = treasury,
        associated_token::token_program = token_program,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub vrf: Program<'info, OraoVrf>,
    #[account(
        mut,
        seeds = [orao_solana_vrf::CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf::ID,
        bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    /// CHECK: ORAO's own fee destination, re-asserted by the CPI. Pinned to the
    /// SOL treasury: ORAO would also accept its token treasury, which needs
    /// remaining accounts we never pass.
    #[account(mut, address = network_state.config.treasury @ CoinflipError::OwnerMismatch)]
    pub orao_treasury: AccountInfo<'info>,
    /// CHECK: created (and PDA-validated against the seed we pass) by the ORAO
    /// CPI itself. The seed is sha256("coinflip-vrf-seed", game, joiner) —
    /// unpredictable pre-join, so the address cannot be grief-pre-funded.
    #[account(mut)]
    pub request: AccountInfo<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    #[account(
        constraint = token_program.key() == *mint.to_account_info().owner
            @ CoinflipError::MintMismatch,
    )]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handle(ctx: Context<JoinGame>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::Open)?;
    require!(
        ctx.accounts.joiner.key() != ctx.accounts.game.host,
        CoinflipError::HostCannotJoin
    );

    let game_key = ctx.accounts.game.key();

    // Matching stake into escrow.
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.joiner_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.joiner.to_account_info(),
            },
        ),
        ctx.accounts.game.amount,
        ctx.accounts.mint.decimals,
    )?;

    // Unpredictable before the joiner commits: nobody can pre-fund the request
    // PDA to permanently block this game's join.
    let vrf_seed = solana_sha256_hasher::hashv(&[
        b"coinflip-vrf-seed",
        game_key.as_ref(),
        ctx.accounts.joiner.key().as_ref(),
    ])
    .to_bytes();

    // The joiner is ORAO's payer: they fund the request account's rent and the
    // request fee out of their own wallet, so this program holds no VRF float
    // and needs no ORAO client registration.
    orao_cpi::request_v2(
        CpiContext::new(
            ctx.accounts.vrf.to_account_info(),
            orao_cpi::accounts::RequestV2 {
                payer: ctx.accounts.joiner.to_account_info(),
                network_state: ctx.accounts.network_state.to_account_info(),
                treasury: ctx.accounts.orao_treasury.to_account_info(),
                request: ctx.accounts.request.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            },
        ),
        vrf_seed,
    )?;

    let game = &mut ctx.accounts.game;
    game.joiner = ctx.accounts.joiner.key();
    game.joiner_token_account = ctx.accounts.joiner_token_account.key();
    game.joined_at_slot = Clock::get()?.slot;
    game.vrf_seed = vrf_seed;
    // Snapshotted, not read live at refund time: a later config change must not
    // retro-shrink this game's settle window.
    game.refund_timeout_slots = ctx.accounts.config.refund_timeout_slots;
    game.state = GameState::AwaitingRandomness.into();

    emit_cpi!(GameJoined {
        game: game_key,
        joiner: game.joiner,
        vrf_request: ctx.accounts.request.key(),
    });
    Ok(())
}
