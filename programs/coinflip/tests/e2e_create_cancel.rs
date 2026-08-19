mod common;

use anchor_spl::associated_token::get_associated_token_address;
use coinflip::state::{Game, GameState, Side};
use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn create_game_escrows_the_stake() {
    let (mut svm, payer) = setup();
    let stake = 5_000_000_000;
    let (fixture, meta) = setup_open_game(&mut svm, &payer, stake);
    assert_eq!(token_balance(&svm, &fixture.escrow), stake);
    assert_eq!(
        token_balance(&svm, &fixture.host_token_account),
        fixture.amount.saturating_mul(10) - fixture.amount
    );

    // Escrow is a self-authority PDA over the game's mint, at the canonical bump.
    let escrow_account = read_token_account(&svm, &fixture.escrow);
    assert_eq!(escrow_account.owner, fixture.escrow);
    assert_eq!(escrow_account.mint, fixture.mint);
    let (canonical_escrow, canonical_bump) = solana_sdk::pubkey::Pubkey::find_program_address(
        &[
            coinflip::constants::ESCROW_SEED,
            fixture.game.pubkey().as_ref(),
        ],
        &coinflip::ID,
    );
    assert_eq!(fixture.escrow, canonical_escrow);

    let game = read_game(&svm, &fixture.game.pubkey());
    assert_eq!(game.version, Game::LAYOUT_VERSION);
    assert_eq!(game.state, u8::from(GameState::Open));
    assert_eq!(game.escrow_bump, canonical_bump);
    assert_eq!(game.fee_bps, DEFAULT_FEE_BPS);
    assert_eq!(game.host, fixture.host.pubkey());
    assert_eq!(game.host_token_account, fixture.host_token_account);
    assert_eq!(game.token_mint, fixture.mint);
    assert_eq!(game.amount, fixture.amount);
    assert_eq!(game.joiner, solana_sdk::pubkey::Pubkey::default());

    // The create_game ix is a pure function of these fields, so rebuilding it
    // reproduces the exact instruction that was sent — enough to re-derive
    // the account-key ordering `find_cpi_event` needs to resolve inner-ix
    // program ids.
    let ix = ix_create_game(
        fixture.host.pubkey(),
        fixture.game.pubkey(),
        fixture.mint,
        fixture.host_token_account,
        0,
        fixture.amount,
    );
    let event = find_cpi_event::<coinflip::events::GameCreated>(
        &[ix],
        &fixture.host.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameCreated event not found in inner instructions");
    assert_eq!(event.game, fixture.game.pubkey());
    assert_eq!(event.host, fixture.host.pubkey());
    assert_eq!(event.mint, fixture.mint);
    assert_eq!(event.amount, fixture.amount);
    assert_eq!(event.host_side, u8::from(Side::Heads));
    assert_eq!(event.fee_bps, DEFAULT_FEE_BPS);
    // The bond is in the event too, so an indexer never has to have seen the
    // (now closed) game account to know what was promised to a losing joiner.
    assert_eq!(event.bond_lamports, expected_bond(&svm));
    assert_eq!(game.bond_lamports, event.bond_lamports);
}

/// The host funds everything the game needs before anyone can join: the game
/// account's own rent, the escrow's, the treasury ATA for this mint, and the
/// reimbursement bond that sits in the game account above its rent.
#[test]
fn create_posts_the_bond_and_creates_the_treasury_ata() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let stake = 1_000;
    // Fixed keys, because this test pins a compute number: the escrow PDA and
    // the treasury ATA are both `find_program_address` searches whose cost
    // depends on how many bumps they miss, and that depends on the game key and
    // the mint. Random keys made the same instruction measure anywhere from
    // ~73.5k to ~100.5k CU; pinned keys make it one number.
    let mint = create_mint_at(&mut svm, fixed_pubkey(0xC1), 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), stake * 10);
    let game = fixed_keypair(0xC2);

    let host_before = svm.get_account(&host.pubkey()).unwrap().lamports;
    let ix = ix_create_game(host.pubkey(), game.pubkey(), mint, host_ta, 0, stake);
    let meta = send_ok(&mut svm, &[&host, &game], std::slice::from_ref(&ix));

    // The bond is `2 * request_fee + rent(fulfilled request)` at ORAO's fee as
    // of create, recorded on the game and actually sitting in the account.
    let bond = expected_bond(&svm);
    let game_account = svm.get_account(&game.pubkey()).unwrap();
    let game_rent = svm.minimum_balance_for_rent_exemption(game_account.data.len());
    assert_eq!(read_game(&svm, &game.pubkey()).bond_lamports, bond);
    assert_eq!(
        read_game(&svm, &game.pubkey()).joiner_sunk_lamports,
        0,
        "nothing is sunk until someone joins"
    );
    assert_eq!(
        game_account.lamports,
        game_rent + bond,
        "the bond sits in the game account, above its rent-exempt minimum"
    );

    // The treasury ATA is the host's cost too (they picked the mint), so no
    // joiner and no cranker ever pays for it.
    let treasury_ata = get_associated_token_address(&treasury(), &mint);
    let treasury_ata_rent = svm
        .get_account(&treasury_ata)
        .expect("treasury ATA must exist after create")
        .lamports;
    assert_eq!(token_balance(&svm, &treasury_ata), 0);

    // Everything the host spends in lamports: two rents, the ATA's rent, the
    // bond, and the two-signature tx fee (host + the game keypair).
    let escrow_rent = svm
        .get_account(&escrow_pda(&game.pubkey()))
        .unwrap()
        .lamports;
    let tx_fee = 2 * 5_000;
    assert_eq!(
        host_before - svm.get_account(&host.pubkey()).unwrap().lamports,
        game_rent + bond + escrow_rent + treasury_ata_rent + tx_fee
    );

    // Budget guard: token transfer + escrow init + treasury-ATA init + the bond
    // transfer + the event CPI. Deterministic thanks to the pinned keys above
    // (measured 70_621 every run), so this is measured + ~10%, not a wide
    // margin hiding a regression.
    assert!(
        meta.compute_units_consumed < 78_000,
        "create used {} CU",
        meta.compute_units_consumed
    );
}

