mod common;

use coinflip::state::{Game, GameState};
use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn create_game_escrows_the_stake() {
    let (mut svm, payer) = setup();
    let fixture = setup_open_game(&mut svm, &payer, 5_000_000_000);
    assert_eq!(token_balance(&svm, &fixture.escrow), 5_000_000_000);
    assert_eq!(
        token_balance(&svm, &fixture.host_token_account),
        45_000_000_000
    );

    let game = read_game(&svm, &fixture.game.pubkey());
    assert_eq!(game.version, Game::LAYOUT_VERSION);
    assert_eq!(game.state, u8::from(GameState::Open));
    assert_eq!(game.fee_bps, DEFAULT_FEE_BPS);
    assert_eq!(game.host, fixture.host.pubkey());
    assert_eq!(game.host_token_account, fixture.host_token_account);
    assert_eq!(game.token_mint, fixture.mint);
    assert_eq!(game.amount, fixture.amount);
    assert_eq!(game.joiner, solana_sdk::pubkey::Pubkey::default());
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
fn create_game_rejects_transfer_fee_mint() {
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
    let mint = create_t22_transfer_fee_mint(&mut svm, 9);
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
