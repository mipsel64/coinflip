mod common;

use anchor_spl::associated_token::{
    get_associated_token_address, get_associated_token_address_with_program_id,
};
use coinflip::state::GameState;
use common::*;
use solana_sdk::{
    instruction::InstructionError,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::TransactionError,
};

/// The join is built inline here rather than through `setup_joined_game`, so
/// the pre-join lamport balances stay observable: this is the test that pins
/// who pays for what.
#[test]
fn join_escrows_stake_and_creates_vrf_request() {
    let (mut svm, payer) = setup();
    // LiteSVM starts at slot 0, where a `joined_at_slot` that never got
    // written is indistinguishable from one that did; warp first.
    let join_slot = 4_321;
    svm.warp_to_slot(join_slot);
    let stake = 5_000_000_000;
    let (f, _create_meta) = setup_open_game(&mut svm, &payer, stake);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), stake * 10);

    let client_before = svm.get_account(&orao.client).unwrap().lamports;
    let joiner_before = svm.get_account(&joiner.pubkey()).unwrap().lamports;

    let ix = ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta);
    let meta = send_ok(&mut svm, &[&joiner], std::slice::from_ref(&ix));

    // Both stakes escrowed; the joiner's account was debited exactly once.
    assert_eq!(token_balance(&svm, &f.escrow), stake * 2);
    assert_eq!(token_balance(&svm, &joiner_ta), stake * 10 - stake);

    let vrf_seed = vrf_seed_for(&f.game.pubkey(), &joiner.pubkey());
    let request_addr = request_pda(&orao.client, &vrf_seed);
    let game = read_game(&svm, &f.game.pubkey());
    assert_eq!(game.state, u8::from(GameState::AwaitingRandomness));
    assert_eq!(game.joiner, joiner.pubkey());
    assert_eq!(game.joiner_token_account, joiner_ta);
    assert_eq!(game.joined_at_slot, join_slot);
    assert_eq!(game.vrf_seed, vrf_seed);
    // The refund deadline is fixed here, from live config, and never re-read.
    assert_eq!(game.refund_timeout_slots, DEFAULT_TIMEOUT_SLOTS);

    // The real ORAO program created the request account, rent-funded, at the
    // address derived from the hashed seed.
    let request = svm
        .get_account(&request_addr)
        .expect("request account must exist");
    assert_eq!(request.owner, orao_solana_vrf_cb::ID);
    let request_rent = svm.minimum_balance_for_rent_exemption(request.data.len());
    assert_eq!(request.lamports, request_rent);

    // The joiner reimbursed the VRF fee, which ORAO moved on to its treasury.
    assert_eq!(
        svm.get_account(&orao.orao_treasury).unwrap().lamports,
        ORAO_TREASURY_START_LAMPORTS + REQUEST_FEE
    );

    // The shared Client PDA comes out exactly neutral: it paid the fee and the
    // request's rent and was reimbursed for precisely that, so repeated joins
    // cannot drain the balance every client of this program shares.
    assert_eq!(
        svm.get_account(&orao.client).unwrap().lamports,
        client_before
    );

    // The treasury ATA exists ahead of settlement (the callback can't pay rent).
    let treasury_ata = get_associated_token_address(&f.treasury, &f.mint);
    let treasury_ata_rent = svm
        .get_account(&treasury_ata)
        .expect("treasury ATA must exist")
        .lamports;
    assert_eq!(token_balance(&svm, &treasury_ata), 0);

    // Everything the joiner spends in lamports (i.e. stake aside): the VRF fee
    // and request rent it reimburses the client for, the treasury ATA's rent
    // it pre-pays, and the one-signature tx fee.
    let tx_fee = 5_000;
    assert_eq!(
        joiner_before - svm.get_account(&joiner.pubkey()).unwrap().lamports,
        REQUEST_FEE + request_rent + treasury_ata_rent + tx_fee
    );

    // Budget guard: token transfer + ATA init + the ORAO CPI must stay well
    // inside one transaction's compute budget.
    assert!(
        meta.compute_units_consumed < 150_000,
        "join used {} CU",
        meta.compute_units_consumed
    );

    let ev = find_cpi_event::<coinflip::events::GameJoined>(
        std::slice::from_ref(&ix),
        &joiner.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameJoined not emitted");
    assert_eq!(ev.game, f.game.pubkey());
    assert_eq!(ev.joiner, joiner.pubkey());
    assert_eq!(ev.vrf_request, request_addr);
}

/// The refund window must open a settle-margin AFTER ORAO stops retrying the
/// callback. If the oracle widens its deadline past what our config allows for,
/// joins stop rather than opening a window where a loser could read the
/// bare-fulfilled randomness and race a refund.
#[test]
fn join_rejects_timeout_below_orao_deadline_margin() {
    let (mut svm, payer) = setup();
    let stake = 5_000_000_000;
    let (f, _create_meta) = setup_open_game(&mut svm, &payer, stake);
    // 17_000 + 1_800 margin > the config's 18_000 refund timeout.
    let orao = setup_orao_with_deadline(&mut svm, 17_000);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), stake * 10);

    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidTimeout);

    // The join never happened: only the host's stake is escrowed.
    assert_eq!(token_balance(&svm, &f.escrow), stake);
    assert_eq!(token_balance(&svm, &joiner_ta), stake * 10);
    assert_eq!(
        read_game(&svm, &f.game.pubkey()).state,
        u8::from(GameState::Open)
    );
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