/// A second game on the same mint finds the treasury ATA already there:
/// `init_if_needed` makes it a no-op, and that host pays only the bond.
#[test]
fn second_game_on_the_same_mint_pays_no_ata_rent() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let treasury_ata = get_associated_token_address(&treasury(), &f.mint);
    let ata_lamports_before = svm.get_account(&treasury_ata).unwrap().lamports;

    let host_b = Keypair::new();
    svm.airdrop(&host_b.pubkey(), 10_000_000_000).unwrap();
    let host_b_ta = create_token_account(&mut svm, f.mint, host_b.pubkey(), 10_000);
    let game_b = Keypair::new();
    let host_before = svm.get_account(&host_b.pubkey()).unwrap().lamports;
    send_ok(
        &mut svm,
        &[&host_b, &game_b],
        &[ix_create_game(
            host_b.pubkey(),
            game_b.pubkey(),
            f.mint,
            host_b_ta,
            0,
            1_000,
        )],
    );

    assert_eq!(
        svm.get_account(&treasury_ata).unwrap().lamports,
        ata_lamports_before,
        "the existing treasury ATA must not be re-funded"
    );
    let game_b_account = svm.get_account(&game_b.pubkey()).unwrap();
    let escrow_rent = svm
        .get_account(&escrow_pda(&game_b.pubkey()))
        .unwrap()
        .lamports;
    let tx_fee = 2 * 5_000;
    assert_eq!(
        host_before - svm.get_account(&host_b.pubkey()).unwrap().lamports,
        game_b_account.lamports + escrow_rent + tx_fee,
        "the second host pays rents + bond, and nothing for the ATA"
    );
}

