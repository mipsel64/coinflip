mod common;

use anchor_spl::{associated_token::get_associated_token_address, token::spl_token};
use common::*;
use solana_sdk::{
    account::Account as SolanaAccount,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};

const STAKE: u64 = 5_000_000_000; // 5 tokens at 9 decimals — the spec's example
/// What each player's token account keeps after staking (`setup_open_game`
/// funds both sides with `10 * amount`).
const REMAINING: u64 = STAKE * 10 - STAKE; // 45.0
const POT: u64 = STAKE * 2; // 10.0
const FEE: u64 = 100_000_000; // 1% of the pot => 0.1
const PAYOUT: u64 = POT - FEE; // 9.9

fn randomness_with_first_byte(byte: u8) -> [u8; 64] {
    let mut r = [7u8; 64];
    r[0] = byte;
    r
}

fn is_gone(svm: &litesvm::LiteSVM, address: &Pubkey) -> bool {
    svm.get_account(address).is_none_or(|a| a.lamports == 0)
}

#[test]
fn settle_pays_host_when_host_side_wins() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // host_side = Heads (0); randomness[0] even => Heads => host wins.
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2),
    );

    let game_key = j.fixture.game.pubkey();
    let host = j.fixture.host.pubkey();
    // Both rents belong to the host; the settle payer is a third party, so the
    // host's lamport delta is exactly game rent + escrow rent.
    let game_rent = svm.get_account(&game_key).unwrap().lamports;
    let escrow_rent = svm.get_account(&j.fixture.escrow).unwrap().lamports;
    let host_lamports_before = svm.get_account(&host).unwrap().lamports;

    let ix = ix_settle_fallback(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    // pot 10, fee 1% = 0.1, payout 9.9 (in base units)
    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        REMAINING + PAYOUT
    );
    assert_eq!(token_balance(&svm, &j.joiner_token_account), REMAINING);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), FEE);
    assert!(is_gone(&svm, &j.fixture.escrow), "escrow must be closed");
    assert!(is_gone(&svm, &game_key), "game must be closed");
    assert_eq!(
        svm.get_account(&host).unwrap().lamports,
        host_lamports_before + game_rent + escrow_rent,
        "both rents must return to the host"
    );

    // Budget guard: two transfers + a close + the event CPI (measured ~42k).
    assert!(
        meta.compute_units_consumed < 60_000,
        "settle used {} CU",
        meta.compute_units_consumed
    );

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.game, game_key);
    assert_eq!(ev.winner, host);
    assert_eq!(ev.mint, j.fixture.mint);
    assert_eq!(ev.outcome, u8::from(coinflip::state::Side::Heads));
    assert_eq!(ev.pot, POT);
    assert_eq!(ev.fee, FEE);
}

#[test]
fn settle_pays_joiner_when_host_side_loses() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // randomness[0] odd => Tails => joiner (host picked Heads) wins.
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(3),
    );

    let ix = ix_settle_fallback(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    assert_eq!(
        token_balance(&svm, &j.joiner_token_account),
        REMAINING + PAYOUT
    );
    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        REMAINING
    );
    assert_eq!(token_balance(&svm, &j.treasury_token_account), FEE);

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.winner, j.joiner.pubkey());
    assert_eq!(ev.outcome, u8::from(coinflip::state::Side::Tails));
}

#[test]
fn settle_before_fulfillment_fails() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // Request exists (real, pending) but is NOT fulfilled.
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback(&j, payer.pubkey())],
    );
    assert_coinflip_error(
        result,
        coinflip::errors::CoinflipError::RandomnessNotFulfilled,
    );
}

