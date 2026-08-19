mod common;

use anchor_lang::AccountDeserialize;
use common::*;
use orao_solana_vrf_cb::state::{
    client::Client, network_state::NetworkState, request::RequestAccount,
};
use solana_sdk::{pubkey::Pubkey, signature::Signer, transaction::TransactionError};

#[test]
fn initialize_and_update_config() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    let treasury = Pubkey::new_unique();

    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            admin,
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );

    // fee update within cap works
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_update_config(admin, None, None, Some(250), None)],
    );
}

/// The config PDA is a one-shot singleton whose initializer picks the admin,
/// the treasury and the fee — so only the program's upgrade authority may
/// claim it, no matter who wins the race to send the transaction.
#[test]
fn initialize_rejects_non_upgrade_authority() {
    let (mut svm, payer) = setup();
    let mallory = solana_sdk::signature::Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();

    let result = send(
        &mut svm,
        &[&mallory],
        &[ix_initialize_config(
            mallory.pubkey(),
            mallory.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
    assert!(
        svm.get_account(&config_pda())
            .is_none_or(|a| a.lamports == 0),
        "config must not exist after a rejected initialize"
    );

    // ...and the real deployer still can.
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
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
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );

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
    send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));
    // Second init must fail because the PDA already exists (the system
    // program's "allocate: account already in use"). It must NOT fail as
    // `AlreadyProcessed` — that would mean the harness rejected the identical
    // instruction as a duplicate transaction (stale blockhash) without ever
    // letting the program run, which would make this assertion vacuous.
    let err = send(&mut svm, &[&payer], &[ix]).unwrap_err().err;
    assert_ne!(
        err,
        TransactionError::AlreadyProcessed,
        "second init was rejected as a duplicate transaction instead of actually executing"
    );
}

#[test]
fn update_config_rejects_fee_above_cap_and_bad_timeout() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            admin,
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
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

    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
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
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            Pubkey::new_unique(),
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );
    let new_admin = solana_sdk::signature::Keypair::new();
    svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    // old admin rotates to new
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            Some(new_admin.pubkey()),
            None,
            None,
            None,
        )],
    );
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
    send_ok(
        &mut svm,
        &[&new_admin],
        &[ix_update_config(
            new_admin.pubkey(),
            None,
            None,
            Some(200),
            None,
        )],
    );
}

/// Verifies the crafted ORAO accounts (`NetworkState`, `Client`, a fulfilled
/// `RequestAccount`) actually round-trip through Anchor's own deserializer
/// with the shapes/values the rest of the harness assumes, instead of only
/// ever being read back out through our own hand-rolled writer. Covers the
/// ORAO side of the harness that later tasks (join/settle) depend on.
#[test]
fn orao_accounts_round_trip() {
    let (mut svm, _payer) = setup();
    let orao = setup_orao(&mut svm);
    let game = Pubkey::new_unique();
    let joiner = Pubkey::new_unique();
    let randomness = [7u8; 64];
    let request_addr = write_fulfilled_request_unchecked(
        &mut svm,
        orao.client,
        vrf_seed_for(&game, &joiner),
        randomness,
    );

    let ns_account = svm.get_account(&orao.network_state).unwrap();
    let network_state = NetworkState::try_deserialize(&mut &ns_account.data[..]).unwrap();
    assert_eq!(network_state.config.request_fee, REQUEST_FEE);

    let client_account = svm.get_account(&orao.client).unwrap();
    let client = Client::try_deserialize(&mut &client_account.data[..]).unwrap();
    assert_eq!(client.state, config_pda());
    assert_eq!(client.program, coinflip::ID);

    let request_account = svm.get_account(&request_addr).unwrap();
    let request = RequestAccount::try_deserialize(&mut &request_account.data[..]).unwrap();
    assert_eq!(request.fulfilled().unwrap().randomness, randomness);
}
