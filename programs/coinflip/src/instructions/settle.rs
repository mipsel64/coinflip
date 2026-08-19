use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use orao_solana_vrf::{state::RandomnessV2, RANDOMNESS_ACCOUNT_SEED};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    events::GameSettled,
    instructions::settlement::{execute_settlement, treasury_ata, SettlementAccounts},
    state::Game,
};

#[event_cpi]
#[derive(Accounts)]
pub struct Settle<'info> {
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
    /// CHECK: reimbursement target, must be the game's joiner — the wallet that
    /// paid ORAO at join, not a token account.
    #[account(mut, address = game.joiner @ CoinflipError::OwnerMismatch)]
    pub joiner: AccountInfo<'info>,
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
    /// The constant treasury's canonical ATA for this mint — the one
    /// `create_game` guaranteed exists. Pinned by derivation, not merely by
    /// owner: a cranker picks this account, and scattering fees across other
    /// treasury-owned accounts would make collection a manual hunt.
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

pub(crate) fn handle(ctx: Context<Settle>) -> Result<()> {
    let randomness = ctx
        .accounts
        .request
        .fulfilled()
        .ok_or(CoinflipError::RandomnessNotFulfilled)?
        .randomness;

    // A cranker chooses these accounts, so the struct's owner+mint constraints
    // are not enough — `execute_settlement` pins each side to its recorded
    // account or its ATA.
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

    // The loser pays their stake and nothing else. When the HOST wins, the
    // joiner's join-time lamport costs come back out of the bond the host
    // posted at create; capped at that bond, which is all this game ever
    // promised (a fee raise between create and join can leave the joiner
    // short — the bond is public before anyone joins).
    //
    // A winning joiner is not reimbursed: they bore their own costs, which is
    // what "the loser pays only their stake" means from the other side.
    //
    // Done by lamport arithmetic, not a system CPI: the game account is
    // program-owned, so a transfer out of it can only be a direct debit — and
    // it must happen here, in the handler, because `close = host` sweeps
    // whatever is left after this instruction returns.
    let mut joiner_reimbursed = 0u64;
    if outcome.host_won {
        let game = &ctx.accounts.game;
        joiner_reimbursed = game.joiner_sunk_lamports.min(game.bond_lamports);
        if joiner_reimbursed > 0 {
            let game_info = game.to_account_info();
            // Cannot underflow (the bond sits above the account's rent) or
            // overflow, but this is a value path: checked, one borrow at a time.
            let debited = game_info
                .lamports()
                .checked_sub(joiner_reimbursed)
                .ok_or(CoinflipError::NumericalOverflow)?;
            **game_info.try_borrow_mut_lamports()? = debited;
            let credited = ctx
                .accounts
                .joiner
                .lamports()
                .checked_add(joiner_reimbursed)
                .ok_or(CoinflipError::NumericalOverflow)?;
            **ctx.accounts.joiner.try_borrow_mut_lamports()? = credited;
        }
    }

    emit_cpi!(GameSettled {
        game: ctx.accounts.game.key(),
        winner: outcome.winner,
        mint: ctx.accounts.game.token_mint,
        outcome: outcome.outcome,
        pot: outcome.pot,
        fee: outcome.fee,
        joiner_reimbursed,
    });
    Ok(())
}
