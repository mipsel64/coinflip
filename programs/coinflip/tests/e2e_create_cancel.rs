mod common;

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