#[test]
fn settle_twice_fails() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(0),
    );
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback(&j, payer.pubkey())],
    );

    // The game account is closed, so a second settle can't even load it — and
    // `send` expires the blockhash, so this really is a fresh transaction and
    // not a duplicate the runtime dropped before execution.
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback(&j, payer.pubkey())],
    );
    let failure = result.unwrap_err();
    assert!(
        !matches!(failure.err, TransactionError::AlreadyProcessed),
        "second settle must actually execute, not be dropped as a duplicate"
    );
    assert!(
        matches!(
            failure.err,
            TransactionError::InstructionError(
                _,
                solana_sdk::instruction::InstructionError::Custom(code),
            ) if code == u32::from(anchor_lang::error::ErrorCode::AccountNotInitialized)
        ),
        "expected AccountNotInitialized, got {:?}; logs:\n{}",
        failure.err,
        failure.meta.pretty_logs()
    );
}

/// Liveness: if the winner's recorded account is gone by settlement time, the
/// payout still lands — but only in their canonical ATA, which anyone can
/// re-create permissionlessly.
#[test]
fn settle_fallback_pays_any_winner_owned_account() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2), // host wins
    );
    // The recorded account is closed after the join (0 lamports, no data).
    svm.set_account(j.fixture.host_token_account, SolanaAccount::default())
        .unwrap();
    let host_ata = get_associated_token_address(&j.fixture.host.pubkey(), &j.fixture.mint);
    write_token_account_at(
        &mut svm,
        host_ata,
        j.fixture.mint,
        j.fixture.host.pubkey(),
        0,
    );

    send_ok(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback_full(
            &j,
            payer.pubkey(),
            host_ata,
            j.joiner_token_account,
            j.treasury_token_account,
        )],
    );

    assert_eq!(token_balance(&svm, &host_ata), PAYOUT);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), FEE);
}

/// ...and nothing else: a cranker cannot redirect the payout into some other
/// account the winner happens to own (which could carry a delegate).
#[test]
fn settle_fallback_rejects_non_ata_payout_account() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2), // host wins
    );
    // Host-owned, right mint — but neither the recorded account nor the ATA.
    let side_account = create_token_account(&mut svm, j.fixture.mint, j.fixture.host.pubkey(), 0);

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback_full(
            &j,
            payer.pubkey(),
            side_account,
            j.joiner_token_account,
            j.treasury_token_account,
        )],
    );
    assert_coinflip_error(
        result,
        coinflip::errors::CoinflipError::InvalidPayoutAccount,
    );
    assert_eq!(token_balance(&svm, &j.fixture.escrow), POT);
}

/// The fallback pays the CURRENT treasury: once the admin rotates it, the old
/// treasury's account is no longer an acceptable fee destination.
#[test]
fn settle_fallback_rejects_stale_treasury() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2),
    );
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            None,
            Some(Pubkey::new_unique()),
            None,
            None,
        )],
    );

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback(&j, payer.pubkey())],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

/// ...and the rotated treasury's account works, ATA or not: the constraint is
/// owner + mint, not the associated-token derivation.
#[test]
fn settle_fallback_pays_rotated_treasury() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2),
    );
    let new_treasury = Pubkey::new_unique();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            None,
            Some(new_treasury),
            None,
            None,
        )],
    );
    let new_treasury_ta = create_token_account(&mut svm, j.fixture.mint, new_treasury, 0);

    send_ok(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback_full(
            &j,
            payer.pubkey(),
            j.fixture.host_token_account,
            j.joiner_token_account,
            new_treasury_ta,
        )],
    );

    assert_eq!(token_balance(&svm, &new_treasury_ta), FEE);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), 0);
    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        REMAINING + PAYOUT
    );
}

