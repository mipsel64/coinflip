use anchor_lang::prelude::*;
use anchor_spl::{
    token_2022::spl_token_2022::{
        self,
        extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    },
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameCreated,
    state::{Config, Game, GameState, Side},
};

#[event_cpi]
#[derive(Accounts)]
pub struct CreateGame<'info> {
    #[account(mut)]
    pub host: Signer<'info>,
    /// Fee snapshot source; games settle at the fee they were created under.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    /// Fresh keypair account — its pubkey IS the game id (it signs init only).
    #[account(init, payer = host, space = 8 + Game::INIT_SPACE)]
    pub game: Box<Account<'info, Game>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init,
        payer = host,
        seeds = [ESCROW_SEED, game.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = escrow,
        token::token_program = token_program,
    )]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = host_token_account.mint == mint.key() @ CoinflipError::MintMismatch,
        constraint = host_token_account.owner == host.key() @ CoinflipError::OwnerMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Deny-list per the design spec: transfer-fee breaks the payout math,
/// transfer hooks change the wire format, a permanent delegate can drain
/// the escrow. Freeze authority is allowed (USDC) — documented risk.
fn validate_mint(mint_info: &AccountInfo) -> Result<()> {
    if *mint_info.owner == anchor_spl::token::ID {
        return Ok(());
    }
    let data = mint_info.try_borrow_data()?;
    let mint = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&data)
        .map_err(|_| error!(CoinflipError::UnsupportedMintExtension))?;
    let extensions = mint
        .get_extension_types()
        .map_err(|_| error!(CoinflipError::UnsupportedMintExtension))?;
    for extension in extensions {
        match extension {
            ExtensionType::TransferFeeConfig
            | ExtensionType::TransferHook
            | ExtensionType::PermanentDelegate => {
                return err!(CoinflipError::UnsupportedMintExtension)
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn handle(ctx: Context<CreateGame>, side: u8, amount: u64) -> Result<()> {
    require!(amount > 0, CoinflipError::ZeroAmount);
    let side = Side::from_byte(side)?;
    validate_mint(&ctx.accounts.mint.to_account_info())?;

    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.host_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.host.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;

    let game = &mut ctx.accounts.game;
    game.version = Game::LAYOUT_VERSION;
    game.state = GameState::Open.into();
    game.host_side = side.into();
    game.escrow_bump = ctx.bumps.escrow;
    game.host = ctx.accounts.host.key();
    game.joiner = Pubkey::default();
    game.token_mint = ctx.accounts.mint.key();
    game.amount = amount;
    game.fee_bps = ctx.accounts.config.fee_bps;
    game.host_token_account = ctx.accounts.host_token_account.key();
    game.joiner_token_account = Pubkey::default();
    game.joined_at_slot = 0;
    game._reserved = [0; 62];

    emit_cpi!(GameCreated {
        game: game.key(),
        host: game.host,
        mint: game.token_mint,
        amount,
        host_side: game.host_side,
    });
    Ok(())
}
