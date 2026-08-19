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
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));

    let game_key = j.fixture.game.pubkey();
    let host = j.fixture.host.pubkey();
    // Both rents belong to the host; the settle payer is a third party, so the
    // host's lamport delta is game rent + escrow rent + whatever is left of the
    // bond after the losing host reimburses the joiner.
    let game_account = svm.get_account(&game_key).unwrap();
    let game_rent = svm.minimum_balance_for_rent_exemption(game_account.data.len());
    let bond = expected_bond(&svm);
    let sunk = expected_joiner_sunk(&svm, REQUEST_FEE);
    assert_eq!(game_account.lamports, game_rent + bond);
    assert_eq!(read_game(&svm, &game_key).joiner_sunk_lamports, sunk);
    let escrow_rent = svm.get_account(&j.fixture.escrow).unwrap().lamports;
    let host_lamports_before = svm.get_account(&host).unwrap().lamports;
    let joiner_lamports_before = svm.get_account(&j.joiner.pubkey()).unwrap().lamports;

    let ix = ix_settle(&j, payer.pubkey());
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
    // The losing joiner is out their stake and NOTHING else: every lamport the
    // join cost them comes back from the winning host's bond. They are not a
    // signer here, so there is no tx fee to net out either.
    assert_eq!(
        svm.get_account(&j.joiner.pubkey()).unwrap().lamports - joiner_lamports_before,
        sunk,
        "the losing joiner must be made whole on their join-time lamports"
    );
    assert_eq!(
        svm.get_account(&host).unwrap().lamports,
        host_lamports_before + game_rent + escrow_rent + bond - sunk,
        "the host recovers both rents plus what is left of the bond"
    );

    // Budget guard: the request PDA derivation + two transfers + a close + the
    // reimbursement + the event CPI (measured ~41k with both payout accounts
    // recorded; the ATA-fallback path derives two more).
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
    assert_eq!(ev.joiner_reimbursed, sunk);
}

#[test]
fn settle_pays_joiner_when_host_side_loses() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // randomness[0] odd => Tails => joiner (host picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(3));

    let game_key = j.fixture.game.pubkey();
    let host = j.fixture.host.pubkey();
    let game_account = svm.get_account(&game_key).unwrap();
    let game_rent = svm.minimum_balance_for_rent_exemption(game_account.data.len());
    let bond = expected_bond(&svm);
    let escrow_rent = svm.get_account(&j.fixture.escrow).unwrap().lamports;
    let host_lamports_before = svm.get_account(&host).unwrap().lamports;
    let joiner_lamports_before = svm.get_account(&j.joiner.pubkey()).unwrap().lamports;

    let ix = ix_settle(&j, payer.pubkey());
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

    // The winning joiner bore their own join-time costs — that is the other
    // half of "the loser pays only their stake" — so the whole bond goes home
    // with the losing host, who is out nothing but the stake.
    assert_eq!(
        svm.get_account(&j.joiner.pubkey()).unwrap().lamports,
        joiner_lamports_before,
        "a winning joiner is never reimbursed"
    );
    assert_eq!(
        svm.get_account(&host).unwrap().lamports,
        host_lamports_before + game_rent + escrow_rent + bond,
        "the full bond returns to the host"
    );

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.winner, j.joiner.pubkey());
    assert_eq!(ev.outcome, u8::from(coinflip::state::Side::Tails));
    assert_eq!(ev.joiner_reimbursed, 0);
}