/// The bond is sized from ORAO's live fee, which ORAO's authority can raise at
/// any time — so the host states the largest lockup they accept and the create
/// fails closed above it, exactly like the joiner's `max_vrf_fee`.
#[test]
fn create_rejects_bond_above_the_hosts_maximum() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let stake = 1_000;
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), stake * 10);
    let bond = expected_bond(&svm);

    // One lamport under what the bond actually costs: rejected, nothing created.
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game_full(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            anchor_spl::token::ID,
            0,
            stake,
            bond - 1,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::BondTooHigh);
    assert!(
        svm.get_account(&game.pubkey()).is_none(),
        "a rejected create must not leave a game account behind"
    );
    assert_eq!(
        token_balance(&svm, &host_ta),
        stake * 10,
        "no stake escrowed"
    );

    // Exactly at the ceiling it goes through.
    let game = Keypair::new();
    send_ok(
        &mut svm,
        &[&host, &game],
        &[ix_create_game_full(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            anchor_spl::token::ID,
            0,
            stake,
            bond,
        )],
    );
    assert_eq!(read_game(&svm, &game.pubkey()).bond_lamports, bond);
}

/// The fee destination must be the treasury's canonical ATA — the address
/// `settle` will pin — not any account the treasury happens to own.
#[test]
fn create_with_non_ata_treasury_account_fails() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 10_000);
    // Treasury-owned and the right mint, but not at the ATA address.
    let fake_treasury_ta = create_token_account(&mut svm, mint, treasury(), 0);

    let game = Keypair::new();
    let mut ix = ix_create_game(host.pubkey(), game.pubkey(), mint, host_ta, 0, 1_000);
    let ata_slot = ix
        .accounts
        .iter()
        .position(|meta| meta.pubkey == get_associated_token_address(&treasury(), &mint))
        .expect("treasury ATA slot");
    ix.accounts[ata_slot].pubkey = fake_treasury_ta;

    // Anchor's associated-token address constraint (error code 3014).
    assert_anchor_error(
        send(&mut svm, &[&host, &game], &[ix]),
        anchor_lang::error::ErrorCode::AccountNotAssociatedTokenAccount,
    );
}

#[test]
fn create_game_rejects_zero_amount() {
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
fn create_game_rejects_denied_mint_extensions() {
    for extension in [
        ExtensionType::TransferFeeConfig,
        ExtensionType::TransferHook,
        ExtensionType::PermanentDelegate,
    ] {
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
        let host = Keypair::new();
        svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
        let mint = create_t22_mint_with_extensions(&mut svm, 9, &[extension]);
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
}

#[test]
fn create_game_rejects_mint_mismatch() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let game_mint = create_mint(&mut svm, 9);
    let other_mint = create_mint(&mut svm, 9);
    // Owned by the host (so the ownership constraint passes) but minted for
    // a different mint than the one passed to `ix_create_game`.
    let host_ta = create_token_account(&mut svm, other_mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(
        &mut svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            game_mint,
            host_ta,
            0,
            10,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}

#[test]
fn create_game_rejects_non_owned_token_account() {
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

#[test]
fn create_game_rejects_stake_above_cap() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    // Funding stays small: the cap guard fires before any transfer is attempted.
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 10);
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
            u64::MAX / 2 + 1,
        )],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::NumericalOverflow);
}

#[test]
fn cancel_refunds_host_and_closes_accounts() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 5_000_000_000);

    let host_lamports_before = svm.get_account(&f.host.pubkey()).unwrap().lamports;
    let escrow_rent = svm.get_account(&f.escrow).unwrap().lamports;
    let game_account = svm.get_account(&f.game.pubkey()).unwrap();
    let game_rent = svm.minimum_balance_for_rent_exemption(game_account.data.len());
    let bond = expected_bond(&svm);
    assert_eq!(game_account.lamports, game_rent + bond);

    let ix = ix_cancel_game(&f);
    let meta = send_ok(&mut svm, &[&f.host], std::slice::from_ref(&ix));

    assert_eq!(
        token_balance(&svm, &f.host_token_account),
        f.amount.saturating_mul(10)
    );
    assert!(svm.get_account(&f.escrow).is_none_or(|a| a.lamports == 0));
    assert!(svm
        .get_account(&f.game.pubkey())
        .is_none_or(|a| a.lamports == 0));

    // Escrow + game rent + the whole unspent bond land in the host's wallet,
    // minus the one-signer tx fee: nobody joined, so nothing was owed.
    let host_lamports_after = svm.get_account(&f.host.pubkey()).unwrap().lamports;
    let tx_fee = 5_000;
    assert_eq!(
        host_lamports_after - host_lamports_before,
        escrow_rent + game_rent + bond - tx_fee
    );

    let ev = find_cpi_event::<coinflip::events::GameCancelled>(
        &[ix],
        &f.host.pubkey(),
        &coinflip::ID,
        &meta,
    )
    .expect("GameCancelled not emitted");
    assert_eq!(ev.game, f.game.pubkey());
    assert_eq!(ev.host, f.host.pubkey());
    assert_eq!(ev.mint, f.mint);
    assert_eq!(ev.amount, f.amount);
}

