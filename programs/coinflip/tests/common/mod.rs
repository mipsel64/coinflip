#![allow(dead_code)]

use anchor_lang::{
    solana_program::bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    AccountDeserialize, AnchorDeserialize, AnchorSerialize, Discriminator, InstructionData,
    ToAccountMetas,
};
use anchor_spl::associated_token::{
    get_associated_token_address, get_associated_token_address_with_program_id,
};
use anchor_spl::token::spl_token;
use anchor_spl::token_2022::spl_token_2022;
use coinflip::constants::{CONFIG_SEED, ESCROW_SEED};
use coinflip::errors::CoinflipError;
use coinflip::state::Game;
use litesvm::{
    types::{FailedTransactionMetadata, TransactionMetadata},
    LiteSVM,
};
use orao_solana_vrf::{
    network_state_account_address, randomness_account_address,
    state::{FulfilledRequest, NetworkConfiguration, NetworkState, RandomnessV2, RequestAccount},
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
pub use spl_token_2022::extension::ExtensionType;
// `solana_sdk::system_program` is soft-deprecated in favor of `solana_system_interface::program`;
// keep using the re-export to avoid adding a new direct dependency for a single constant.
#[allow(deprecated)]
use solana_sdk::system_program;

pub const REQUEST_FEE: u64 = 1_000_000; // what our crafted NetworkState charges
/// Starting balance of the crafted ORAO treasury — tests assert request-fee
/// deltas against it.
pub const ORAO_TREASURY_START_LAMPORTS: u64 = LAMPORTS_PER_SOL;
pub const DEFAULT_FEE_BPS: u16 = 100;
/// The floor (`MIN_REFUND_TIMEOUT_SLOTS`); refund tests warp past it anyway.
pub const DEFAULT_TIMEOUT_SLOTS: u64 = 1_500;
/// What ORAO's `RequestV2` allocates for a pending request account, and thus
/// the rent the joiner pays: `8 + RandomnessV2::PENDING_SIZE`.
pub const PENDING_REQUEST_LEN: usize = 8 + RandomnessV2::PENDING_SIZE;
/// What ORAO shrinks that account to at fulfillment (`8 +
/// RandomnessV2::FULFILLED_SIZE`, 137 bytes): its rent is the part the joiner
/// never gets back, so it is what `create_game` bonds and `join_game` records.
pub const FULFILLED_REQUEST_LEN: usize = 8 + RandomnessV2::FULFILLED_SIZE;

/// Rent for a fulfilled-size request account — the program's own
/// `fulfilled_request_rent()`, computed against this SVM's rent parameters.
pub fn fulfilled_request_rent(svm: &LiteSVM) -> u64 {
    svm.minimum_balance_for_rent_exemption(FULFILLED_REQUEST_LEN)
}

/// What `create_game` bonds into the game account at the crafted `REQUEST_FEE`:
/// `2 * request_fee + rent(fulfilled request)`.
pub fn expected_bond(svm: &LiteSVM) -> u64 {
    REQUEST_FEE * 2 + fulfilled_request_rent(svm)
}

/// What `join_game` records as the joiner's unrecoverable outlay at a given
/// ORAO fee: `request_fee + rent(fulfilled request)`.
pub fn expected_joiner_sunk(svm: &LiteSVM, request_fee: u64) -> u64 {
    request_fee + fulfilled_request_rent(svm)
}

pub fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &coinflip::ID).0
}

/// The treasury the deployed `.so` enforces: the committed `local` fixture key.
///
/// Read from the fixture rather than from `coinflip::treasury::ID` because the
/// host-side crate these tests link against is usually built WITHOUT the
/// `local` feature (plain `cargo test -p coinflip`), where that constant is the
/// real, deployable treasury instead.
pub fn treasury() -> Pubkey {
    static TREASURY: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *TREASURY.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/treasury-local.json"
        );
        solana_sdk::signature::read_keypair_file(path)
            .expect("missing tests/fixtures/treasury-local.json")
            .pubkey()
    })
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

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Newest modification time of any `.rs` file under `dir` (recursive).
fn newest_rs_mtime(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let newer = if path.is_dir() {
            newest_rs_mtime(&path)
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            mtime(&path)
        } else {
            None
        };
        newest = newest.max(newer);
    }
    newest
}