/// The bond is sized from ORAO's fee at CREATE time. If that fee is raised
/// before the join, the joiner sinks more than the bond covers — the
/// reimbursement then caps at the bond rather than eating into the rents (which
/// would leave the game account short of what `close` owes the host).
#[test]
fn under_bonded_game_reimburses_up_to_the_bond() {
    let (mut svm, payer) = setup();
    let (f, _create_meta) = setup_open_game(&mut svm, &payer, STAKE);
    let bond = expected_bond(&svm);
    assert_eq!(read_game(&svm, &f.game.pubkey()).bond_lamports, bond);

    // ORAO's authority raises the fee after the game was bonded.
    let orao = setup_orao(&mut svm);
    let raised_fee = REQUEST_FEE * 5;
    set_orao_request_fee(&mut svm, &orao, raised_fee);

    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), STAKE * 10);
    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );

    let vrf_seed = vrf_seed_for(&f.game.pubkey(), &joiner.pubkey(), 0);
    let sunk = expected_joiner_sunk(&svm, raised_fee);
    assert_eq!(read_game(&svm, &f.game.pubkey()).joiner_sunk_lamports, sunk);
    assert!(sunk > bond, "this game must actually be under-bonded");

    let treasury_token_account = get_associated_token_address(&treasury(), &f.mint);
    let j = JoinedGame {
        fixture: f,
        joiner,
        joiner_token_account: joiner_ta,
        treasury_token_account,
        orao,
        request: request_pda(&vrf_seed),
        vrf_seed,
    };
    // randomness[0] even => Heads => the host (who picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));

    let game_key = j.fixture.game.pubkey();
    let host = j.fixture.host.pubkey();
    let game_rent =
        svm.minimum_balance_for_rent_exemption(svm.get_account(&game_key).unwrap().data.len());
    let escrow_rent = svm.get_account(&j.fixture.escrow).unwrap().lamports;
    let host_lamports_before = svm.get_account(&host).unwrap().lamports;
    let joiner_lamports_before = svm.get_account(&j.joiner.pubkey()).unwrap().lamports;

    let ix = ix_settle(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    assert_eq!(
        svm.get_account(&j.joiner.pubkey()).unwrap().lamports - joiner_lamports_before,
        bond,
        "reimbursement caps at the bond, not at what the joiner actually sank"
    );
    assert_eq!(
        svm.get_account(&host).unwrap().lamports,
        host_lamports_before + game_rent + escrow_rent,
        "the whole bond is spent; the rents are untouched"
    );

    let ev = find_cpi_event::<coinflip::events::GameSettled>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameSettled not emitted");
    assert_eq!(ev.joiner_reimbursed, bond);
}

#[test]
fn settle_before_fulfillment_fails() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // Request exists (real, pending) but is NOT fulfilled.
    let result = send(&mut svm, &[&payer], &[ix_settle(&j, payer.pubkey())]);
    assert_coinflip_error(
        result,
        coinflip::errors::CoinflipError::RandomnessNotFulfilled,
    );
}

#[test]
fn settle_twice_fails() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(0));
    send_ok(&mut svm, &[&payer], &[ix_settle(&j, payer.pubkey())]);

    // The game account is closed, so a second settle can't even load it — and
    // `send` expires the blockhash, so this really is a fresh transaction and
    // not a duplicate the runtime dropped before execution.
    let result = send(&mut svm, &[&payer], &[ix_settle(&j, payer.pubkey())]);
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
fn settle_pays_ata_when_recorded_account_is_gone() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // randomness[0] even => Heads => the host (who picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));
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
        &[ix_settle_full(
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
fn settle_rejects_non_ata_payout_account() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    // randomness[0] even => Heads => the host (who picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));
    // Host-owned, right mint — but neither the recorded account nor the ATA.
    let side_account = create_token_account(&mut svm, j.fixture.mint, j.fixture.host.pubkey(), 0);

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_full(
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

/// The fee destination is pinned to the constant treasury's canonical ATA, by
/// derivation: neither someone else's account nor another account the treasury
/// itself owns is an acceptable place for a cranker to route fees.
#[test]
fn settle_rejects_non_treasury_fee_account() {
    let (mut svm, payer) = setup();
    let (j, _join_meta) = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));
    // Right mint, real token accounts — one owned by a random key, one owned by
    // the treasury itself but sitting at a non-ATA address.
    let mallory_ta = create_token_account(&mut svm, j.fixture.mint, Pubkey::new_unique(), 0);
    let treasury_side_account = create_token_account(&mut svm, j.fixture.mint, treasury(), 0);

    for fee_account in [mallory_ta, treasury_side_account] {
        let result = send(
            &mut svm,
            &[&payer],
            &[ix_settle_full(
                &j,
                payer.pubkey(),
                j.fixture.host_token_account,
                j.joiner_token_account,
                fee_account,
            )],
        );
        // The ATA derivation is checked before the mint, so both cases land on
        // the same error.
        assert_coinflip_error(
            result,
            coinflip::errors::CoinflipError::InvalidPayoutAccount,
        );
        assert_eq!(token_balance(&svm, &fee_account), 0);
    }
    assert_eq!(
        token_balance(&svm, &j.fixture.escrow),
        POT,
        "escrow must be untouched"
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

    // randomness[0] even => Heads => the host (who picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));
    let ix = ix_settle(&j, payer.pubkey());
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

    // A second game on the same config, joined and fulfilled.
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
        vrf_seed_for(&game_b_key, &joiner_b.pubkey(), 0),
        randomness_with_first_byte(2),
    );

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_settle_with_request(
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
    // randomness[0] even => Heads => the host (who picked Heads) wins.
    write_fulfilled_request(&mut svm, j.vrf_seed, randomness_with_first_byte(2));

    let ix = ix_settle(&j, payer.pubkey());
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
