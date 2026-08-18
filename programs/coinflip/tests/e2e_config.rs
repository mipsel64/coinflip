mod common;

use common::*;
use solana_sdk::{pubkey::Pubkey, signature::Signer};

#[test]
fn initialize_and_update_config() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    let treasury = Pubkey::new_unique();

    send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            admin,
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    )
    .unwrap();

    // fee update within cap works
    send(
        &mut svm,
        &[&payer],
        &[ix_update_config(admin, None, None, Some(250), None)],
    )
    .unwrap();
}

#[test]
fn initialize_rejects_fee_above_cap() {
    let (mut svm, payer) = setup();
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            1_001,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::FeeTooHigh);
}

#[test]
fn update_config_rejects_non_admin() {
    let (mut svm, payer) = setup();
    send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    )
    .unwrap();

    let mallory = solana_sdk::signature::Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let result = send(
        &mut svm,
        &[&mallory],
        &[ix_update_config(
            mallory.pubkey(),
            None,
            None,
            Some(0),
            None,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn initialize_config_is_one_shot() {
    let (mut svm, payer) = setup();
    let ix = ix_initialize_config(
        payer.pubkey(),
        payer.pubkey(),
        Pubkey::new_unique(),
        DEFAULT_FEE_BPS,
        DEFAULT_TIMEOUT_SLOTS,
    );
    send(&mut svm, &[&payer], std::slice::from_ref(&ix)).unwrap();
    // second init must fail: the PDA already exists
    assert!(send(&mut svm, &[&payer], &[ix]).is_err());
}

#[test]
fn update_config_rejects_fee_above_cap_and_bad_timeout() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            admin,
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    )
    .unwrap();
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_update_config(admin, None, None, Some(1_001), None)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::FeeTooHigh);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_update_config(admin, None, None, None, Some(0))],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidTimeout);
}

#[test]
fn default_key_authorities_are_rejected() {
    let (mut svm, payer) = setup();
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::default(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidAuthority);

    send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    )
    .unwrap();
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            Some(Pubkey::default()),
            None,
            None,
            None,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidAuthority);
}

#[test]
fn admin_rotation_round_trip() {
    let (mut svm, payer) = setup();
    send(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    )
    .unwrap();
    let new_admin = solana_sdk::signature::Keypair::new();
    svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    // old admin rotates to new
    send(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            Some(new_admin.pubkey()),
            None,
            None,
            None,
        )],
    )
    .unwrap();
    // old admin is now rejected
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            None,
            None,
            Some(200),
            None,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
    // new admin works
    send(
        &mut svm,
        &[&new_admin],
        &[ix_update_config(
            new_admin.pubkey(),
            None,
            None,
            Some(200),
            None,
        )],
    )
    .unwrap();
}
