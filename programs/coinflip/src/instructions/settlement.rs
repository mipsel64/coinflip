use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    math::fee_amount,
    state::{Game, GameState, Side},
};

pub(crate) struct SettlementOutcome {
    pub winner: Pubkey,
    pub outcome: u8,
    pub pot: u64,
    pub fee: u64,
}

/// Named so the two call sites (callback and fallback) cannot transpose
/// same-typed token accounts.
pub(crate) struct SettlementAccounts<'a, 'info> {
    pub game: &'a mut Account<'info, Game>,
    pub escrow: &'a InterfaceAccount<'info, TokenAccount>,
    pub mint: &'a InterfaceAccount<'info, Mint>,
    pub host_token_account: &'a InterfaceAccount<'info, TokenAccount>,
    pub joiner_token_account: &'a InterfaceAccount<'info, TokenAccount>,
    pub treasury_token_account: &'a InterfaceAccount<'info, TokenAccount>,
    pub token_program: &'a Interface<'info, TokenInterface>,
    pub host: &'a AccountInfo<'info>,
}

/// A permissionless cranker may only route funds to the account the player
/// recorded, or to the player's canonical ATA (permissionlessly re-creatable,
/// so a closed recorded account can never strand funds) — never to some other
/// player-owned account that might carry a delegate.
pub(crate) fn require_payout_account(
    account: &InterfaceAccount<TokenAccount>,
    recorded: Pubkey,
    player: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
) -> Result<()> {
    let ata = anchor_spl::associated_token::get_associated_token_address_with_program_id(
        &player,
        &mint,
        &token_program,
    );
    require!(
        account.key() == recorded || account.key() == ata,
        CoinflipError::InvalidPayoutAccount
    );
    Ok(())
}

/// Pays the winner, takes the fee, closes the escrow, marks the game Settled.
/// The pot is the escrow's actual balance so donated dust can never brick the
/// close. Callers close the game account (`close = host`) and emit the event.
pub(crate) fn execute_settlement(
    accounts: SettlementAccounts,
    randomness: &[u8; 64],
) -> Result<SettlementOutcome> {
    let SettlementAccounts {
        game,
        escrow,
        mint,
        host_token_account,
        joiner_token_account,
        treasury_token_account,
        token_program,
        host,
    } = accounts;

    game.require_state(GameState::AwaitingRandomness)?;

    // Guarded here, not at the call sites, so no settlement path can forget it
    // (the callback's address-pinned accounts satisfy it trivially).
    let token_program_id = token_program.key();
    require_payout_account(
        host_token_account,
        game.host_token_account,
        game.host,
        game.token_mint,
        token_program_id,
    )?;
    require_payout_account(
        joiner_token_account,
        game.joiner_token_account,
        game.joiner,
        game.token_mint,
        token_program_id,
    )?;

    let outcome = Side::from_randomness(randomness);
    let (winner, winner_token_account) = if game.winner_is_host(outcome)? {
        (game.host, host_token_account)
    } else {
        (game.joiner, joiner_token_account)
    };

    let pot = escrow.amount;
    // Fee comes from the game's snapshot, never live config: admin fee changes
    // must not retro-apply to already-created games.
    let fee = fee_amount(pot, game.fee_bps)?;
    let payout = pot
        .checked_sub(fee)
        .ok_or(CoinflipError::NumericalOverflow)?;

    let game_key = game.key();
    let seeds: &[&[&[u8]]] = &[&[ESCROW_SEED, game_key.as_ref(), &[game.escrow_bump]]];

    // Checks-effects-interactions: the state write precedes the transfers even
    // though it is unobservable here (callers close the game account anyway).
    game.state = GameState::Settled.into();

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: escrow.to_account_info(),
                mint: mint.to_account_info(),
                to: winner_token_account.to_account_info(),
                authority: escrow.to_account_info(),
            },
            seeds,
        ),
        payout,
        mint.decimals,
    )?;
    if fee > 0 {
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                TransferChecked {
                    from: escrow.to_account_info(),
                    mint: mint.to_account_info(),
                    to: treasury_token_account.to_account_info(),
                    authority: escrow.to_account_info(),
                },
                seeds,
            ),
            fee,
            mint.decimals,
        )?;
    }
    token_interface::close_account(CpiContext::new_with_signer(
        token_program.to_account_info(),
        CloseAccount {
            account: escrow.to_account_info(),
            destination: host.clone(),
            authority: escrow.to_account_info(),
        },
        seeds,
    ))?;

    Ok(SettlementOutcome {
        winner,
        outcome: outcome.into(),
        pot,
        fee,
    })
}
