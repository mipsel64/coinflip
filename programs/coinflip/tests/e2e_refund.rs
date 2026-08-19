mod common;

use anchor_spl::{associated_token::get_associated_token_address, token::spl_token};
use common::*;
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account as SolanaAccount,
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

const STAKE: u64 = 1_000_000;
/// `setup_open_game` funds both players with `10 * amount`.
const FUNDED: u64 = STAKE * 10;
const POT: u64 = STAKE * 2;
/// LiteSVM starts at slot 0, where a `joined_at_slot` that never got written is
/// indistinguishable from one that did; join from a non-zero slot instead.
const JOIN_SLOT: u64 = 1_000;
/// Last slot at which a refund is still refused: the program requires
/// `clock.slot > joined_at_slot + refund_timeout_slots`.
const DEADLINE: u64 = JOIN_SLOT + DEFAULT_TIMEOUT_SLOTS;

fn is_gone(svm: &LiteSVM, address: &Pubkey) -> bool {
    svm.get_account(address).is_none_or(|a| a.lamports == 0)
}

/// A joined game whose deadline is the `DEADLINE` constant (asserted against
/// what the program actually recorded).
fn setup_joined_at_known_slot(svm: &mut LiteSVM, payer: &Keypair) -> JoinedGame {
    svm.warp_to_slot(JOIN_SLOT);
    let (j, _join_meta) = setup_joined_game(svm, payer, STAKE);
    assert_eq!(
        read_game(svm, &j.fixture.game.pubkey()).joined_at_slot,
        JOIN_SLOT,
    );
    j
}

/// Randomness never arrived: anyone may unwind the game, and each player gets
/// exactly their own stake back — no fee, no coin flip.
#[test]
fn refund_after_timeout_returns_both_stakes() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);
    let game_key = j.fixture.game.pubkey();
    let host = j.fixture.host.pubkey();
    // Both rents belong to the host; the cranker is a third party, so the
    // host's lamport delta is exactly game rent + escrow rent.
    let game_rent = svm.get_account(&game_key).unwrap().lamports;
    let escrow_rent = svm.get_account(&j.fixture.escrow).unwrap().lamports;
    let host_lamports_before = svm.get_account(&host).unwrap().lamports;

    svm.warp_to_slot(DEADLINE + 1);
    let ix = ix_refund_timeout(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), FUNDED);
    assert_eq!(token_balance(&svm, &j.joiner_token_account), FUNDED);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), 0);
    assert!(is_gone(&svm, &j.fixture.escrow), "escrow must be closed");
    assert!(is_gone(&svm, &game_key), "game must be closed");
    assert_eq!(
        svm.get_account(&host).unwrap().lamports,
        host_lamports_before + game_rent + escrow_rent,
        "both rents must return to the host"
    );

    let ev = find_cpi_event::<coinflip::events::GameRefunded>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameRefunded not emitted");
    assert_eq!(ev.game, game_key);
    assert_eq!(ev.host, host);
    assert_eq!(ev.joiner, j.joiner.pubkey());
    assert_eq!(ev.mint, j.fixture.mint);
    assert_eq!(ev.host_refund, STAKE);
    assert_eq!(ev.joiner_refund, STAKE);
}

/// The deadline is read from the game's own snapshot, so an admin who raises
/// (or lowers) the configured timeout mid-flight cannot move a live game's
/// refund window: a live read here would still say TimeoutNotReached.
#[test]
fn refund_uses_joined_timeout_snapshot() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);
    assert_eq!(
        read_game(&svm, &j.fixture.game.pubkey()).refund_timeout_slots,
        DEFAULT_TIMEOUT_SLOTS,
        "join must snapshot the configured timeout"
    );

    send_ok(
        &mut svm,
        &[&payer],
        &[ix_update_config(
            payer.pubkey(),
            None,
            None,
            Some(coinflip::constants::MAX_REFUND_TIMEOUT_SLOTS),
        )],
    );

    svm.warp_to_slot(DEADLINE + 1);
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), FUNDED);
    assert_eq!(token_balance(&svm, &j.joiner_token_account), FUNDED);
}

/// The escape hatch stays shut while the oracle still has time to answer.
#[test]
fn refund_before_timeout_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::TimeoutNotReached);
    assert_eq!(token_balance(&svm, &j.fixture.escrow), POT);
}

/// The comparison is strict: the deadline slot itself is still too early, and
/// the very next slot opens the window.
#[test]
fn refund_exactly_at_deadline_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);

    svm.warp_to_slot(DEADLINE);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::TimeoutNotReached);

    svm.warp_to_slot(DEADLINE + 1);
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), FUNDED);
    assert_eq!(token_balance(&svm, &j.joiner_token_account), FUNDED);
}

/// Once randomness exists the game has an outcome, so it must be settled on it
/// — a late cranker cannot refund the loser's stake back to them.
#[test]
fn refund_of_fulfilled_request_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);
    write_fulfilled_request(&mut svm, j.vrf_seed, [1u8; 64]);

    svm.warp_to_slot(DEADLINE + 1);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::AlreadyFulfilled);
    assert_eq!(token_balance(&svm, &j.fixture.escrow), POT);
}