/// Token-2022 end to end: create and join a game whose mint carries an allowed
/// extension, with every token account (escrow, players, treasury ATA) living
/// under the Token-2022 program.
#[test]
fn t22_game_full_join() {
    let (mut svm, payer) = setup();
    let treasury = Pubkey::new_unique();
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
    let token_program = anchor_spl::token_2022::ID;
    let mint = create_t22_mint_with_extensions(&mut svm, 9, &[ExtensionType::MetadataPointer]);

    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let stake = 1_000;
    let host_ta =
        create_token_account_for_program(&mut svm, token_program, mint, host.pubkey(), stake * 10);
    let game = Keypair::new();
    let game_key = game.pubkey();
    send_ok(
        &mut svm,
        &[&host, &game],
        &[ix_create_game_with_program(
            host.pubkey(),
            game_key,
            mint,
            host_ta,
            token_program,
            0,
            stake,
        )],
    );

    let f = GameFixture {
        host,
        game,
        mint,
        host_token_account: host_ta,
        escrow: escrow_pda(&game_key),
        treasury,
        amount: stake,
    };
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account_for_program(
        &mut svm,
        token_program,
        mint,
        joiner.pubkey(),
        stake * 10,
    );
    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game_with_program(
            &f,
            &orao,
            joiner.pubkey(),
            joiner_ta,
            token_program,
        )],
    );

    assert_eq!(token_balance(&svm, &f.escrow), stake * 2);
    let game_state = read_game(&svm, &f.game.pubkey());
    assert_eq!(game_state.state, u8::from(GameState::AwaitingRandomness));
    // The treasury ATA was created under Token-2022, not the classic program.
    let treasury_ata =
        get_associated_token_address_with_program_id(&treasury, &mint, &token_program);
    assert_eq!(
        svm.get_account(&treasury_ata).unwrap().owner,
        token_program,
        "treasury ATA must live under the Token-2022 program"
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

/// The stake must come out of the joiner's own account — not one they merely
/// have a handle on.
#[test]
fn join_with_third_party_token_account_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let victim = Pubkey::new_unique();
    let victim_ta = create_token_account(&mut svm, f.mint, victim, 10_000);
    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), victim_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

/// The fee destination must be the treasury's canonical ATA — the address the
/// callback will be frozen against — not any account the treasury happens to
/// own.
#[test]
fn join_with_non_ata_treasury_account_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);
    // Treasury-owned and the right mint, but not at the ATA address.
    let fake_treasury_ta = create_token_account(&mut svm, f.mint, f.treasury, 0);

    let mut ix = ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta);
    let ata_slot = ix
        .accounts
        .iter()
        .position(|meta| meta.pubkey == get_associated_token_address(&f.treasury, &f.mint))
        .expect("treasury ATA slot");
    ix.accounts[ata_slot].pubkey = fake_treasury_ta;

    // Anchor's associated-token address constraint (error code 3014).
    assert_anchor_error(
        send(&mut svm, &[&joiner], &[ix]),
        anchor_lang::error::ErrorCode::AccountNotAssociatedTokenAccount,
    );
}

/// The reason the seed is hashed: with the seed being the game pubkey alone,
/// anyone could derive an open game's request address and pre-fund it with one
/// lamport, permanently blocking the join. That address is now irrelevant.
#[test]
fn pre_funded_game_keyed_request_address_does_not_block_join() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);

    // The address the old (game-keyed) scheme would have used.
    let guessable = request_pda(&orao.client, &f.game.pubkey().to_bytes());
    svm.airdrop(&guessable, 1).unwrap();

    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );
    let game = read_game(&svm, &f.game.pubkey());
    assert_eq!(game.state, u8::from(GameState::AwaitingRandomness));
}

/// Control for the test above: pre-funding the address the join will ACTUALLY
/// use does block it, so the defense is the seed's unpredictability, not any
/// immunity to pre-funding.
#[test]
fn pre_funded_real_request_address_blocks_join() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);

    let real = request_pda(
        &orao.client,
        &vrf_seed_for(&f.game.pubkey(), &joiner.pubkey()),
    );
    svm.airdrop(&real, 1).unwrap();

    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );
    // The system program refuses to create an account that already holds
    // lamports: SystemError::AccountAlreadyInUse == Custom(0). (Anchor-level
    // errors are all >= 6000, so this code is unambiguous.)
    assert!(
        matches!(
            result.unwrap_err().err,
            TransactionError::InstructionError(_, InstructionError::Custom(0))
        ),
        "expected the ORAO request creation to hit AccountAlreadyInUse"
    );
}

/// The token program must be the one that owns the mint: passing the other one
/// must fail rather than reach a half-executed transfer.
#[test]
fn join_with_wrong_token_program_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);
    // The game's mint is a classic SPL mint; claim it is a Token-2022 one.
    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game_with_program(
            &f,
            &orao,
            joiner.pubkey(),
            joiner_ta,
            anchor_spl::token_2022::ID,
        )],
    );
    // The treasury ATA's `init_if_needed` runs before our `token_program`
    // constraint (accounts validate in declaration order), so the associated
    // token program rejects it first; our constraint is the backstop for the
    // paths that get past it.
    assert!(
        matches!(
            result.unwrap_err().err,
            TransactionError::InstructionError(_, InstructionError::IncorrectProgramId)
        ),
        "wrong token program must be rejected"
    );
}
