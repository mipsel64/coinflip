#![allow(dead_code)]

use anchor_lang::{AnchorSerialize, Discriminator, InstructionData, ToAccountMetas};
// Not yet called from this module; later tasks' helpers (e.g. treasury/joiner ATAs) use it.
#[allow(unused_imports)]
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::spl_token;
use coinflip::errors::CoinflipError;
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
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
    Pubkey::find_program_address(&[b"config"], &coinflip::ID).0
}

pub fn escrow_pda(game: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"escrow", game.as_ref()], &coinflip::ID).0
}

pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &coinflip::ID).0
}

pub fn setup() -> (LiteSVM, Keypair) {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(coinflip::ID, "../../target/deploy/coinflip.so")
        .expect("run `anchor build` first");
    svm.add_program_from_file(orao_solana_vrf_cb::ID, "tests/fixtures/orao_vrf_cb.so")
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
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&signers[0].pubkey()),
        signers,
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).map(|_| ())
}

pub fn assert_coinflip_error(
    result: Result<(), FailedTransactionMetadata>,
    expected: CoinflipError,
) {
    match result.unwrap_err().err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(code, 6000 + expected as u32, "wrong custom error");
        }
        other => panic!("expected custom error, got {other:?}"),
    }
}

/// Serialize an Anchor account (discriminator + borsh) into the SVM.
pub fn write_anchor_account<T: AnchorSerialize + Discriminator>(
    svm: &mut LiteSVM,
    address: Pubkey,
    owner: Pubkey,
    value: &T,
    extra_lamports: u64,
) {
    let mut data = T::DISCRIMINATOR.to_vec();
    value.serialize(&mut data).unwrap();
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
    let network_state = NetworkState::new(
        ns_bump,
        NetworkConfiguration::new(Pubkey::new_unique(), orao_treasury, REQUEST_FEE),
    );
    write_anchor_account(svm, ns_addr, orao_solana_vrf_cb::ID, &network_state, 0);

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
    // 10 SOL of client balance to pay request fees + rent.
    write_anchor_account(
        svm,
        client_addr,
        orao_solana_vrf_cb::ID,
        &client,
        10 * LAMPORTS_PER_SOL,
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
pub fn write_fulfilled_request(
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
    write_anchor_account(svm, addr, orao_solana_vrf_cb::ID, &request, 0);
    addr
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
