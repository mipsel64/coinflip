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

/// Pays the winner, takes the fee, closes the escrow, marks the game Settled.
/// The pot is the escrow's actual balance so donated dust can never brick the
/// close. Callers close the game account (`close = host`) and emit the event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_settlement<'info>(
    game: &mut Account<'info, Game>,
    escrow: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    host_token_account: &InterfaceAccount<'info, TokenAccount>,
    joiner_token_account: &InterfaceAccount<'info, TokenAccount>,
    treasury_token_account: &InterfaceAccount<'info, TokenAccount>,
    token_program: &Interface<'info, TokenInterface>,
    host: &AccountInfo<'info>,
    randomness: &[u8; 64],
) -> Result<SettlementOutcome> {
    game.require_state(GameState::AwaitingRandomness)?;

    let outcome = Side::from_randomness(randomness);
    let (winner, winner_token_account) = if game.winner_is_host(outcome)? {
        (game.host, host_token_account)
    } else {
        (game.joiner, joiner_token_account)
    };

    // Fee comes from the game's snapshot, never live config: admin fee changes
    // must not retro-apply to already-created games.
    let pot = escrow.amount;
    let fee = fee_amount(pot, game.fee_bps)?;
    let payout = pot
        .checked_sub(fee)
        .ok_or(CoinflipError::NumericalOverflow)?;

    let game_key = game.key();
    let seeds: &[&[&[u8]]] = &[&[ESCROW_SEED, game_key.as_ref(), &[game.escrow_bump]]];

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

    game.state = GameState::Settled.into();
    Ok(SettlementOutcome {
        winner,
        outcome: outcome.into(),
        pot,
        fee,
    })
}
