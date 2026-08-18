mod common;

use coinflip::state::{Game, GameState, Side};
use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn create_game_escrows_the_stake() {
    let (mut svm, payer) = setup();
    let stake = 5_000_000_000;
    let (fixture, meta) = setup_open_game(&mut svm, &payer, stake);
    assert_eq!(token_balance(&svm, &fixture.escrow), stake);
    assert_eq!(
        token_balance(&svm, &fixture.host_token_account),
        fixture.amount.saturating_mul(10) - fixture.amount
    );

    // Escrow is a self-authority PDA over the game's mint, at the canonical bump.
    let escrow_account = read_token_account(&svm, &fixture.escrow);
    assert_eq!(escrow_account.owner, fixture.escrow);
    assert_eq!(escrow_account.mint, fixture.mint);
    let (canonical_escrow, canonical_bump) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[
            coinflip::constants::ESCROW_SEED,
            fixture.game.pubkey().as_ref(),
        ],
        &coinflip::ID,
    );
    assert_eq!(fixture.escrow, canonical_escrow);

    let game = read_game(&svm, &fixture.game.pubkey());
    assert_eq!(game.version, Game::LAYOUT_VERSION);
    assert_eq!(game.state, u8::from(GameState::Open));
    assert_eq!(game.escrow_bump, canonical_bump);
    assert_eq!(game.fee_bps, DEFAULT_FEE_BPS);
    assert_eq!(game.host, fixture.host.pubkey());
    assert_eq!(game.host_token_account, fixture.host_token_account);
    assert_eq!(game.token_mint, fixture.mint);
    assert_eq!(game.amount, fixture.amount);
    assert_eq!(game.joiner, solana_sdk::pubkey::Pubkey::default());

    // The create_game ix is a pure function of these fields, so rebuilding it
    // reproduces the exact instruction that was sent — enough to re-derive
    // the account-key ordering `find_cpi_event` needs to resolve inner-ix
    // program ids.
    let ix = ix_create_game(
        fixture.host.pubkey(),
        fixture.game.pubkey(),
        fixture.mint,
        fixture.host_token_account,
        0,
        fixture.amount,
    );
    let event = find_cpi_event::<coinflip::events::GameCreated>(
        &[ix],
        &fixture.host.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameCreated event not found in inner instructions");
    assert_eq!(event.game, fixture.game.pubkey());
    assert_eq!(event.host, fixture.host.pubkey());
    assert_eq!(event.mint, fixture.mint);
    assert_eq!(event.amount, fixture.amount);
    assert_eq!(event.host_side, u8::from(Side::Heads));
    assert_eq!(event.fee_bps, DEFAULT_FEE_BPS);
}

#[test]
fn create_game_rejects_zero_amount() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            0,
            0,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::ZeroAmount);
}

#[test]
fn create_game_rejects_invalid_side() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            2,
            10,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidSide);
}

#[test]
fn create_game_rejects_denied_mint_extensions() {
    for extension in [
        ExtensionType::TransferFeeConfig,
        ExtensionType::TransferHook,
        ExtensionType::PermanentDelegate,
    ] {
        let (mut svm, payer) = setup();
        let treasury = solana_sdk::pubkey::Pubkey::new_unique();
        send_ok(
            &mut svm,
            &[&payer],
            &[ix_initialize_config(
                payer.pubkey(),
                payer.pubkey(),
                treasury,
                DEFAULT_FEE_BPS,
                DEFAULT_TIMEOUT_SLOTS,
            )],
        );
        let host = Keypair::new();
        svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
        let mint = create_t22_mint_with_extensions(&mut svm, 9, &[extension]);
        let host_ta = create_token_account_for_program(
            &mut svm,
            anchor_spl::token_2022::ID,
            mint,
            host.pubkey(),
            100,
        );
        let game = Keypair::new();
        let result = send(
            &mut svm,
            &[&host, &game],
            &[ix_create_game_with_program(
                host.pubkey(),
                game.pubkey(),
                mint,
                host_ta,
                anchor_spl::token_2022::ID,
                0,
                10,
            )],
        );
        assert_coinflip_error(
            result,
            coinflip::errors::CoinflipError::UnsupportedMintExtension,
        );
    }
}

