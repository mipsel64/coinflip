use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
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
    /// CPI itself. The seed is
    /// sha256("coinflip-vrf-seed", game, joiner, nonce) — see the handler for
    /// what the hash and the nonce each defend against.
    #[account(mut)]
    pub request: AccountInfo<'info>,
    #[account(
        constraint = token_program.key() == *mint.to_account_info().owner
            @ CoinflipError::MintMismatch,
    )]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// * `nonce` — client-chosen salt for the VRF seed. Any value works; clients
///   start at 0 and retry with the next one if the request address is already
///   taken (see the seed derivation below).
/// * `max_vrf_fee` — the largest ORAO request fee, in lamports, the joiner
///   accepts paying. ORAO's fee is live config that its authority can raise at
///   any time, and the joiner pays it directly, so the joiner — not this
///   program — states their limit.
pub(crate) fn handle(ctx: Context<JoinGame>, nonce: u64, max_vrf_fee: u64) -> Result<()> {
    let request_fee = ctx.accounts.network_state.config.request_fee;
    ctx.accounts.game.require_state(GameState::Open)?;
    require!(
        ctx.accounts.joiner.key() != ctx.accounts.game.host,
        CoinflipError::HostCannotJoin
    );
    // Fee cap up front with the other entry guards: reject before any
    // transfer so the failure path does no work and reads like the rest of
    // the codebase's checks-effects-interactions ordering.
    require!(request_fee <= max_vrf_fee, CoinflipError::VrfFeeTooHigh);

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

    // ORAO's request PDAs live in a GLOBAL namespace (no per-client component),
    // so anyone may create the request for any seed and thereby make this join
    // fail. Two properties keep that from being a durable block: the hash is
    // unpredictable before the joiner commits, and the client-chosen `nonce`
    // moves the address on demand — a join that loses the race retries at
    // nonce+1, a fresh address the attacker must guess and win all over again.
    // Residual: a per-ATTEMPT race, open only to someone who knows
    // (game, joiner, nonce) and can outrun the join transaction, and who pays
    // ORAO's fee plus the request's rent for each attempt while the joiner
    // pays a transaction fee to retry.
    let vrf_seed = solana_sha256_hasher::hashv(&[
        b"coinflip-vrf-seed",
        game_key.as_ref(),
        ctx.accounts.joiner.key().as_ref(),
        &nonce.to_le_bytes(),
    ])
    .to_bytes();

    // ORAO's fee is live config, raisable by ORAO's authority between the
    // client building this transaction and it landing. The joiner pays it from
    // their own wallet, so they get to bound it.
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
    // What this join just cost the joiner and never comes back on its own: the
    // fee ORAO keeps, plus the rent of the request account at its fulfilled
    // size (ORAO returns the rest of the rent to them when it fulfills). A
    // losing host reimburses exactly this out of the bond; see `settle`.
    game.joiner_sunk_lamports = request_fee
        .checked_add(super::fulfilled_request_rent()?)
        .ok_or(CoinflipError::NumericalOverflow)?;
    game.state = GameState::AwaitingRandomness.into();

    emit_cpi!(GameJoined {
        game: game_key,
        joiner: game.joiner,
        vrf_request: ctx.accounts.request.key(),
    });
    Ok(())
}