/// The host is refunded their recorded stake and the joiner takes the rest, so
/// tokens donated straight into the escrow can never brick the close.
#[test]
fn refund_returns_dust_to_joiner() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);

    const DUST: u64 = 12_345;
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

    svm.warp_to_slot(DEADLINE + 1);
    let ix = ix_refund_timeout(&j, payer.pubkey());
    let meta = send_ok(&mut svm, &[&payer], std::slice::from_ref(&ix));

    assert_eq!(
        token_balance(&svm, &j.fixture.host_token_account),
        FUNDED,
        "host gets exactly their stake, never the dust"
    );
    assert_eq!(token_balance(&svm, &j.joiner_token_account), FUNDED + DUST);
    assert!(
        is_gone(&svm, &j.fixture.escrow),
        "escrow must close even with donated dust"
    );

    let ev = find_cpi_event::<coinflip::events::GameRefunded>(
        std::slice::from_ref(&ix),
        &payer.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameRefunded not emitted");
    assert_eq!(ev.host_refund, STAKE);
    assert_eq!(
        ev.joiner_refund,
        STAKE + DUST,
        "the event must report what each side actually received"
    );
}

/// Liveness: a recorded account that is gone by refund time must not strand the
/// stake — the player's canonical ATA (permissionlessly re-creatable) works too.
#[test]
fn refund_pays_ata_when_recorded_account_is_gone() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);
    // The joiner's recorded account is closed after the join.
    svm.set_account(j.joiner_token_account, SolanaAccount::default())
        .unwrap();
    let joiner_ata = get_associated_token_address(&j.joiner.pubkey(), &j.fixture.mint);
    write_token_account_at(&mut svm, joiner_ata, j.fixture.mint, j.joiner.pubkey(), 0);

    svm.warp_to_slot(DEADLINE + 1);
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout_full(
            &j,
            payer.pubkey(),
            j.fixture.host_token_account,
            joiner_ata,
        )],
    );

    assert_eq!(token_balance(&svm, &joiner_ata), STAKE);
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), FUNDED);
    assert!(is_gone(&svm, &j.fixture.escrow), "escrow must be closed");
}

/// ...and nothing else: a cranker cannot redirect a stake into some other
/// account the player happens to own (which could carry a delegate).
#[test]
fn refund_rejects_non_ata_payout_account() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);
    // Joiner-owned, right mint — but neither the recorded account nor the ATA.
    let side_account = create_token_account(&mut svm, j.fixture.mint, j.joiner.pubkey(), 0);

    svm.warp_to_slot(DEADLINE + 1);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout_full(
            &j,
            payer.pubkey(),
            j.fixture.host_token_account,
            side_account,
        )],
    );
    assert_coinflip_error(
        result,
        coinflip::errors::CoinflipError::InvalidPayoutAccount,
    );
    assert_eq!(token_balance(&svm, &j.fixture.escrow), POT);
}

/// The refund closes the game, so it can never pay out twice.
#[test]
fn refund_twice_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);

    svm.warp_to_slot(DEADLINE + 1);
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );

    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    assert_anchor_error(result, anchor_lang::error::ErrorCode::AccountNotInitialized);
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), FUNDED);
    assert_eq!(token_balance(&svm, &j.joiner_token_account), FUNDED);
}

/// The request is bound to the game by `game.vrf_seed`: another game's (still
/// pending) request cannot stand in for this game's un-fulfilled one.
#[test]
fn refund_rejects_foreign_request() {
    let (mut svm, payer) = setup();
    let j = setup_joined_at_known_slot(&mut svm, &payer);

    // A second game on the same config, joined (so its request exists and is
    // pending, exactly like game A's).
    let host_b = Keypair::new();
    svm.airdrop(&host_b.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap();
    let host_b_ta = create_token_account(&mut svm, j.fixture.mint, host_b.pubkey(), FUNDED);
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
    svm.airdrop(&joiner_b.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap();
    let joiner_b_ta = create_token_account(&mut svm, f_b.mint, joiner_b.pubkey(), FUNDED);
    send_ok(
        &mut svm,
        &[&joiner_b],
        &[ix_join_game(&f_b, &j.orao, joiner_b.pubkey(), joiner_b_ta)],
    );
    let request_b = request_pda(&vrf_seed_for(&game_b_key, &joiner_b.pubkey()));

    svm.warp_to_slot(DEADLINE + 1);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout_with_request(
            &j,
            payer.pubkey(),
            request_b,
            j.fixture.host_token_account,
            j.joiner_token_account,
        )],
    );
    assert_anchor_error(result, anchor_lang::error::ErrorCode::ConstraintSeeds);
    assert_eq!(
        token_balance(&svm, &j.fixture.escrow),
        POT,
        "game A's escrow must be untouched"
    );
}

/// An Open game has no joiner and no request, so there is nothing to time out.
#[test]
fn refund_of_open_game_fails() {
    let (mut svm, payer) = setup();
    let (fixture, _create_meta) = setup_open_game(&mut svm, &payer, STAKE);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    let joiner_token_account = create_token_account(&mut svm, fixture.mint, joiner.pubkey(), 0);
    let treasury_token_account = get_associated_token_address(&treasury(), &fixture.mint);
    // An unjoined game carries a zeroed vrf_seed, so the request PDA its seeds
    // resolve to was never created.
    let vrf_seed = [0u8; 32];
    let j = JoinedGame {
        fixture,
        joiner,
        joiner_token_account,
        treasury_token_account,
        request: request_pda(&vrf_seed),
        orao,
        vrf_seed,
    };

    svm.warp_to_slot(DEADLINE + 1);
    let result = send(
        &mut svm,
        &[&payer],
        &[ix_refund_timeout(&j, payer.pubkey())],
    );
    // Account resolution runs before the handler, so the missing request
    // account — not `require_state` — is what rejects this.
    assert_anchor_error(result, anchor_lang::error::ErrorCode::AccountNotInitialized);
    assert_eq!(token_balance(&svm, &j.fixture.escrow), STAKE);
}