#[test]
fn create_game_rejects_mint_mismatch() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let game_mint = create_mint(&mut svm, 9);
    let other_mint = create_mint(&mut svm, 9);
    // Owned by the host (so the ownership constraint passes) but minted for
    // a different mint than the one passed to `ix_create_game`.
    let host_ta = create_token_account(&mut svm, other_mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            game_mint,
            host_ta,
            0,
            10,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}

#[test]
fn create_game_rejects_non_owned_token_account() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    // Token account is real and holds the right mint, but its SPL `owner`
    // field belongs to someone else — not the signing host.
    let mallory = solana_sdk::pubkey::Pubkey::new_unique();
    let host_ta = create_token_account(&mut svm, mint, mallory, 100);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            0,
            10,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn create_game_rejects_stake_above_cap() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    // Funding stays small: the cap guard fires before any transfer is attempted.
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 10);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            0,
            u64::MAX / 2 + 1,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::NumericalOverflow);
}

#[test]
fn cancel_refunds_host_and_closes_accounts() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 5_000_000_000);

    let host_lamports_before = svm.get_account(&f.host.pubkey()).unwrap().lamports;
    let escrow_rent = svm.get_account(&f.escrow).unwrap().lamports;
    let game_rent = svm.get_account(&f.game.pubkey()).unwrap().lamports;

    let ix = ix_cancel_game(&f);
    let meta = send_ok(&mut svm, &[&f.host], std::slice::from_ref(&ix));

    assert_eq!(
        token_balance(&svm, &f.host_token_account),
        f.amount.saturating_mul(10)
    );
    assert!(svm.get_account(&f.escrow).is_none_or(|a| a.lamports == 0));
    assert!(svm
        .get_account(&f.game.pubkey())
        .is_none_or(|a| a.lamports == 0));

    // Escrow + game rent land in the host's wallet, minus the one-signer tx fee.
    let host_lamports_after = svm.get_account(&f.host.pubkey()).unwrap().lamports;
    let tx_fee = 5_000;
    assert_eq!(
        host_lamports_after - host_lamports_before,
        escrow_rent + game_rent - tx_fee
    );

    let ev = find_cpi_event::<coinflip::events::GameCancelled>(
        &[ix],
        &f.host.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameCancelled not emitted");
    assert_eq!(ev.game, f.game.pubkey());
    assert_eq!(ev.host, f.host.pubkey());
    assert_eq!(ev.mint, f.mint);
    assert_eq!(ev.amount, f.amount);
}

#[test]
fn cancel_by_non_host_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let mallory = Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let mut ix = ix_cancel_game(&f);
    ix.accounts[0].pubkey = mallory.pubkey(); // host slot
    let result = send(&mut svm, &[&mallory], &[ix]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn cancel_pays_any_host_owned_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 5_000_000_000);
    // A second host-owned account for the same mint, distinct from the one
    // recorded on the game at create time.
    let second_host_ta = create_token_account(&mut svm, f.mint, f.host.pubkey(), 0);
    send_ok(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, second_host_ta)],
    );
    assert_eq!(token_balance(&svm, &second_host_ta), f.amount);
    // The recorded account is untouched: the refund went to the account we
    // actually passed in, not the one stored on the game.
    assert_eq!(
        token_balance(&svm, &f.host_token_account),
        f.amount.saturating_mul(9)
    );
    assert!(svm.get_account(&f.escrow).is_none_or(|a| a.lamports == 0));
    assert!(svm
        .get_account(&f.game.pubkey())
        .is_none_or(|a| a.lamports == 0));
}

#[test]
fn cancel_rejects_non_host_owned_refund_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let mallory = solana_sdk::pubkey::Pubkey::new_unique();
    let mallory_ta = create_token_account(&mut svm, f.mint, mallory, 0);
    let result = send(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, mallory_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn cancel_rejects_wrong_mint_refund_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    // Host-owned, but for a different mint than the game's.
    let other_mint = create_mint(&mut svm, 9);
    let wrong_mint_ta = create_token_account(&mut svm, other_mint, f.host.pubkey(), 0);
    let result = send(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, wrong_mint_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}
