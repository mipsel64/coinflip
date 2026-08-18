#![allow(dead_code)]

use anchor_lang::{
    AccountDeserialize, AnchorSerialize, Discriminator, InstructionData, ToAccountMetas,
};
// Not yet called from this module; later tasks' helpers (e.g. treasury/joiner ATAs) use it.
#[allow(unused_imports)]
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use coinflip::constants::{CONFIG_SEED, ESCROW_SEED};
use coinflip::errors::CoinflipError;
use litesvm::{
    types::{FailedTransactionMetadata, TransactionMetadata},
    LiteSVM,
};
use orao_solana_vrf_cb::state::{
    client::Client,
    network_state::{NetworkConfiguration, NetworkState},
    request::{Fulfilled, RequestAccount, RequestState},
};
use solana_sdk::{
    account::Account as SolanaAccount,
    instruction::{Instruction, InstructionError},
    native_token::LAMPORTS_PER_SOL,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
// `solana_sdk::system_program` is soft-deprecated in favor of `solana_system_interface::program`;
// keep using the re-export to avoid adding a new direct dependency for a single constant.
#[allow(deprecated)]
use solana_sdk::system_program;

pub const REQUEST_FEE: u64 = 1_000_000; // what our crafted NetworkState charges
pub const DEFAULT_FEE_BPS: u16 = 100;
pub const DEFAULT_TIMEOUT_SLOTS: u64 = 1_000;

pub fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &coinflip::ID).0
}

pub fn escrow_pda(game: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ESCROW_SEED, game.as_ref()], &coinflip::ID).0
}

pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &coinflip::ID).0
}

/// Path to the built coinflip.so, honoring `CARGO_TARGET_DIR` if the caller
/// set one (otherwise the workspace's default `target/deploy`).
fn coinflip_so_path() -> String {
    match std::env::var("CARGO_TARGET_DIR") {
        Ok(target_dir) => format!("{target_dir}/deploy/coinflip.so"),
        Err(_) => format!(
            "{}/../../target/deploy/coinflip.so",
            env!("CARGO_MANIFEST_DIR")
        ),
    }
}

pub fn setup() -> (LiteSVM, Keypair) {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(coinflip::ID, coinflip_so_path())
        .expect("run `anchor build` first");
    svm.add_program_from_file(
        orao_solana_vrf_cb::ID,
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/orao_vrf_cb.so"),
    )
    .expect("missing tests/fixtures/orao_vrf_cb.so");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 1_000 * LAMPORTS_PER_SOL)
        .unwrap();
    (svm, payer)
}

// `FailedTransactionMetadata` is large (carries full tx logs); boxing it would
// ripple into every test call site for no behavioral benefit here.
#[allow(clippy::result_large_err)]
pub fn send(
    svm: &mut LiteSVM,
    signers: &[&Keypair],
    ixs: &[Instruction],
) -> Result<(), FailedTransactionMetadata> {
    // LiteSVM never advances its blockhash on its own: without this, two sends
    // of an identical instruction set produce an identical signature and get
    // rejected as `AlreadyProcessed` before the program even runs, which would
    // make "do X twice, expect the second to fail" tests pass vacuously.
    svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&signers[0].pubkey()),
        signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).map(|_| ())
}

/// Like `send`, but for the positive path: panics with pretty-printed logs
/// (instead of a bare `Result::unwrap` panic) if the transaction fails, so a
/// broken "should succeed" test points straight at the on-chain error.
pub fn send_ok(
    svm: &mut LiteSVM,
    signers: &[&Keypair],
    ixs: &[Instruction],
) -> TransactionMetadata {
    svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&signers[0].pubkey()),
        signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx)
        .unwrap_or_else(|failure| panic!("transaction failed:\n{}", failure.meta.pretty_logs()))
}

pub fn assert_coinflip_error(
    result: Result<(), FailedTransactionMetadata>,
    expected: CoinflipError,
) {
    let failure = result.unwrap_err();
    let code = match &failure.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => *code,
        other => panic!(
            "expected custom error, got {other:?}; logs:\n{}",
            failure.meta.pretty_logs()
        ),
    };
    assert_eq!(
        code,
        u32::from(expected),
        "wrong custom error; logs:\n{}",
        failure.meta.pretty_logs()
    );
    // Coinflip's and ORAO's custom error codes fully overlap (both live in
    // 6000-6013), so a matching code alone doesn't prove *our* program raised
    // it. When an error propagates out of a CPI, every program on the stack
    // logs "Program <id> failed" as it unwinds — innermost first — so the
    // FIRST such line names the true origin, and it must be coinflip.
    let origin = failure
        .meta
        .logs
        .iter()
        .find(|log| log.starts_with("Program ") && log.contains(" failed"))
        .unwrap_or_else(|| {
            panic!(
                "no 'Program <id> failed' log line found; logs:\n{}",
                failure.meta.pretty_logs()
            )
        });
    let expected_origin = format!("Program {} failed", coinflip::ID);
    assert!(
        origin.starts_with(&expected_origin),
        "error did not originate in coinflip (first failure: {origin:?}); logs:\n{}",
        failure.meta.pretty_logs()
    );
}