#[test]
fn cancel_by_non_host_fails() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let mallory = Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let mut ix = ix_cancel_game(&f);
    ix.accounts[0].pubkey = mallory.pubkey(); // host slot
    let result = send(&mut svm, &[&mallory], &[ix]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn cancel_pays_any_host_owned_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 5_000_000_000);
    // A second host-owned account for the same mint, distinct from the one
    // recorded on the game at create time.
    let second_host_ta = create_token_account(&mut svm, f.mint, f.host.pubkey(), 0);
    send_ok(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, second_host_ta)],
    );
    assert_eq!(token_balance(&svm, &second_host_ta), f.amount);
    // The recorded account is untouched: the refund went to the account we
    // actually passed in, not the one stored on the game.
    assert_eq!(
        token_balance(&svm, &f.host_token_account),
        f.amount.saturating_mul(9)
    );
    assert!(svm.get_account(&f.escrow).is_none_or(|a| a.lamports == 0));
    assert!(svm
        .get_account(&f.game.pubkey())
        .is_none_or(|a| a.lamports == 0));
}

#[test]
fn cancel_rejects_non_host_owned_refund_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    let mallory = solana_sdk::pubkey::Pubkey::new_unique();
    let mallory_ta = create_token_account(&mut svm, f.mint, mallory, 0);
    let result = send(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, mallory_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn cancel_after_join_fails() {
    let (mut svm, payer) = setup();
    let (joined, _meta) = setup_joined_game(&mut svm, &payer, 1_000);
    // A joined game holds the joiner's stake too: cancelling here would let
    // the host walk off with it.
    let result = send(
        &mut svm,
        &[&joined.fixture.host],
        &[ix_cancel_game(&joined.fixture)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidGameState);
    assert_eq!(token_balance(&svm, &joined.fixture.escrow), 2_000);
}

/// The mint allow-list's positive side: a Token-2022 mint carrying only an
/// allowed extension flows create -> cancel end to end.
#[test]
fn t22_allowed_extension_mint_creates_and_cancels() {
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
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let token_program = anchor_spl::token_2022::ID;
    let mint = create_t22_mint_with_extensions(&mut svm, 9, &[ExtensionType::MetadataPointer]);
    let host_ta =
        create_token_account_for_program(&mut svm, token_program, mint, host.pubkey(), 1_000);

    let game = Keypair::new();
    let amount = 100;
    send_ok(
        &mut svm,
        &[&host, &game],
        &[ix_create_game_with_program(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_ta,
            token_program,
            0,
            amount,
        )],
    );
    let escrow = escrow_pda(&game.pubkey());
    assert_eq!(token_balance(&svm, &escrow), amount);
    assert_eq!(token_balance(&svm, &host_ta), 900);

    let f = GameFixture {
        host,
        game,
        mint,
        host_token_account: host_ta,
        escrow,
        amount,
    };
    send_ok(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_program(&f, token_program)],
    );
    assert_eq!(token_balance(&svm, &f.host_token_account), 1_000);
    assert!(svm.get_account(&f.escrow).is_none_or(|a| a.lamports == 0));
    assert!(svm
        .get_account(&f.game.pubkey())
        .is_none_or(|a| a.lamports == 0));
}

#[test]
fn cancel_rejects_wrong_mint_refund_account() {
    let (mut svm, payer) = setup();
    let (f, _meta) = setup_open_game(&mut svm, &payer, 1_000);
    // Host-owned, but for a different mint than the game's.
    let other_mint = create_mint(&mut svm, 9);
    let wrong_mint_ta = create_token_account(&mut svm, other_mint, f.host.pubkey(), 0);
    let result = send(
        &mut svm,
        &[&f.host],
        &[ix_cancel_game_with_refund_account(&f, wrong_mint_ta)],
    );
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}
