mod common;

use coinflip::state::GameState;
use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn join_escrows_stake_and_creates_vrf_request() {
    let (mut svm, payer) = setup();
    // LiteSVM starts at slot 0, where a `joined_at_slot` that never got
    // written is indistinguishable from one that did; warp first.
    let join_slot = 4_321;
    svm.warp_to_slot(join_slot);
    let stake = 5_000_000_000;
    let (joined, meta) = setup_joined_game(&mut svm, &payer, stake);
    let f = &joined.fixture;

    // Both stakes escrowed; the joiner's account was debited exactly once.
    assert_eq!(token_balance(&svm, &f.escrow), stake * 2);
    assert_eq!(
        token_balance(&svm, &joined.joiner_token_account),
        stake.saturating_mul(10) - stake
    );

    let game = read_game(&svm, &f.game.pubkey());
    assert_eq!(game.state, u8::from(GameState::AwaitingRandomness));
    assert_eq!(game.joiner, joined.joiner.pubkey());
    assert_eq!(game.joiner_token_account, joined.joiner_token_account);
    assert_eq!(game.joined_at_slot, join_slot);

    // The real ORAO program created the request account, rent-funded.
    let request = svm
        .get_account(&joined.request)
        .expect("request account must exist");
    assert_eq!(request.owner, orao_solana_vrf_cb::ID);
    assert!(
        request.lamports >= svm.minimum_balance_for_rent_exemption(request.data.len()),
        "request account is not rent-exempt"
    );

    // The joiner reimbursed the VRF fee, which ORAO moved on to its treasury.
    assert_eq!(
        svm.get_account(&joined.orao.orao_treasury)
            .unwrap()
            .lamports,
        ORAO_TREASURY_START_LAMPORTS + REQUEST_FEE
    );

    // The treasury ATA exists ahead of settlement (the callback can't pay rent).
    assert_eq!(token_balance(&svm, &joined.treasury_token_account), 0);

    // `ix_join_game` is a pure function of these inputs, so rebuilding it
    // reproduces the instruction that was sent — enough to re-derive the
    // account-key ordering `find_cpi_event` needs.
    let ix = ix_join_game(
        f,
        &joined.orao,
        joined.joiner.pubkey(),
        joined.joiner_token_account,
    );
    let ev = find_cpi_event::<coinflip::events::GameJoined>(
        &[ix],
        &joined.joiner.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameJoined not emitted");
    assert_eq!(ev.game, f.game.pubkey());
    assert_eq!(ev.joiner, joined.joiner.pubkey());
    assert_eq!(ev.vrf_request, joined.request);
}

/// The callback account list is frozen into the request at join time; the
/// `SettleCallback` accounts struct must line up with it position for
/// position, so pin the exact order here.
#[test]
fn join_pins_callback_account_order() {
    let (mut svm, payer) = setup();
    let (joined, _meta) = setup_joined_game(&mut svm, &payer, 1_000);
    let f = &joined.fixture;

    assert_eq!(
        request_callback_account_metas(&svm, &joined.request),
        vec![
            (f.game.pubkey(), true),
            (f.escrow, true),
            (f.host.pubkey(), true),
            (f.host_token_account, true),
            (joined.joiner_token_account, true),
            (joined.treasury_token_account, true),
            (f.mint, false),
            (anchor_spl::token::ID, false),
            (event_authority(), false),
            (coinflip::ID, false),
        ],
    );
}

#[test]
fn host_cannot_join_own_game() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let result = send(
        &mut svm,
        &[&f.host],
        &[ix_join_game(
            &f,
            &orao,
            f.host.pubkey(),
            f.host_token_account,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::HostCannotJoin);
}

#[test]
fn double_join_fails() {
    let (mut svm, payer) = setup();
    let (joined, _meta) = setup_joined_game(&mut svm, &payer, 1_000);
    let second = Keypair::new();
    svm.airdrop(&second.pubkey(), 10_000_000_000).unwrap();
    let second_ta = create_token_account(&mut svm, joined.fixture.mint, second.pubkey(), 10_000);
    let result = send(
        &mut svm,
        &[&second],
        &[ix_join_game(
            &joined.fixture,
            &joined.orao,
            second.pubkey(),
            second_ta,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidGameState);
}

#[test]
fn join_with_wrong_mint_token_account_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let wrong_mint = create_mint(&mut svm, 9);
    let wrong_ta = create_token_account(&mut svm, wrong_mint, joiner.pubkey(), 10_000);
    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), wrong_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}
