use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, system_instruction};
use anchor_spl::{
    associated_token::AssociatedToken,
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};
use orao_solana_vrf_cb::{
    cpi as orao_cpi,
    program::OraoVrfCb,
    state::{
        client::{Callback, Client, RemainingAccount},
        network_state::NetworkState,
    },
    RequestParams, CB_CLIENT_ACCOUNT_SEED, CB_CONFIG_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameJoined,
    state::{Config, Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct JoinGame<'info> {
    #[account(mut)]
    pub joiner: Signer<'info>,
    /// Registered as the ORAO client state PDA; signs the Request CPI.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut)]
    pub game: Box<Account<'info, Game>>,
    /// CHECK: rent receiver for terminal closes; authorized writable for the callback.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
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
    #[account(mut, address = game.host_token_account @ CoinflipError::MintMismatch)]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: fee-destination authority recorded in config.
    #[account(address = config.treasury @ CoinflipError::OwnerMismatch)]
    pub treasury: AccountInfo<'info>,
    /// Must exist by settlement time — the oracle's callback cannot pay rent.
    #[account(
        init_if_needed,
        payer = joiner,
        associated_token::mint = mint,
        associated_token::authority = treasury,
        associated_token::token_program = token_program,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub vrf: Program<'info, OraoVrfCb>,
    #[account(
        mut,
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
    )]
    pub client: Box<Account<'info, Client>>,
    #[account(
        mut,
        seeds = [CB_CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = network_state.bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    /// CHECK: asserted by the CPI.
    #[account(mut, address = network_state.config.treasury)]
    pub orao_treasury: AccountInfo<'info>,
    /// CHECK: created by the CPI; seed = game pubkey, so it's unique per game
    /// and a pre-existing (precomputed) request makes the join fail.
    #[account(
        mut,
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump,
    )]
    pub request: AccountInfo<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handle(ctx: Context<JoinGame>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::Open)?;
    require!(
        ctx.accounts.joiner.key() != ctx.accounts.game.host,
        CoinflipError::HostCannotJoin
    );

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

    // The ORAO Client PDA pays the request fee; the joiner reimburses it so
    // the client balance stays neutral.
    invoke(
        &system_instruction::transfer(
            &ctx.accounts.joiner.key(),
            &ctx.accounts.client.key(),
            ctx.accounts.network_state.config.request_fee,
        ),
        &[
            ctx.accounts.joiner.to_account_info(),
            ctx.accounts.client.to_account_info(),
        ],
    )?;

    // Callback account list — order must match SettleCallback's struct.
    let game_key = ctx.accounts.game.key();
    let callback = Callback::from_instruction_data(&crate::instruction::SettleCallback {})
        .with_remaining_accounts(vec![
            RemainingAccount::arbitrary_writable(game_key),
            RemainingAccount::arbitrary_writable(ctx.accounts.escrow.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.host.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.host_token_account.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.joiner_token_account.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.treasury_token_account.key()),
            RemainingAccount::readonly(ctx.accounts.mint.key()),
            RemainingAccount::readonly(ctx.accounts.token_program.key()),
            RemainingAccount::readonly(ctx.accounts.event_authority.key()),
            RemainingAccount::readonly(crate::ID),
        ]);

    let mut cpi_accounts = orao_cpi::accounts::Request {
        payer: ctx.accounts.joiner.to_account_info(),
        state: ctx.accounts.config.to_account_info(),
        client: ctx.accounts.client.to_account_info(),
        network_state: ctx.accounts.network_state.to_account_info(),
        treasury: ctx.accounts.orao_treasury.to_account_info(),
        request: ctx.accounts.request.to_account_info(),
        system_program: ctx.accounts.system_program.to_account_info(),
    };
    // Our Config PDA is the registered request authority.
    cpi_accounts.state.is_signer = true;
    let signer_seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[ctx.accounts.config.bump]]];

    let cpi_ctx = CpiContext::new(ctx.accounts.vrf.to_account_info(), cpi_accounts)
        .with_signer(signer_seeds)
        // Arbitrary-writable callback accounts are authorized by being
        // writable accounts 8+ of the Request instruction.
        .with_remaining_accounts(vec![
            ctx.accounts.game.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            ctx.accounts.host.to_account_info(),
            ctx.accounts.host_token_account.to_account_info(),
            ctx.accounts.joiner_token_account.to_account_info(),
            ctx.accounts.treasury_token_account.to_account_info(),
        ]);
    orao_cpi::request(
        cpi_ctx,
        RequestParams::new(game_key.to_bytes()).with_callback(Some(callback)),
    )?;

    let game = &mut ctx.accounts.game;
    game.joiner = ctx.accounts.joiner.key();
    game.joiner_token_account = ctx.accounts.joiner_token_account.key();
    game.joined_at_slot = Clock::get()?.slot;
    game.state = GameState::AwaitingRandomness.into();

    emit_cpi!(GameJoined {
        game: game_key,
        joiner: game.joiner,
        vrf_request: ctx.accounts.request.key(),
    });
    Ok(())
}
