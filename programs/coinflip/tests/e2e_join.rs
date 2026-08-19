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

    let joiner_before = svm.get_account(&joiner.pubkey()).unwrap().lamports;

    let ix = ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta);
    let meta = send_ok(&mut svm, &[&joiner], std::slice::from_ref(&ix));

    // Both stakes escrowed; the joiner's account was debited exactly once.
    assert_eq!(token_balance(&svm, &f.escrow), stake * 2);
    assert_eq!(token_balance(&svm, &joiner_ta), stake * 10 - stake);

    let vrf_seed = vrf_seed_for(&f.game.pubkey(), &joiner.pubkey(), 0);
    let request_addr = request_pda(&vrf_seed);
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
    assert_eq!(request.owner, orao_solana_vrf::ID);
    assert_eq!(
        request.data.len(),
        PENDING_REQUEST_LEN,
        "ORAO allocates 8 + RandomnessV2::PENDING_SIZE for a pending request"
    );
    let request_rent = svm.minimum_balance_for_rent_exemption(request.data.len());
    assert_eq!(request.lamports, request_rent);
    // The joiner is ORAO's `client` for this request: whatever the oracle
    // refunds at fulfillment goes back to them, not to this program.
    let pending = read_request(&svm, &request_addr);
    assert_eq!(*pending.client(), joiner.pubkey());
    assert_eq!(*pending.seed(), vrf_seed);
    assert!(pending.fulfilled().is_none(), "request must start pending");

    // The joiner paid ORAO's request fee straight into ORAO's treasury — no
    // Client PDA float, no reimbursement leg.
    assert_eq!(
        svm.get_account(&orao.orao_treasury).unwrap().lamports,
        ORAO_TREASURY_START_LAMPORTS + REQUEST_FEE
    );

    // The treasury ATA exists ahead of settlement, so no cranker ever pays for it.
    let treasury_ata = get_associated_token_address(&treasury(), &f.mint);
    let treasury_ata_rent = svm
        .get_account(&treasury_ata)
        .expect("treasury ATA must exist")
        .lamports;
    assert_eq!(token_balance(&svm, &treasury_ata), 0);

    // Everything the joiner spends in lamports (i.e. stake aside): ORAO's
    // request fee, the request account's rent, the treasury ATA's rent it
    // pre-pays, and the one-signature tx fee.
    let tx_fee = 5_000;
    assert_eq!(
        joiner_before - svm.get_account(&joiner.pubkey()).unwrap().lamports,
        REQUEST_FEE + request_rent + treasury_ata_rent + tx_fee
    );

    // Budget guard: token transfer + ATA init + the ORAO CPI must stay well
    // inside one transaction's compute budget (measured ~75k).
    assert!(
        meta.compute_units_consumed < 100_000,
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

/// Token-2022 end to end: create and join a game whose mint carries an allowed
/// extension, with every token account (escrow, players, treasury ATA) living
/// under the Token-2022 program.
#[test]
fn t22_game_full_join() {
    let (mut svm, payer) = setup();
    send_ok(
        &mut svm,
        &[&payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
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
        get_associated_token_address_with_program_id(&treasury(), &mint, &token_program);
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

/// The fee destination must be the treasury's canonical ATA — the address
/// `settle` will pin — not any account the treasury happens to own.
#[test]
fn join_with_non_ata_treasury_account_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);
    // Treasury-owned and the right mint, but not at the ATA address.
    let fake_treasury_ta = create_token_account(&mut svm, f.mint, treasury(), 0);

    let mut ix = ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta);
    let ata_slot = ix
        .accounts
        .iter()
        .position(|meta| meta.pubkey == get_associated_token_address(&treasury(), &f.mint))
        .expect("treasury ATA slot");
    ix.accounts[ata_slot].pubkey = fake_treasury_ta;

    // Anchor's associated-token address constraint (error code 3014).
    assert_anchor_error(
        send(&mut svm, &[&joiner], &[ix]),
        anchor_lang::error::ErrorCode::AccountNotAssociatedTokenAccount,
    );
}

/// Plain VRF creates the request with Anchor's `init`, which absorbs a
/// pre-existing lamport balance instead of failing on it — so the one-lamport
/// grief that the callback VRF's raw `create_account` was vulnerable to does
/// not block a join here, at the real address or any other.
#[test]
fn pre_funded_request_address_does_not_block_join() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);

    // Both the address the old (game-keyed) scheme would have used and the one
    // this join will ACTUALLY use.
    svm.airdrop(&request_pda(&f.game.pubkey().to_bytes()), 1)
        .unwrap();
    let real = request_pda(&vrf_seed_for(&f.game.pubkey(), &joiner.pubkey(), 0));
    svm.airdrop(&real, 1).unwrap();

    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );
    let game = read_game(&svm, &f.game.pubkey());
    assert_eq!(game.state, u8::from(GameState::AwaitingRandomness));
}