/// The pot is the escrow's ACTUAL balance: tokens anyone donated straight into
/// the escrow are paid out (and fee'd) too, so nothing is ever stranded and
/// the close can never fail on a non-zero balance.
#[test]
fn settlement_drains_donated_dust_to_winner() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);

    const DUST: u64 = 12_345_678;
    let donor = Keypair::new();
    svm.airdrop(&donor.pubkey(), 10_000_000_000).unwrap();
    let donor_ta = create_token_account(&mut svm, j.fixture.mint, donor.pubkey(), DUST);
    send_ok(
        &mut svm,
        &[&donor],
        &[spl_token::instruction::transfer_checked(
            &spl_token::ID,
            &donor_ta,
            &j.fixture.mint,
            &j.fixture.escrow,
            &donor.pubkey(),
            &[],
            DUST,
            9,
        )
        .unwrap()],
    );
    assert_eq!(token_balance(&svm, &j.fixture.escrow), POT + DUST);

    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2), // host wins
    );
    let ix = ix_settle_fallback(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    // Fee is charged on the FULL escrow balance, not on 2 * stake.
    let pot = POT + DUST; // 10_012_345_678
    let fee = pot / 100; // 100_123_456 (floor, 1%)
    let payout = pot - fee; // 9_912_222_222
    assert_eq!(fee, 100_123_456);
    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        REMAINING + payout
    );
    assert_eq!(token_balance(&svm, &j.treasury_token_account), fee);
    assert!(
        is_gone(&svm, &j.fixture.escrow),
        "escrow must close even with donated dust"
    );

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.pot, pot);
    assert_eq!(ev.fee, fee);
}

/// The request is bound to the game by `game.vrf_seed`: another game's
/// (fulfilled) request cannot be substituted to settle this one early.
#[test]
fn settle_rejects_foreign_request() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);

    // A second game on the same config/ORAO client, joined and fulfilled.
    let host_b = Keypair::new();
    svm.airdrop(&host_b.pubkey(), 10_000_000_000).unwrap();
    let host_b_ta = create_token_account(&mut svm, j.fixture.mint, host_b.pubkey(), STAKE * 10);
    let game_b = Keypair::new();
    let game_b_key = game_b.pubkey();
    send_ok(
        &mut svm,
        &[&host_b, &game_b],
        &[ix_create_game(
            host_b.pubkey(),
            game_b_key,
            j.fixture.mint,
            host_b_ta,
            0,
            STAKE,
        )],
    );
    let f_b = GameFixture {
        host: host_b,
        game: game_b,
        mint: j.fixture.mint,
        host_token_account: host_b_ta,
        escrow: escrow_pda(&game_b_key),
        treasury: j.fixture.treasury,
        amount: STAKE,
    };
    let joiner_b = Keypair::new();
    svm.airdrop(&joiner_b.pubkey(), 10_000_000_000).unwrap();
    let joiner_b_ta = create_token_account(&mut svm, f_b.mint, joiner_b.pubkey(), STAKE * 10);
    send_ok(
        &mut svm,
        &[&joiner_b],
        &[ix_join_game(&f_b, &j.orao, joiner_b.pubkey(), joiner_b_ta)],
    );
    let request_b = write_fulfilled_request(
        &mut svm,
        j.orao.client,
        vrf_seed_for(&game_b_key, &joiner_b.pubkey()),
        randomness_with_first_byte(2),
    );

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_fallback_with_request(
            &j,
            payer.pubkey(),
            request_b,
            j.fixture.host_token_account,
            j.joiner_token_account,
            j.treasury_token_account,
        )],
    );
    assert_anchor_error(result, anchor_lang::error::ErrorCode::ConstraintSeeds);
    assert_eq!(
        token_balance(&svm, &j.fixture.escrow),
        POT,
        "game A's escrow must be untouched"
    );
}

/// A zero-fee game pays the whole pot out and never touches the treasury.
#[test]
fn zero_fee_game_settles_full_pot() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game_with_fee(&mut svm, &payer, STAKE, 0);
    write_fulfilled_request(
        &mut svm,
        j.orao.client,
        j.vrf_seed,
        randomness_with_first_byte(2), // host wins
    );

    let ix = ix_settle_fallback(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        REMAINING + POT
    );
    assert_eq!(token_balance(&svm, &j.treasury_token_account), 0);
    assert!(is_gone(&svm, &j.fixture.escrow), "escrow must be closed");

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.pot, POT);
    assert_eq!(ev.fee, 0);
}