/// Newest mtime across everything that changes what the deployed program does:
/// its sources, its manifest, and the workspace's manifest + lockfile (a bumped
/// dependency changes the binary without touching a single `.rs` file).
fn newest_program_input_mtime() -> Option<std::time::SystemTime> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("../..");
    [
        newest_rs_mtime(&manifest_dir.join("src")),
        mtime(&manifest_dir.join("Cargo.toml")),
        mtime(&workspace.join("Cargo.toml")),
        mtime(&workspace.join("Cargo.lock")),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// `cargo test` never rebuilds the deployed artifact, so an e2e suite happily
/// runs green against a `.so` built before the change under test. Compare
/// mtimes and refuse to run instead.
fn assert_program_not_stale(so_path: &str) {
    let so_mtime = mtime(std::path::Path::new(so_path))
        .unwrap_or_else(|| panic!("cannot stat {so_path} — run `anchor build`"));
    let Some(input_mtime) = newest_program_input_mtime() else {
        return;
    };
    // Deliberately cruder than cargo's own fingerprinting: if a manifest's
    // timestamp moved without its contents changing, `anchor build` is a no-op
    // and only touching the artifact clears this.
    assert!(
        so_mtime >= input_mtime,
        "stale {so_path} — run `anchor build` \
         (no-op build? only a manifest timestamp moved: `touch {so_path}`)"
    );
}

/// Whether the compiled program embeds `key` as a constant.
///
/// Searches for the key's eight 4-byte words rather than the whole 32 bytes:
/// the treasury constant is only ever *compared* against, and the release
/// build turns that into immediate loads instead of a contiguous rodata blob —
/// each SBF `lddw` carries its 64-bit immediate as two 4-byte halves in
/// separate instruction words (verified against both builds: 8-byte runs never
/// appear, every 4-byte word appears once per comparison site). Eight
/// independent 4-byte hits cannot line up by chance.
fn elf_embeds_key(elf: &[u8], key: &Pubkey) -> bool {
    key.to_bytes()
        .chunks(4)
        .all(|word| elf.windows(4).any(|window| window == word))
}

/// The treasury is a compile-time constant, so the `.so` the suite loads must
/// be the `local` build — otherwise every fee destination the harness derives
/// is one the program rejects, and the whole suite fails at the constraint
/// instead of at the actual mistake.
fn assert_built_with_local_feature(elf: &[u8], so_path: &str) {
    assert!(
        elf_embeds_key(elf, &treasury()),
        "{so_path} was built without --features local; run:\n  \
         anchor build --no-idl -- --features local --tools-version v1.56\n  \
         anchor idl build -o target/idl/coinflip.json -t target/types/coinflip.ts \
         -- --features local"
    );
    // Only decidable when the host-side crate itself was built without the
    // feature — there `coinflip::treasury::ID` is the deployable id, and
    // finding it in the artifact would mean the constant never got swapped.
    #[cfg(not(feature = "local"))]
    assert!(
        !elf_embeds_key(elf, &coinflip::treasury::ID),
        "{so_path} still embeds the deployable treasury ({}) — rebuild it with \
         --features local",
        coinflip::treasury::ID
    );
}

/// Registers the built program the way a real deployment does: an upgradeable
/// loader program account pointing at a ProgramData account that names
/// `upgrade_authority`. `add_program_from_file` installs programs under the
/// non-upgradeable loader instead, where there is no ProgramData at all and
/// `initialize_config`'s deployer gate could never be satisfied.
fn add_upgradeable_program(
    svm: &mut LiteSVM,
    program_id: Pubkey,
    elf: &[u8],
    upgrade_authority: Pubkey,
) {
    // bincode layout of `UpgradeableLoaderState::ProgramData`, then the ELF —
    // exactly what the loader (and LiteSVM's program loader) expects to find.
    let mut programdata = 3u32.to_le_bytes().to_vec();
    programdata.extend_from_slice(&0u64.to_le_bytes()); // deployed slot
    programdata.push(1); // Some(upgrade_authority)
    programdata.extend_from_slice(upgrade_authority.as_ref());
    assert_eq!(
        programdata.len(),
        UpgradeableLoaderState::size_of_programdata_metadata()
    );
    programdata.extend_from_slice(elf);
    let programdata_address = program_data_address(&program_id);
    let lamports = svm.minimum_balance_for_rent_exemption(programdata.len());
    svm.set_account(
        programdata_address,
        SolanaAccount {
            lamports,
            data: programdata,
            owner: bpf_loader_upgradeable::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // `UpgradeableLoaderState::Program { programdata_address }`. Setting an
    // executable account makes LiteSVM load the ELF through this pointer, so
    // the ProgramData account must already be in place.
    let mut program = 2u32.to_le_bytes().to_vec();
    program.extend_from_slice(programdata_address.as_ref());
    assert_eq!(program.len(), UpgradeableLoaderState::size_of_program());
    let lamports = svm.minimum_balance_for_rent_exemption(program.len());
    svm.set_account(
        program_id,
        SolanaAccount {
            lamports,
            data: program,
            owner: bpf_loader_upgradeable::ID,
            executable: true,
            rent_epoch: 0,
        },
    )
    .expect("failed to register coinflip under the upgradeable loader");
}

/// The program's ProgramData account, which holds its upgrade authority.
pub fn program_data_address(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::ID).0
}

/// Boots an SVM with coinflip + ORAO loaded (program AND crafted
/// `NetworkState`, which `create_game` reads to size the host's bond) and a
/// funded payer. That payer is also coinflip's upgrade authority, so
/// `ix_initialize_config(payer, ..)` clears the deployer gate.
pub fn setup() -> (LiteSVM, Keypair) {
    let mut svm = LiteSVM::new();
    let so_path = coinflip_so_path();
    assert_program_not_stale(&so_path);
    let elf = std::fs::read(&so_path).unwrap_or_else(|_| panic!("{so_path}: run `anchor build`"));
    assert_built_with_local_feature(&elf, &so_path);
    let payer = Keypair::new();
    add_upgradeable_program(&mut svm, coinflip::ID, &elf, payer.pubkey());
    svm.add_program_from_file(
        orao_solana_vrf::ID,
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/orao_vrf.so"),
    )
    .expect("missing tests/fixtures/orao_vrf.so");
    svm.airdrop(&payer.pubkey(), 1_000 * LAMPORTS_PER_SOL)
        .unwrap();
    setup_orao(&mut svm);
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

/// Like `assert_coinflip_error`, but for an error the Anchor framework itself
/// raises (its own 2000/3000-range codes) rather than one of ours. Also proves
/// the transaction actually executed, instead of being dropped as a duplicate.
#[allow(clippy::result_large_err)]
pub fn assert_anchor_error(
    result: Result<(), FailedTransactionMetadata>,
    expected: anchor_lang::error::ErrorCode,
) {
    let failure = result.unwrap_err();
    match &failure.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(
                *code,
                u32::from(expected),
                "wrong anchor error; logs:\n{}",
                failure.meta.pretty_logs()
            );
        }
        other => panic!(
            "expected custom error, got {other:?}; logs:\n{}",
            failure.meta.pretty_logs()
        ),
    }
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
    write_token_account_at_for_program(svm, spl_token::ID, address, mint, owner, amount);
}

/// Like `create_token_account`, but the account is owned by `token_program`
/// instead of always the classic SPL Token program (e.g. Token-2022 mints
/// need their token accounts owned by the Token-2022 program).
pub fn create_token_account_for_program(
    svm: &mut LiteSVM,
    token_program: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
) -> Pubkey {
    let address = Pubkey::new_unique();
    write_token_account_at_for_program(svm, token_program, address, mint, owner, amount);
    address
}

/// Like `write_token_account_at`, but the account is owned by `token_program`.
/// A base (no account-level extensions) Token-2022 account has the exact same
/// 165-byte layout as a classic SPL Token account, so the same packing works.
pub fn write_token_account_at_for_program(
    svm: &mut LiteSVM,
    token_program: Pubkey,
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
            owner: token_program,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Token-2022 mint carrying the given extensions (zeroed/default field
/// values) — used to prove the allow-list rejects the denied ones and accepts
/// the allowed ones. Crafted with spl-token-2022's own TLV packing.
pub fn create_t22_mint_with_extensions(
    svm: &mut LiteSVM,
    decimals: u8,
    extensions: &[ExtensionType],
) -> Pubkey {
    use spl_token_2022::extension::{
        metadata_pointer::MetadataPointer, permanent_delegate::PermanentDelegate,
        transfer_fee::TransferFeeConfig, transfer_hook::TransferHook, BaseStateWithExtensionsMut,
        StateWithExtensionsMut,
    };

    let mint = Pubkey::new_unique();
    let account_len =
        ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(extensions)
            .unwrap();
    let mut data = vec![0u8; account_len];
    let mut state =
        StateWithExtensionsMut::<spl_token_2022::state::Mint>::unpack_uninitialized(&mut data)
            .unwrap();
    // Zeroed/default extension fields are fine — the allow-list only checks
    // presence of the extension, not its configured values.
    for extension in extensions {
        match extension {
            ExtensionType::TransferFeeConfig => {
                state.init_extension::<TransferFeeConfig>(true).unwrap();
            }
            ExtensionType::TransferHook => {
                state.init_extension::<TransferHook>(true).unwrap();
            }
            ExtensionType::PermanentDelegate => {
                state.init_extension::<PermanentDelegate>(true).unwrap();
            }
            // Allowed by the mint allow-list: zeroed authority/metadata_address
            // both decode as `None`.
            ExtensionType::MetadataPointer => {
                state.init_extension::<MetadataPointer>(true).unwrap();
            }
            other => panic!("create_t22_mint_with_extensions: unsupported extension {other:?}"),
        }
    }
    state.base = spl_token_2022::state::Mint {
        mint_authority: COption::None,
        supply: 1_000_000_000_000,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    };
    state.pack_base();
    state.init_account_type().unwrap();

    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        mint,
        SolanaAccount {
            lamports,
            data,
            owner: spl_token_2022::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    mint
}

/// Reads the base token-account fields (owner, mint, amount, ...) from either
/// a legacy SPL Token account or a Token-2022 account (with or without
/// extensions) — `StateWithExtensions` falls back to a plain base unpack when
/// there's no TLV data trailing the base account, so a legacy 165-byte
/// account round-trips the same way.
pub fn read_token_account(svm: &LiteSVM, address: &Pubkey) -> spl_token_2022::state::Account {
    let account = svm.get_account(address).expect("token account missing");
    spl_token_2022::extension::StateWithExtensions::<spl_token_2022::state::Account>::unpack(
        &account.data,
    )
    .unwrap()
    .base
}

pub fn token_balance(svm: &LiteSVM, address: &Pubkey) -> u64 {
    read_token_account(svm, address).amount
}

pub fn read_game(svm: &LiteSVM, address: &Pubkey) -> Game {
    let account = svm.get_account(address).expect("game account missing");
    Game::try_deserialize(&mut &account.data[..]).expect("failed to deserialize Game")
}

/// Finds the first inner instruction that is an `emit_cpi!`'d `T` event
/// originating from `program_id`, and decodes it.
///
/// `ixs`/`payer` must be exactly what was passed to the `send`/`send_ok` call
/// that produced `meta`: resolving each inner instruction's
/// `program_id_index` requires re-deriving the same account-key ordering
/// `Transaction::new_signed_with_payer` used when compiling the sent
/// transaction (it calls `Message::new(instructions, payer)` internally, so
/// calling it again here with the same inputs reproduces the same ordering).
pub fn find_cpi_event<T: AnchorDeserialize + Discriminator>(
    ixs: &[Instruction],
    payer: &Pubkey,
    program_id: &Pubkey,
    meta: &TransactionMetadata,
) -> Option<T> {
    let message = solana_sdk::message::Message::new(ixs, Some(payer));
    for inner in meta.inner_instructions.iter().flatten() {
        let compiled = &inner.instruction;
        if message.account_keys.get(compiled.program_id_index as usize) != Some(program_id) {
            continue;
        }
        let Some(rest) = compiled
            .data
            .strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE)
        else {
            continue;
        };
        let Some(fields) = rest.strip_prefix(T::DISCRIMINATOR) else {
            continue;
        };
        if let Ok(event) = T::try_from_slice(fields) {
            return Some(event);
        }
    }
    None
}

pub struct OraoEnv {
    pub network_state: Pubkey,
    pub orao_treasury: Pubkey,
}

/// Hand-crafts ORAO's `NetworkState` — the one account the VRF program needs
/// before it will accept a `RequestV2`, and the one `create_game` reads to size
/// the host's bond. Plain VRF has no client registration, so there is nothing
/// else to fabricate.
///
/// Idempotent: `setup()` installs it, so a later call just hands back the env
/// that is already in place (crafting a second one would move ORAO's treasury
/// out from under the games already created against it).
pub fn setup_orao(svm: &mut LiteSVM) -> OraoEnv {
    let existing = svm.get_account(&network_state_account_address(&orao_solana_vrf::ID));
    if let Some(account) = existing.filter(|a| !a.data.is_empty()) {
        let network_state = NetworkState::try_deserialize(&mut &account.data[..])
            .expect("crafted NetworkState must deserialize");
        return OraoEnv {
            network_state: network_state_account_address(&orao_solana_vrf::ID),
            orao_treasury: network_state.config.treasury,
        };
    }

    let orao_treasury = Pubkey::new_unique();
    svm.airdrop(&orao_treasury, ORAO_TREASURY_START_LAMPORTS)
        .unwrap();

    let network_state = NetworkState {
        config: NetworkConfiguration {
            authority: Pubkey::new_unique(),
            treasury: orao_treasury,
            request_fee: REQUEST_FEE,
            // Mainnet's NetworkState always has at least one fulfillment
            // authority; match that shape, not the degenerate empty-vec case.
            fulfillment_authorities: vec![Pubkey::new_unique()],
            // No SPL fee path: `join_game` always pays ORAO in lamports.
            token_fee_config: None,
        },
        num_received: 0,
    };
    let ns_addr = network_state_account_address(&orao_solana_vrf::ID);
    // ORAO's own `InitNetwork` allocates `8 + 464`; match it so the account the
    // program writes `num_received` back into is the size it expects.
    write_anchor_account(
        svm,
        ns_addr,
        orao_solana_vrf::ID,
        &network_state,
        0,
        Some(8 + 464),
    );

    OraoEnv {
        network_state: ns_addr,
        orao_treasury,
    }
}

/// Rewrites the crafted `NetworkState` with a new `request_fee`, the way ORAO's
/// authority can raise its fee at any time — including between a create and the
/// join it is bonded for.
pub fn set_orao_request_fee(svm: &mut LiteSVM, orao: &OraoEnv, request_fee: u64) {
    let account = svm
        .get_account(&orao.network_state)
        .expect("network state missing");
    let mut network_state = NetworkState::try_deserialize(&mut &account.data[..]).unwrap();
    network_state.config.request_fee = request_fee;
    write_anchor_account(
        svm,
        orao.network_state,
        orao_solana_vrf::ID,
        &network_state,
        0,
        Some(account.data.len()),
    );
}

/// Mirrors the program's VRF seed derivation (`join_game`): a hash of the
/// game, the joiner, and the client-chosen nonce — unpredictable until the
/// joiner commits, and movable to a fresh address if that one is taken.
pub fn vrf_seed_for(game: &Pubkey, joiner: &Pubkey, nonce: u64) -> [u8; 32] {
    solana_sdk::hash::hashv(&[
        b"coinflip-vrf-seed",
        game.as_ref(),
        joiner.as_ref(),
        &nonce.to_le_bytes(),
    ])
    .to_bytes()
}

/// The ORAO request PDA for `seed`. Plain VRF namespaces requests globally —
/// `[RANDOMNESS_ACCOUNT_SEED, seed]`, with no per-client component — so the
/// hashed `vrf_seed` is what makes ours unique and unpredictable.
pub fn request_pda(seed: &[u8; 32]) -> Pubkey {
    randomness_account_address(&orao_solana_vrf::ID, seed)
}

/// ORAO's own `request_v2`, sent directly rather than through `join_game` —
/// requests live in a global namespace, so anyone can create one for any seed.
pub fn ix_orao_request_v2(orao: &OraoEnv, payer: Pubkey, seed: [u8; 32]) -> Instruction {
    Instruction {
        program_id: orao_solana_vrf::ID,
        accounts: orao_solana_vrf::accounts::RequestV2 {
            payer,
            network_state: orao.network_state,
            treasury: orao.orao_treasury,
            request: request_pda(&seed),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: orao_solana_vrf::instruction::RequestV2 { seed }.data(),
    }
}

/// Reads a (real) request account back out of the SVM.
pub fn read_request(svm: &LiteSVM, address: &Pubkey) -> RandomnessV2 {
    let account = svm.get_account(address).expect("request account missing");
    RandomnessV2::try_deserialize(&mut &account.data[..])
        .expect("failed to deserialize RandomnessV2")
}

/// Overwrite a request account with a fulfilled state carrying `randomness`.
///
/// LiteSVM cannot produce the oracle quorum's ed25519 signatures, so this
/// stands in for a real `fulfill_v2`. The `client` is carried over from the
/// pending request the ORAO program actually wrote, rather than guessed.
///
/// Panics if `seed` doesn't already have a (real, pending) request account in
/// the SVM: this helper is meant to fulfill a request that a real `request_v2`
/// CPI created, not to conjure one out of nothing. For a standalone write with
/// no pre-existing request, use `write_fulfilled_request_unchecked`.
pub fn write_fulfilled_request(svm: &mut LiteSVM, seed: [u8; 32], randomness: [u8; 64]) -> Pubkey {
    let addr = request_pda(&seed);
    assert!(
        svm.get_account(&addr).is_some(),
        "request account {addr} does not exist yet; join the game (or otherwise \
         trigger a real `request_v2` CPI) before fulfilling it, or use \
         write_fulfilled_request_unchecked for a standalone write"
    );
    let client = *read_request(svm, &addr).client();
    write_fulfilled_request_unchecked(svm, client, seed, randomness)
}

/// Like `write_fulfilled_request`, but doesn't require a pre-existing request
/// account. Intended for tests that only exercise the crafted ORAO account
/// shapes in isolation (see the `orao_accounts_round_trip` smoke test).
pub fn write_fulfilled_request_unchecked(
    svm: &mut LiteSVM,
    client: Pubkey,
    seed: [u8; 32],
    randomness: [u8; 64],
) -> Pubkey {
    let addr = request_pda(&seed);
    let request = RandomnessV2 {
        request: RequestAccount::Fulfilled(FulfilledRequest {
            client,
            seed,
            randomness,
        }),
    };
    write_anchor_account(svm, addr, orao_solana_vrf::ID, &request, 0, None);
    addr
}

// ---------- instruction builders ----------

pub fn ix_initialize_config(
    payer: Pubkey,
    admin: Pubkey,
    fee_bps: u16,
    refund_timeout_slots: u64,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::InitializeConfig {
            payer,
            config: config_pda(),
            program: coinflip::ID,
            program_data: program_data_address(&coinflip::ID),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::InitializeConfig {
            admin,
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
    ix_create_game_with_program(
        host,
        game,
        mint,
        host_token_account,
        spl_token::ID,
        side,
        amount,
    )
}

/// Like `ix_create_game`, but lets the caller pick the token program (e.g.
/// Token-2022 mints must be created with `token_program = spl_token_2022::ID`).
pub fn ix_create_game_with_program(
    host: Pubkey,
    game: Pubkey,
    mint: Pubkey,
    host_token_account: Pubkey,
    token_program: Pubkey,
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
            treasury: treasury(),
            treasury_token_account: get_associated_token_address_with_program_id(
                &treasury(),
                &mint,
                &token_program,
            ),
            // A fixed PDA, so no OraoEnv is needed to build the instruction —
            // but the account must EXIST (setup() crafts it), because
            // create_game reads ORAO's fee to size the host's bond.
            network_state: network_state_account_address(&orao_solana_vrf::ID),
            associated_token_program: anchor_spl::associated_token::ID,
            token_program,
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
    pub amount: u64,
}

/// initialize_config + mint + funded host + create_game (host_side = Heads).
/// Also returns the create_game transaction's metadata, so callers can assert
/// on its emitted events (see `find_cpi_event`) without re-sending.
pub fn setup_open_game(
    svm: &mut LiteSVM,
    payer: &Keypair,
    amount: u64,
) -> (GameFixture, TransactionMetadata) {
    setup_open_game_with_fee(svm, payer, amount, DEFAULT_FEE_BPS)
}

/// Like `setup_open_game`, but with an explicit protocol fee (the game snapshots
/// it at create, so this is what settlement will charge).
pub fn setup_open_game_with_fee(
    svm: &mut LiteSVM,
    payer: &Keypair,
    amount: u64,
    fee_bps: u16,
) -> (GameFixture, TransactionMetadata) {
    send_ok(
        svm,
        &[payer],
        &[ix_initialize_config(
            payer.pubkey(),
            payer.pubkey(),
            fee_bps,
            DEFAULT_TIMEOUT_SLOTS,
        )],
    );

    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10 * LAMPORTS_PER_SOL).unwrap();
    let mint = create_mint(svm, 9);
    let host_token_account =
        create_token_account(svm, mint, host.pubkey(), amount.saturating_mul(10));

    let game = Keypair::new();
    let meta = send_ok(
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
    (
        GameFixture {
            host,
            game,
            mint,
            host_token_account,
            escrow,
            amount,
        },
        meta,
    )
}

pub fn ix_cancel_game(f: &GameFixture) -> Instruction {
    ix_cancel_game_with_refund_account(f, f.host_token_account)
}

/// Like `ix_cancel_game`, but lets the caller pick which host-owned token
/// account receives the refund (liveness: any host-owned account of the
/// game's mint is accepted, not just the one recorded on the game).
pub fn ix_cancel_game_with_refund_account(
    f: &GameFixture,
    host_token_account: Pubkey,
) -> Instruction {
    ix_cancel_game_full(f, host_token_account, spl_token::ID)
}

/// Like `ix_cancel_game`, but lets the caller pick the token program (a
/// Token-2022 game must be cancelled through `spl_token_2022::ID`).
pub fn ix_cancel_game_with_program(f: &GameFixture, token_program: Pubkey) -> Instruction {
    ix_cancel_game_full(f, f.host_token_account, token_program)
}

pub fn ix_cancel_game_full(
    f: &GameFixture,
    host_token_account: Pubkey,
    token_program: Pubkey,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::CancelGame {
            host: f.host.pubkey(),
            game: f.game.pubkey(),
            mint: f.mint,
            escrow: f.escrow,
            host_token_account,
            token_program,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::CancelGame {}.data(),
    }
}

pub struct JoinedGame {
    pub fixture: GameFixture,
    pub joiner: Keypair,
    pub joiner_token_account: Pubkey,
    pub treasury_token_account: Pubkey,
    pub orao: OraoEnv,
    pub request: Pubkey,
    pub vrf_seed: [u8; 32],
}

/// The common case: nonce 0 and no cap on ORAO's fee.
pub fn ix_join_game(
    f: &GameFixture,
    orao: &OraoEnv,
    joiner: Pubkey,
    joiner_token_account: Pubkey,
) -> Instruction {
    ix_join_game_with_program(f, orao, joiner, joiner_token_account, spl_token::ID)
}

/// Like `ix_join_game`, but lets the caller pick the token program (a
/// Token-2022 game must be joined through `spl_token_2022::ID`).
pub fn ix_join_game_with_program(
    f: &GameFixture,
    orao: &OraoEnv,
    joiner: Pubkey,
    joiner_token_account: Pubkey,
    token_program: Pubkey,
) -> Instruction {
    ix_join_game_full(
        f,
        orao,
        joiner,
        joiner_token_account,
        token_program,
        0,
        u64::MAX,
    )
}

/// Like `ix_join_game`, but with an explicit VRF-seed nonce — the retry knob a
/// client turns when someone else already created the request at nonce N.
pub fn ix_join_game_with_nonce(
    f: &GameFixture,
    orao: &OraoEnv,
    joiner: Pubkey,
    joiner_token_account: Pubkey,
    nonce: u64,
) -> Instruction {
    ix_join_game_full(
        f,
        orao,
        joiner,
        joiner_token_account,
        spl_token::ID,
        nonce,
        u64::MAX,
    )
}

/// The full builder: every `join_game` argument spelled out.
pub fn ix_join_game_full(
    f: &GameFixture,
    orao: &OraoEnv,
    joiner: Pubkey,
    joiner_token_account: Pubkey,
    token_program: Pubkey,
    nonce: u64,
    max_vrf_fee: u64,
) -> Instruction {
    let request = request_pda(&vrf_seed_for(&f.game.pubkey(), &joiner, nonce));
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::JoinGame {
            joiner,
            config: config_pda(),
            game: f.game.pubkey(),
            mint: f.mint,
            escrow: f.escrow,
            joiner_token_account,
            vrf: orao_solana_vrf::ID,
            network_state: orao.network_state,
            orao_treasury: orao.orao_treasury,
            request,
            token_program,
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::JoinGame { nonce, max_vrf_fee }.data(),
    }
}

/// Full flow: open game + ORAO env + join. Game ends AwaitingRandomness
/// with a REAL pending request account created by the dumped ORAO program.
///
/// Returns the join transaction's metadata alongside the fixture (same shape
/// as `setup_open_game`) so callers can assert on the emitted `GameJoined`.
pub fn setup_joined_game(
    svm: &mut LiteSVM,
    payer: &Keypair,
    amount: u64,
) -> (JoinedGame, TransactionMetadata) {
    setup_joined_game_with_fee(svm, payer, amount, DEFAULT_FEE_BPS)
}

/// Like `setup_joined_game`, but with an explicit protocol fee.
pub fn setup_joined_game_with_fee(
    svm: &mut LiteSVM,
    payer: &Keypair,
    amount: u64,
    fee_bps: u16,
) -> (JoinedGame, TransactionMetadata) {
    let (fixture, _create_meta) = setup_open_game_with_fee(svm, payer, amount, fee_bps);
    let orao = setup_orao(svm); // already installed by `setup()`; this reads it back
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10 * LAMPORTS_PER_SOL)
        .unwrap();
    let joiner_token_account = create_token_account(
        svm,
        fixture.mint,
        joiner.pubkey(),
        amount.saturating_mul(10),
    );
    let meta = send_ok(
        svm,
        &[&joiner],
        &[ix_join_game(
            &fixture,
            &orao,
            joiner.pubkey(),
            joiner_token_account,
        )],
    );
    let treasury_token_account = get_associated_token_address(&treasury(), &fixture.mint);
    let vrf_seed = vrf_seed_for(&fixture.game.pubkey(), &joiner.pubkey(), 0);
    let request = request_pda(&vrf_seed);
    (
        JoinedGame {
            fixture,
            joiner,
            joiner_token_account,
            treasury_token_account,
            orao,
            request,
            vrf_seed,
        },
        meta,
    )
}

pub fn ix_settle(j: &JoinedGame, cranker: Pubkey) -> Instruction {
    ix_settle_full(
        j,
        cranker,
        j.fixture.host_token_account,
        j.joiner_token_account,
        j.treasury_token_account,
    )
}

/// Like `ix_settle`, but lets the caller pick the payout/fee destinations
/// (liveness: any winner-owned account of the game mint is accepted, and the
/// fee must go to an account the constant treasury owns).
pub fn ix_settle_full(
    j: &JoinedGame,
    cranker: Pubkey,
    host_token_account: Pubkey,
    joiner_token_account: Pubkey,
    treasury_token_account: Pubkey,
) -> Instruction {
    ix_settle_with_request(
        j,
        cranker,
        j.request,
        host_token_account,
        joiner_token_account,
        treasury_token_account,
    )
}

/// Like `ix_settle_full`, but also lets the caller pick the request account —
/// used to prove a foreign game's request cannot settle this game.
pub fn ix_settle_with_request(
    j: &JoinedGame,
    cranker: Pubkey,
    request: Pubkey,
    host_token_account: Pubkey,
    joiner_token_account: Pubkey,
    treasury_token_account: Pubkey,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::Settle {
            cranker,
            request,
            game: j.fixture.game.pubkey(),
            escrow: j.fixture.escrow,
            host: j.fixture.host.pubkey(),
            joiner: j.joiner.pubkey(),
            host_token_account,
            joiner_token_account,
            treasury_token_account,
            mint: j.fixture.mint,
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::Settle {}.data(),
    }
}

pub fn ix_refund_timeout(j: &JoinedGame, cranker: Pubkey) -> Instruction {
    ix_refund_timeout_full(
        j,
        cranker,
        j.fixture.host_token_account,
        j.joiner_token_account,
    )
}

/// Like `ix_refund_timeout`, but lets the caller pick where each side's stake
/// goes (liveness: a recorded account that is gone by refund time must not
/// strand the funds, so the player's ATA is accepted too).
pub fn ix_refund_timeout_full(
    j: &JoinedGame,
    cranker: Pubkey,
    host_token_account: Pubkey,
    joiner_token_account: Pubkey,
) -> Instruction {
    ix_refund_timeout_with_request(
        j,
        cranker,
        j.request,
        host_token_account,
        joiner_token_account,
    )
}

/// Like `ix_refund_timeout_full`, but also lets the caller pick the request
/// account — used to prove a foreign game's request cannot refund this game.
pub fn ix_refund_timeout_with_request(
    j: &JoinedGame,
    cranker: Pubkey,
    request: Pubkey,
    host_token_account: Pubkey,
    joiner_token_account: Pubkey,
) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::RefundTimeout {
            cranker,
            request,
            game: j.fixture.game.pubkey(),
            escrow: j.fixture.escrow,
            host: j.fixture.host.pubkey(),
            host_token_account,
            joiner_token_account,
            mint: j.fixture.mint,
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::RefundTimeout {}.data(),
    }
}

pub fn ix_update_config(
    admin: Pubkey,
    new_admin: Option<Pubkey>,
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
            new_fee_bps,
            new_refund_timeout_slots,
        }
        .data(),
    }
}