/// Requests live in ORAO's GLOBAL namespace, so anyone may create one for any
/// seed — an attacker who can predict the seed can front-run the join and make
/// it fail. The `nonce` is the in-protocol recovery: the same joiner retries at
/// nonce+1, which is a completely different address, and the attacker has to
/// win the race all over again (paying ORAO's fee and the request's rent each
/// time) to keep the block up.
#[test]
fn front_run_request_is_recovered_by_bumping_the_nonce() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), 10_000);

    // The attacker knows (or guesses) the joiner, so they can derive nonce 0's
    // seed and take that address first.
    let mallory = Keypair::new();
    svm.airdrop(&mallory.pubkey(), 10_000_000_000).unwrap();
    let seed = vrf_seed_for(&f.game.pubkey(), &joiner.pubkey(), 0);
    send_ok(
        &mut svm,
        &[&mallory],
        &[ix_orao_request_v2(&orao, mallory.pubkey(), seed)],
    );

    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game(&f, &orao, joiner.pubkey(), joiner_ta)],
    );
    // Anchor's `init` falls back to allocate+assign on a pre-funded address,
    // and the system program refuses to allocate an account that already has
    // data: SystemError::AccountAlreadyInUse == Custom(0). (Anchor-level codes
    // are all >= 2000, so this one is unambiguous.)
    assert!(
        matches!(
            result.unwrap_err().err,
            TransactionError::InstructionError(_, InstructionError::Custom(0))
        ),
        "the join must fail once the request account already exists"
    );
    assert_eq!(
        read_game(&svm, &f.game.pubkey()).state,
        u8::from(GameState::Open),
        "the blocked join must leave the game untouched"
    );

    // The recovery, with no help from anyone: same game, same joiner, nonce 1.
    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game_with_nonce(
            &f,
            &orao,
            joiner.pubkey(),
            joiner_ta,
            1,
        )],
    );
    let game = read_game(&svm, &f.game.pubkey());
    assert_eq!(game.state, u8::from(GameState::AwaitingRandomness));
    assert_eq!(
        game.vrf_seed,
        vrf_seed_for(&f.game.pubkey(), &joiner.pubkey(), 1),
        "the game must record the seed it actually requested"
    );
    // ...and that request is ours, not the attacker's.
    let request = read_request(&svm, &request_pda(&game.vrf_seed));
    assert_eq!(*request.client(), joiner.pubkey());
}

/// ORAO's request fee is live config its authority can raise at any time, and
/// the joiner pays it directly — so the joiner states a ceiling and the join
/// fails closed rather than silently overpaying.
#[test]
fn join_rejects_vrf_fee_above_the_callers_maximum() {
    let (mut svm, payer) = setup();
    let stake = 1_000;
    let (f, _meta) = setup_open_game(&mut svm, &payer, stake);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let joiner_ta = create_token_account(&mut svm, f.mint, joiner.pubkey(), stake * 10);

    let result = send(
        &mut svm,
        &[&joiner],
        &[ix_join_game_full(
            &f,
            &orao,
            joiner.pubkey(),
            joiner_ta,
            anchor_spl::token::ID,
            0,
            REQUEST_FEE - 1,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::VrfFeeTooHigh);

    // Nothing moved: no stake escrowed, no request created.
    assert_eq!(token_balance(&svm, &f.escrow), stake);
    assert_eq!(token_balance(&svm, &joiner_ta), stake * 10);
    assert_eq!(
        read_game(&svm, &f.game.pubkey()).state,
        u8::from(GameState::Open)
    );
    assert!(
        svm.get_account(&request_pda(&vrf_seed_for(
            &f.game.pubkey(),
            &joiner.pubkey(),
            0
        )))
        .is_none(),
        "a rejected join must not leave a request account behind"
    );

    // Exactly at the cap it goes through.
    send_ok(
        &mut svm,
        &[&joiner],
        &[ix_join_game_full(
            &f,
            &orao,
            joiner.pubkey(),
            joiner_ta,
            anchor_spl::token::ID,
            0,
            REQUEST_FEE,
        )],
    );
    assert_eq!(
        read_game(&svm, &f.game.pubkey()).state,
        u8::from(GameState::AwaitingRandomness)
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
