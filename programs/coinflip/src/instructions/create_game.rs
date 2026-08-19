use anchor_lang::{
    prelude::*,
    system_program::{self, Transfer},
};
use anchor_spl::{
    associated_token::AssociatedToken,
    token_2022::spl_token_2022::{
        self,
        extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    },
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};
use orao_solana_vrf::state::NetworkState;

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
    /// CHECK: the compile-time fee-destination authority.
    #[account(address = crate::treasury::ID @ CoinflipError::OwnerMismatch)]
    pub treasury: AccountInfo<'info>,
    /// Created here, by the host, so neither the joiner nor a cranker ever pays
    /// rent for the protocol's fee account. The host picked the mint, so the
    /// per-mint cost is theirs; it is a no-op for every game after the first of
    /// a given mint.
    #[account(
        init_if_needed,
        payer = host,
        associated_token::mint = mint,
        associated_token::authority = treasury,
        associated_token::token_program = token_program,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Read-only: ORAO's live `request_fee` sizes the host's bond (see the
    /// handler). Nothing is paid to ORAO here — the joiner does that at join.
    #[account(
        seeds = [orao_solana_vrf::CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf::ID,
        bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Allow-list per the design spec, deny by default: an extension this
/// program hasn't reviewed must never silently pass. Freeze authority is
/// allowed (USDC) — documented risk.
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
            // Known-safe: display/metadata/grouping, confidential mint config,
            // interest/scaled display, mint close (only closable at 0 supply),
            // default account state (fails closed at transfer time).
            ExtensionType::MintCloseAuthority
            | ExtensionType::InterestBearingConfig
            | ExtensionType::ScaledUiAmount
            | ExtensionType::MetadataPointer
            | ExtensionType::TokenMetadata
            | ExtensionType::GroupPointer
            | ExtensionType::GroupMemberPointer
            | ExtensionType::TokenGroup
            | ExtensionType::TokenGroupMember
            | ExtensionType::ConfidentialTransferMint
            | ExtensionType::DefaultAccountState => {}
            // Everything else — including TransferFeeConfig/TransferHook
            // (break payout math), PermanentDelegate (escrow drain),
            // Pausable (global freeze), ConfidentialTransferFeeConfig,
            // NonTransferable (can never pay out), and any extension a future
            // dependency bump introduces — is denied.
            _ => return err!(CoinflipError::UnsupportedMintExtension),
        }
    }
    Ok(())
}

/// * `max_bond` — the largest bond, in lamports, the host accepts locking up
///   for the life of the game. The bond is sized from ORAO's live `request_fee`
///   (see below), which ORAO's authority can raise at any time, so the host
///   states their own ceiling — the mirror image of the joiner's `max_vrf_fee`.
pub(crate) fn handle(ctx: Context<CreateGame>, side: u8, amount: u64, max_bond: u64) -> Result<()> {
    require!(amount > 0, CoinflipError::ZeroAmount);
    require!(amount <= u64::MAX / 2, CoinflipError::NumericalOverflow);
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

    // The winner-pays bond: lamports the host parks in the game account, above
    // its rent, so that a LOSING host can reimburse the joiner's join-time
    // costs (the loser must be out only their stake). It is sized here, at
    // create, from ORAO's live fee — 2x it, plus the rent the joiner sinks into
    // the request account — so it still covers a joiner who joins after a
    // moderate fee raise. Beyond that the reimbursement caps at the bond
    // (`settle`), which is why the bond is stored where a frontend can read it
    // before anyone joins. Whatever is not paid out returns to the host when
    // the game account closes — in full on cancel, on refund, and on a joiner
    // win; the remainder after the reimbursement when the host wins.
    let bond = ctx
        .accounts
        .network_state
        .config
        .request_fee
        .checked_mul(2)
        .ok_or(CoinflipError::NumericalOverflow)?
        .checked_add(super::fulfilled_request_rent()?)
        .ok_or(CoinflipError::NumericalOverflow)?;
    require!(bond <= max_bond, CoinflipError::BondTooHigh);
    // Safe to fund now, not before: `init` runs pre-handler, so the game
    // account exists and is already rent-exempt for its own data.
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            Transfer {
                from: ctx.accounts.host.to_account_info(),
                to: ctx.accounts.game.to_account_info(),
            },
        ),
        bond,
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
    game.vrf_seed = [0; 32];
    // Snapshotted at join, not here: the refund window starts when the joiner
    // commits, under whatever timeout was configured then.
    game.refund_timeout_slots = 0;
    game.bond_lamports = bond;
    // Both recorded at join: there is no request and no joiner yet.
    game.request_bump = 0;
    game.joiner_sunk_lamports = 0;
    game._reserved = [0; 21];

    emit_cpi!(GameCreated {
        game: game.key(),
        host: game.host,
        mint: game.token_mint,
        amount,
        host_side: game.host_side,
        fee_bps: game.fee_bps,
        bond_lamports: bond,
    });
    Ok(())
}