/// Serialize an Anchor account (discriminator + borsh) into the SVM.
///
/// `alloc_len` is the total account data length to allocate (discriminator +
/// fields + optional padding); `None` allocates exactly the serialized
/// length. Trailing padding is harmless: Anchor's borsh-based
/// `try_deserialize` reads fields off a cursor and never requires the buffer
/// to be fully consumed.
pub fn write_anchor_account<T: AnchorSerialize + Discriminator>(
    svm: &mut LiteSVM,
    address: Pubkey,
    owner: Pubkey,
    value: &T,
    extra_lamports: u64,
    alloc_len: Option<usize>,
) {
    let mut data = T::DISCRIMINATOR.to_vec();
    value.serialize(&mut data).unwrap();
    if let Some(len) = alloc_len {
        assert!(
            len >= data.len(),
            "alloc_len {len} smaller than serialized data ({})",
            data.len()
        );
        data.resize(len, 0);
    }
    let lamports = svm.minimum_balance_for_rent_exemption(data.len()) + extra_lamports;
    svm.set_account(
        address,
        SolanaAccount {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

pub fn create_mint(svm: &mut LiteSVM, decimals: u8) -> Pubkey {
    let mint = Pubkey::new_unique();
    let mut data = vec![0u8; spl_token::state::Mint::LEN];
    spl_token::state::Mint {
        mint_authority: COption::None,
        supply: 1_000_000_000_000,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut data);
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        mint,
        SolanaAccount {
            lamports,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    mint
}

pub fn create_token_account(svm: &mut LiteSVM, mint: Pubkey, owner: Pubkey, amount: u64) -> Pubkey {
    let address = Pubkey::new_unique();
    write_token_account_at(svm, address, mint, owner, amount);
    address
}

pub fn write_token_account_at(
    svm: &mut LiteSVM,
    address: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
) {
    let mut data = vec![0u8; spl_token::state::Account::LEN];
    spl_token::state::Account {
        mint,
        owner,
        amount,
        delegate: COption::None,
        state: spl_token::state::AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    }
    .pack_into_slice(&mut data);
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        address,
        SolanaAccount {
            lamports,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

pub fn token_balance(svm: &LiteSVM, address: &Pubkey) -> u64 {
    let account = svm.get_account(address).expect("token account missing");
    spl_token::state::Account::unpack(&account.data)
        .unwrap()
        .amount
}

pub struct OraoEnv {
    pub network_state: Pubkey,
    pub client: Pubkey,
    pub orao_treasury: Pubkey,
}

/// Hand-crafts the ORAO NetworkState + Client accounts (registration is an
/// off-chain deployment step; tests fabricate its result).
pub fn setup_orao(svm: &mut LiteSVM) -> OraoEnv {
    let orao_treasury = Pubkey::new_unique();
    svm.airdrop(&orao_treasury, LAMPORTS_PER_SOL).unwrap();

    let (ns_addr, ns_bump) = NetworkState::find_address(&orao_solana_vrf_cb::ID);
    let mut network_state = NetworkState::new(
        ns_bump,
        NetworkConfiguration::new(Pubkey::new_unique(), orao_treasury, REQUEST_FEE),
    );
    // Mainnet's NetworkState always has at least one fulfill authority; match
    // that account shape instead of the degenerate empty-vec case.
    network_state.config.fulfill_authorities = vec![Pubkey::new_unique()];
    write_anchor_account(
        svm,
        ns_addr,
        orao_solana_vrf_cb::ID,
        &network_state,
        0,
        Some(8 + network_state.size()),
    );

    let (client_addr, client_bump) =
        Client::find_address(&coinflip::ID, &config_pda(), &orao_solana_vrf_cb::ID);
    let client = Client::new(
        client_bump,
        Pubkey::new_unique(), // owner (irrelevant for tests)
        coinflip::ID,
        config_pda(),
        0,
        None,
    );
    // 10 SOL of client balance to pay request fees + rent. `Client::STATIC_SIZE`
    // is sized as if a callback were present; ours is `None`, so this over-
    // allocates slightly to match a real, callback-capable client's account size.
    write_anchor_account(
        svm,
        client_addr,
        orao_solana_vrf_cb::ID,
        &client,
        10 * LAMPORTS_PER_SOL,
        Some(8 + Client::STATIC_SIZE),
    );

    OraoEnv {
        network_state: ns_addr,
        client: client_addr,
        orao_treasury,
    }
}

pub fn request_pda(client: &Pubkey, game: &Pubkey) -> Pubkey {
    RequestAccount::find_address(client, &game.to_bytes(), &orao_solana_vrf_cb::ID).0
}

/// Overwrite a request account with a fulfilled state carrying `randomness`.
///
/// Models the post-callback frozen shape (`responses: None`); a request that
/// was fulfilled but whose callback hasn't run yet would carry
/// `Some(responses)` instead — harmless here because only `randomness` is
/// ever read back out of a fulfilled request in these tests.
///
/// Panics if `client`+`game` doesn't already have a (real, pending) request
/// account in the SVM: this helper is meant to settle a request that a real
/// `request` CPI created, not to conjure one out of nothing. For a standalone
/// write with no pre-existing request, use `write_fulfilled_request_unchecked`.
pub fn write_fulfilled_request(
    svm: &mut LiteSVM,
    client: Pubkey,
    game: Pubkey,
    randomness: [u8; 64],
) -> Pubkey {
    let (addr, _) =
        RequestAccount::find_address(&client, &game.to_bytes(), &orao_solana_vrf_cb::ID);
    assert!(
        svm.get_account(&addr).is_some(),
        "request account {addr} does not exist yet; join the game (or otherwise \
         trigger a real `request` CPI) before fulfilling it, or use \
         write_fulfilled_request_unchecked for a standalone write"
    );
    write_fulfilled_request_unchecked(svm, client, game, randomness)
}

/// Like `write_fulfilled_request`, but doesn't require a pre-existing request
/// account. Intended for tests that only exercise the crafted ORAO account
/// shapes in isolation (see the `orao_accounts_round_trip` smoke test).
pub fn write_fulfilled_request_unchecked(
    svm: &mut LiteSVM,
    client: Pubkey,
    game: Pubkey,
    randomness: [u8; 64],
) -> Pubkey {
    let (addr, bump) =
        RequestAccount::find_address(&client, &game.to_bytes(), &orao_solana_vrf_cb::ID);
    let request = RequestAccount::new(
        bump,
        0,
        client,
        game.to_bytes(),
        RequestState::Fulfilled(Fulfilled::new(randomness, None)),
    );
    write_anchor_account(svm, addr, orao_solana_vrf_cb::ID, &request, 0, None);
    addr
}

/// Deserializes a (real, pending) request account and returns the pubkeys of
/// its callback's remaining accounts, in order. Task 10 uses this to pin
/// callback account ordering against the real ORAO callback CPI.
pub fn request_callback_accounts(svm: &LiteSVM, request: &Pubkey) -> Vec<Pubkey> {
    let acct = svm.get_account(request).expect("request account missing");
    let request_account = RequestAccount::try_deserialize(&mut &acct.data[..])
        .expect("failed to deserialize RequestAccount");
    let pending = request_account
        .pending()
        .expect("request is not pending (already fulfilled?)");
    let callback = pending
        .callback
        .as_ref()
        .expect("request has no callback configured");
    callback
        .remaining_accounts()
        .iter()
        .map(|ra| *ra.pubkey())
        .collect()
}

// ---------- instruction builders ----------

pub fn ix_initialize_config(
    payer: Pubkey,
    admin: Pubkey,
    treasury: Pubkey,
    fee_bps: u16,
    refund_timeout_slots: u64,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::InitializeConfig {
            payer,
            config: config_pda(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::InitializeConfig {
            admin,
            treasury,
            fee_bps,
            refund_timeout_slots,
        }
        .data(),
    }
}

pub fn ix_create_game(
    host: Pubkey,
    game: Pubkey,
    mint: Pubkey,
    host_token_account: Pubkey,
    side: u8,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::CreateGame {
            host,
            config: config_pda(),
            game,
            mint,
            escrow: escrow_pda(&game),
            host_token_account,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::CreateGame { side, amount }.data(),
    }
}

/// Everything a test needs for one game.
pub struct GameFixture {
    pub host: Keypair,
    pub game: Keypair,
    pub mint: Pubkey,
    pub host_token_account: Pubkey,
    pub escrow: Pubkey,
    pub treasury: Pubkey,
    pub amount: u64,
}

/// initialize_config + mint + funded host + create_game (host_side = Heads).
pub fn setup_open_game(svm: &mut LiteSVM, payer: &Keypair, amount: u64) -> GameFixture {
    let treasury = Pubkey::new_unique();
    send_ok(
        svm,
        &[payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            treasury,
            DEFAULT_FEE_BPS,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );

    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10 * LAMPORTS_PER_SOL).unwrap();
    let mint = create_mint(svm, 9);
    let host_token_account = create_token_account(svm, mint, host.pubkey(), amount * 10);

    let game = Keypair::new();
    send_ok(
        svm,
        &[&host, &game],
        &[ix_create_game(
            host.pubkey(),
            game.pubkey(),
            mint,
            host_token_account,
            0,
            amount,
        )],
    );

    let escrow = escrow_pda(&game.pubkey());
    GameFixture {
        host,
        game,
        mint,
        host_token_account,
        escrow,
        treasury,
        amount,
    }
}

pub fn ix_update_config(
    admin: Pubkey,
    new_admin: Option<Pubkey>,
    new_treasury: Option<Pubkey>,
    new_fee_bps: Option<u16>,
    new_refund_timeout_slots: Option<u64>,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::UpdateConfig {
            admin,
            config: config_pda(),
        }
        .to_account_metas(None),
        data: coinflip::instruction::UpdateConfig {
            new_admin,
            new_treasury,
            new_fee_bps,
            new_refund_timeout_slots,
        }
        .data(),
    }
}
