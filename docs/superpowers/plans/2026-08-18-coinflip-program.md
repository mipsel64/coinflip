# Coinflip Program Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An Anchor program for a 1v1 SPL-token coinflip game settled by ORAO Callback VRF, with a configurable protocol fee.

**Architecture:** Single Anchor program `coinflip` in an Anchor workspace. Two state accounts (`Config` singleton PDA, `Game` keypair account) plus a token-escrow PDA per game. ORAO Callback VRF (`orao-solana-vrf-cb` 0.4) settles games via a request-level callback; permissionless `settle_fallback` and `refund_timeout` guarantee funds can't get stuck. Spec: `docs/superpowers/specs/2026-08-18-coinflip-program-design.md`.

**Tech Stack:** Rust 1.93.0, **Anchor 0.32.1** (pinned by the `orao-solana-vrf-cb` dependency — NOT 1.x), Agave/Solana 2.3.x, `orao-solana-vrf-cb` 0.4 (cpi feature), `anchor-spl` (TokenInterface), LiteSVM for e2e tests, proptest for math.

**Key reference material** (read if stuck):
- ORAO callback example client program: `https://github.com/orao-network/solana-vrf/tree/master/callback/rust/examples/cpi` (also cloned during planning; re-clone if needed). Our `join_game`/`settle_callback` follow its `request.rs`/`request_level_callback.rs` exactly.
- ORAO facts verified during planning:
  - Program ID: `VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU`.
  - Request CPI accounts: `payer` (tx signer; pays ONLY tx fees), `state` (our Config PDA; must sign the CPI), `client` (ORAO Client PDA; pays request fee + rent), `network_state`, `treasury` (ORAO's), `request`, `system_program`. Accounts 8+ of the instruction authorize "arbitrary writable" callback accounts.
  - PDA seeds (constants exported by the crate): client = `[CB_CLIENT_ACCOUNT_SEED, program_id, state]`, network state = `[CB_CONFIG_ACCOUNT_SEED]`, request = `[CB_REQUEST_ACCOUNT_SEED, client, seed]`, all under the ORAO program id.
  - Callback fixed account prefix: 1. Client PDA (signer), 2. state PDA (writable), 3. NetworkState, 4. RequestAccount, then the `RemainingAccount` list given at request time, in order.
  - `RemainingAccount::readonly(pk)` / `arbitrary_writable(pk)` (must be passed writable to the Request CPI) / `writable(pk, seeds_with_bump)` (client-program PDA).
  - The crate's instruction handlers are **empty stubs** — the deployed binary holds the real logic. Tests therefore can't execute a real `fulfill`; see Task 7's strategy.
  - `RequestAccount::fulfilled() -> Option<&Fulfilled>`, `Fulfilled { randomness: [u8; 64], .. }`, `RequestAccount::find_address(&client, &seed, &vrf_id)`, `Client::find_address(&program, &state, &vrf_id)`, `NetworkState::find_address(&vrf_id)`, `NetworkState.config.request_fee: u64`.

---

## File structure (final)

```
Anchor.toml  Cargo.toml  rust-toolchain.toml  .gitignore  README.md  CHANGELOG.md
programs/coinflip/
  Cargo.toml
  src/
    lib.rs               # declare_id! + #[program]; bodies delegate only
    constants.rs         # seeds, MAX_FEE_BPS, BPS_DENOMINATOR
    errors.rs            # CoinflipError (append-only)
    events.rs            # 5 events
    math.rs              # fee_amount, pot_amount (+ unit/proptests)
    state/mod.rs
    state/config.rs      # Config PDA
    state/game.rs        # Game, GameState, Side (+ unit tests)
    instructions/mod.rs
    instructions/initialize_config.rs
    instructions/update_config.rs
    instructions/create_game.rs      # incl. validate_mint()
    instructions/cancel_game.rs
    instructions/join_game.rs        # ORAO Request CPI
    instructions/settlement.rs       # shared core: execute_settlement()
    instructions/settle_callback.rs
    instructions/settle_fallback.rs
    instructions/refund_timeout.rs
  tests/
    fixtures/orao_vrf_cb.so          # dumped mainnet binary (checked in)
    common/mod.rs                    # LiteSVM harness + ix builders
    e2e_config.rs  e2e_create_cancel.rs  e2e_join.rs  e2e_settle.rs  e2e_refund.rs
scripts/
  package.json  tsconfig.json  register.ts   # one-time ORAO register + deposit
.github/workflows/ci.yml
```

Version note that governs everything: **our program's `anchor-lang` MUST be 0.32.1** because `orao-solana-vrf-cb` 0.4 pins `anchor-lang = "0.32.1"` and its account types appear in our `#[derive(Accounts)]` structs. Do not "upgrade" to Anchor 1.x; revisit only when ORAO publishes a 1.x-compatible release.

---

### Task 1: Toolchain + workspace scaffold

**Files:**
- Delete: `src/main.rs`, root `Cargo.toml` (cargo-new leftovers)
- Create: `rust-toolchain.toml`, `Anchor.toml`, `Cargo.toml` (workspace), `programs/coinflip/Cargo.toml`, `programs/coinflip/src/lib.rs`
- Modify: `.gitignore`

- [ ] **Step 1: Verify/install the toolchain**

Run:
```bash
rustc --version                 # any recent; rust-toolchain.toml will pin builds
avm use 0.32.1 || (avm install 0.32.1 && avm use 0.32.1)
anchor --version                # expect: anchor-cli 0.32.1
solana --version                # expect: 2.3.x; if not: agave-install init 2.3.9
```
Expected: `anchor-cli 0.32.1`, `solana-cli 2.3.x`. If `avm`/`solana` are missing, install per their docs before continuing.

- [ ] **Step 2: Remove cargo-new leftovers and write the workspace files**

```bash
rm src/main.rs && rmdir src && rm Cargo.toml
```

Create `rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.93.0"
```

Create `Cargo.toml` (workspace root):
```toml
[workspace]
members = ["programs/*"]
resolver = "2"

[workspace.dependencies]
anchor-lang = "0.32.1"
anchor-spl = "0.32.1"
orao-solana-vrf-cb = { version = "0.4", default-features = false, features = ["cpi"] }
num_enum = "0.7"
static_assertions = "1.1"

[workspace.lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = [
    'cfg(target_os, values("solana"))',
] }

[profile.release]
overflow-checks = true
lto = "fat"
codegen-units = 1
```

Create `Anchor.toml`:
```toml
[toolchain]
anchor_version = "0.32.1"
solana_version = "2.3.9"

[programs.localnet]
coinflip = "11111111111111111111111111111111"

[provider]
cluster = "Localnet"
wallet = "~/.config/solana/id.json"

[scripts]
test = "cargo test"
```

Create `programs/coinflip/Cargo.toml`:
```toml
[package]
name = "coinflip"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "lib"]
name = "coinflip"

[lints]
workspace = true

[features]
default = []
cpi = ["no-entrypoint"]
no-entrypoint = []
no-idl = []
no-log-ix-name = []
idl-build = ["anchor-lang/idl-build", "anchor-spl/idl-build", "orao-solana-vrf-cb/idl-build"]

[dependencies]
anchor-lang = { workspace = true, features = ["event-cpi", "init-if-needed"] }
anchor-spl = { workspace = true }
orao-solana-vrf-cb = { workspace = true }
num_enum = { workspace = true }
static_assertions = { workspace = true }

[dev-dependencies]
litesvm = "0.6"
proptest = "1.6"
solana-sdk = "2.3"
```
(If `cargo build` later reports version conflicts between `litesvm`/`solana-sdk` and anchor's solana crates, adjust these two dev-dependency versions to the resolver's suggestion — they must sit on the same solana 2.x line as anchor 0.32.)

Create `programs/coinflip/src/lib.rs`:
```rust
use anchor_lang::prelude::*;

declare_id!("11111111111111111111111111111111");

#[program]
pub mod coinflip {}
```

Append to `.gitignore`:
```
target/
.anchor/
node_modules/
test-ledger/
```

- [ ] **Step 3: Build and sync the program id**

```bash
anchor build          # generates target/deploy/coinflip-keypair.json
anchor keys sync      # rewrites declare_id! and Anchor.toml with the real id
anchor build
```
Expected: second build succeeds; `lib.rs` and `Anchor.toml` now carry the generated program id (keep it from here on).

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: anchor workspace scaffold, pinned toolchain"
```

---

### Task 2: Constants and errors

**Files:**
- Create: `programs/coinflip/src/constants.rs`, `programs/coinflip/src/errors.rs`
- Modify: `programs/coinflip/src/lib.rs`

- [ ] **Step 1: Write `constants.rs`**

```rust
use anchor_lang::prelude::*;

#[constant]
pub const CONFIG_SEED: &[u8] = b"config";

#[constant]
pub const ESCROW_SEED: &[u8] = b"escrow";

/// Hard cap on the protocol fee: 10%.
#[constant]
pub const MAX_FEE_BPS: u16 = 1_000;

#[constant]
pub const BPS_DENOMINATOR: u16 = 10_000;
```

- [ ] **Step 2: Write `errors.rs`**

```rust
use anchor_lang::prelude::*;

/// Append-only: error codes are ABI.
#[error_code]
#[derive(PartialEq)]
pub enum CoinflipError {
    #[msg("fee_bps exceeds MAX_FEE_BPS")]
    FeeTooHigh, // 6000
    #[msg("game is not in the required state")]
    InvalidGameState, // 6001
    #[msg("invalid side value")]
    InvalidSide, // 6002
    #[msg("bet amount must be greater than zero")]
    ZeroAmount, // 6003
    #[msg("host cannot join their own game")]
    HostCannotJoin, // 6004
    #[msg("token account does not match the game")]
    MintMismatch, // 6005
    #[msg("mint has an unsupported extension")]
    UnsupportedMintExtension, // 6006
    #[msg("randomness request is not fulfilled yet")]
    RandomnessNotFulfilled, // 6007
    #[msg("randomness already fulfilled; call settle_fallback instead")]
    AlreadyFulfilled, // 6008
    #[msg("callback caller is not the registered VRF client")]
    UnauthorizedVrfClient, // 6009
    #[msg("refund timeout has not been reached")]
    TimeoutNotReached, // 6010
    #[msg("numerical overflow")]
    NumericalOverflow, // 6011
    #[msg("account owner does not match")]
    OwnerMismatch, // 6012
}
```

- [ ] **Step 3: Wire modules into `lib.rs`** (above `declare_id!`):

```rust
pub mod constants;
pub mod errors;
```

- [ ] **Step 4: Check + commit**

```bash
cargo check -p coinflip
git add -A && git commit -m "feat: constants and error enum"
```

---

### Task 3: Fee math (TDD)

**Files:**
- Create: `programs/coinflip/src/math.rs`
- Modify: `programs/coinflip/src/lib.rs` (add `pub mod math;`)

- [ ] **Step 1: Write the failing tests** — create `math.rs` containing ONLY the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fee_is_one_percent_of_10_sol() {
        // 10 SOL pot, 1% fee => 0.1 SOL fee (the spec's example)
        assert_eq!(fee_amount(10_000_000_000, 100).unwrap(), 100_000_000);
    }

    #[test]
    fn fee_rounds_down_in_winners_favor() {
        assert_eq!(fee_amount(99, 100).unwrap(), 0);
        assert_eq!(fee_amount(199, 100).unwrap(), 1);
    }

    #[test]
    fn pot_overflow_is_an_error() {
        assert!(pot_amount(u64::MAX).is_err());
        assert_eq!(pot_amount(5).unwrap(), 10);
    }

    proptest! {
        #[test]
        fn fee_never_exceeds_pot(pot in 0u64.., bps in 0u16..=10_000) {
            prop_assert!(fee_amount(pot, bps).unwrap() <= pot);
        }

        #[test]
        fn fee_is_monotonic_in_bps(pot in 0u64.., bps in 0u16..1_000) {
            prop_assert!(fee_amount(pot, bps).unwrap() <= fee_amount(pot, bps + 1).unwrap());
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p coinflip math`
Expected: FAIL — `fee_amount`/`pot_amount` not found.

- [ ] **Step 3: Implement** — prepend to `math.rs`:

```rust
use anchor_lang::prelude::*;

use crate::{constants::BPS_DENOMINATOR, errors::CoinflipError};

/// Protocol fee on the whole pot, rounded DOWN (in the winner's favor —
/// explicit decision recorded in the design spec).
pub fn fee_amount(pot: u64, fee_bps: u16) -> Result<u64> {
    let fee = (pot as u128)
        .checked_mul(fee_bps as u128)
        .ok_or(CoinflipError::NumericalOverflow)?
        / BPS_DENOMINATOR as u128;
    u64::try_from(fee).map_err(|_| error!(CoinflipError::NumericalOverflow))
}

pub fn pot_amount(stake: u64) -> Result<u64> {
    stake
        .checked_mul(2)
        .ok_or_else(|| error!(CoinflipError::NumericalOverflow))
}
```
Add `pub mod math;` to `lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p coinflip math`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: fee math with proptest coverage"
```

---

### Task 4: State accounts (TDD)

**Files:**
- Create: `programs/coinflip/src/state/mod.rs`, `state/config.rs`, `state/game.rs`
- Modify: `programs/coinflip/src/lib.rs` (add `pub mod state;`)

- [ ] **Step 1: Write `state/mod.rs`**

```rust
pub mod config;
pub mod game;

pub use config::*;
pub use game::*;
```

- [ ] **Step 2: Write `state/config.rs`**

```rust
use anchor_lang::prelude::*;
use static_assertions::const_assert_eq;

use crate::{constants::MAX_FEE_BPS, errors::CoinflipError};

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub version: u8,
    pub bump: u8,
    /// Can call update_config.
    pub admin: Pubkey,
    /// Authority whose token accounts receive fees.
    pub treasury: Pubkey,
    /// Fee on the pot, in basis points. Capped at MAX_FEE_BPS.
    pub fee_bps: u16,
    /// Slots after join before refund_timeout is allowed.
    pub refund_timeout_slots: u64,
    pub _reserved: [u8; 64],
}

const_assert_eq!(Config::INIT_SPACE, 1 + 1 + 32 + 32 + 2 + 8 + 64);

impl Config {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn validate_fee(fee_bps: u16) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, CoinflipError::FeeTooHigh);
        Ok(())
    }
}
```

- [ ] **Step 3: Write `state/game.rs` with tests first at the bottom**

```rust
use anchor_lang::prelude::*;
use num_enum::{IntoPrimitive, TryFromPrimitive};
use static_assertions::const_assert_eq;

use crate::errors::CoinflipError;

/// Discriminant 0 must be the zeroed-account default meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum GameState {
    Open = 0,
    AwaitingRandomness = 1,
    Settled = 2,
    Cancelled = 3,
    Refunded = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum Side {
    Heads = 0,
    Tails = 1,
}

impl Side {
    pub fn from_randomness(randomness: &[u8; 64]) -> Self {
        if randomness[0] & 1 == 0 {
            Side::Heads
        } else {
            Side::Tails
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Game {
    pub version: u8,
    /// GameState as u8 — an account byte is untrusted input, convert via state().
    pub state: u8,
    /// Side as u8.
    pub host_side: u8,
    pub escrow_bump: u8,
    pub host: Pubkey,
    /// Pubkey::default() until joined.
    pub joiner: Pubkey,
    pub token_mint: Pubkey,
    /// Per-player stake in base units.
    pub amount: u64,
    /// Fee snapshot from Config at create (added in the Task 6 review round).
    pub fee_bps: u16,
    pub host_token_account: Pubkey,
    pub joiner_token_account: Pubkey,
    pub joined_at_slot: u64,
    pub _reserved: [u8; 62],
}

const_assert_eq!(
    Game::INIT_SPACE,
    1 + 1 + 1 + 1 + 32 + 32 + 32 + 8 + 2 + 32 + 32 + 8 + 62
);

impl Game {
    pub const LAYOUT_VERSION: u8 = 1;

    pub fn state(&self) -> Result<GameState> {
        GameState::try_from(self.state).map_err(|_| error!(CoinflipError::InvalidGameState))
    }

    pub fn host_side(&self) -> Result<Side> {
        Side::try_from(self.host_side).map_err(|_| error!(CoinflipError::InvalidSide))
    }

    pub fn require_state(&self, expected: GameState) -> Result<()> {
        require!(self.state()? == expected, CoinflipError::InvalidGameState);
        Ok(())
    }

    /// Returns (winner, winner_token_account) for the flipped outcome.
    pub fn winner(&self, outcome: Side) -> Result<(Pubkey, Pubkey)> {
        if outcome == self.host_side()? {
            Ok((self.host, self.host_token_account))
        } else {
            Ok((self.joiner, self.joiner_token_account))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_bytes_round_trip() {
        for s in [
            GameState::Open,
            GameState::AwaitingRandomness,
            GameState::Settled,
            GameState::Cancelled,
            GameState::Refunded,
        ] {
            assert_eq!(GameState::try_from(u8::from(s)).unwrap(), s);
        }
        assert!(GameState::try_from(5u8).is_err());
        assert!(Side::try_from(2u8).is_err());
    }

    #[test]
    fn outcome_from_randomness_parity() {
        let mut r = [0u8; 64];
        assert_eq!(Side::from_randomness(&r), Side::Heads);
        r[0] = 1;
        assert_eq!(Side::from_randomness(&r), Side::Tails);
        r[0] = 0xFE;
        assert_eq!(Side::from_randomness(&r), Side::Heads);
    }

    #[test]
    fn winner_mapping() {
        let host = Pubkey::new_unique();
        let joiner = Pubkey::new_unique();
        let host_ta = Pubkey::new_unique();
        let joiner_ta = Pubkey::new_unique();
        let mut game = Game {
            version: 1,
            state: GameState::AwaitingRandomness.into(),
            host_side: Side::Heads.into(),
            escrow_bump: 255,
            host,
            joiner,
            token_mint: Pubkey::new_unique(),
            amount: 5,
            fee_bps: 100,
            host_token_account: host_ta,
            joiner_token_account: joiner_ta,
            joined_at_slot: 0,
            _reserved: [0; 62],
        };
        assert_eq!(game.winner(Side::Heads).unwrap(), (host, host_ta));
        assert_eq!(game.winner(Side::Tails).unwrap(), (joiner, joiner_ta));
        game.host_side = Side::Tails.into();
        assert_eq!(game.winner(Side::Tails).unwrap(), (host, host_ta));
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p coinflip state`
Expected: PASS (3 tests). Also `cargo check -p coinflip` clean (the `const_assert_eq!` lines prove the layout).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: Config and Game state with layout asserts"
```

> **Post-review amendments (applied after Task 4's code review):** `Game::winner()`
> was replaced by `winner_is_host(outcome) -> Result<bool>` (callers pick the
> account/pubkey themselves — no pubkey re-comparison in the payout path);
> `Side::from_byte(u8) -> Result<Side>` centralizes the InvalidSide mapping;
> tests now also pin enum discriminant VALUES, the borsh byte layout/offsets
> (crank memcmp depends on state at offset 9), require_state/state()/host_side()
> error paths, and validate_fee boundaries. Later tasks' snippets already reflect
> the new API.

---

### Task 5: Events

**Files:**
- Create: `programs/coinflip/src/events.rs`
- Modify: `programs/coinflip/src/lib.rs` (add `pub mod events;`)

- [ ] **Step 1: Write `events.rs`** — events are the durable history (game accounts close on terminal states), so they carry enough for an indexer:

```rust
use anchor_lang::prelude::*;

#[event]
pub struct GameCreated {
    pub game: Pubkey,
    pub host: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub host_side: u8,
    /// Fee snapshot the game was created under (events are the only durable
    /// history once accounts close).
    pub fee_bps: u16,
}

#[event]
pub struct GameJoined {
    pub game: Pubkey,
    pub joiner: Pubkey,
    pub vrf_request: Pubkey,
}

#[event]
pub struct GameSettled {
    pub game: Pubkey,
    pub winner: Pubkey,
    pub mint: Pubkey,
    pub outcome: u8,
    pub pot: u64,
    pub fee: u64,
}

#[event]
pub struct GameCancelled {
    pub game: Pubkey,
    pub host: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct GameRefunded {
    pub game: Pubkey,
    pub host: Pubkey,
    pub joiner: Pubkey,
    pub mint: Pubkey,
    pub host_refund: u64,
    /// Includes any donated dust.
    pub joiner_refund: u64,
}
```

- [ ] **Step 2: Check + commit**

```bash
cargo check -p coinflip
git add -A && git commit -m "feat: event definitions"
```

---

### Task 6: initialize_config + update_config

**Files:**
- Create: `programs/coinflip/src/instructions/mod.rs`, `instructions/initialize_config.rs`, `instructions/update_config.rs`
- Modify: `programs/coinflip/src/lib.rs`

- [ ] **Step 1: Write `instructions/mod.rs`**

```rust
pub mod initialize_config;
pub mod update_config;

pub use initialize_config::*;
pub use update_config::*;
```

- [ ] **Step 2: Write `instructions/initialize_config.rs`**

```rust
use anchor_lang::prelude::*;

use crate::{constants::CONFIG_SEED, state::Config};

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, Config>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handle(
    ctx: Context<InitializeConfig>,
    admin: Pubkey,
    treasury: Pubkey,
    fee_bps: u16,
    refund_timeout_slots: u64,
) -> Result<()> {
    Config::validate_fee(fee_bps)?;
    let config = &mut ctx.accounts.config;
    config.version = Config::LAYOUT_VERSION;
    config.bump = ctx.bumps.config;
    config.admin = admin;
    config.treasury = treasury;
    config.fee_bps = fee_bps;
    config.refund_timeout_slots = refund_timeout_slots;
    config._reserved = [0; 64];
    Ok(())
}
```

- [ ] **Step 3: Write `instructions/update_config.rs`**

```rust
use anchor_lang::prelude::*;

use crate::{constants::CONFIG_SEED, errors::CoinflipError, state::Config};

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = admin @ CoinflipError::OwnerMismatch,
    )]
    pub config: Account<'info, Config>,
}

pub(crate) fn handle(
    ctx: Context<UpdateConfig>,
    new_admin: Option<Pubkey>,
    new_treasury: Option<Pubkey>,
    new_fee_bps: Option<u16>,
    new_refund_timeout_slots: Option<u64>,
) -> Result<()> {
    let config = &mut ctx.accounts.config;
    if let Some(fee_bps) = new_fee_bps {
        Config::validate_fee(fee_bps)?;
        config.fee_bps = fee_bps;
    }
    if let Some(admin) = new_admin {
        config.admin = admin;
    }
    if let Some(treasury) = new_treasury {
        config.treasury = treasury;
    }
    if let Some(slots) = new_refund_timeout_slots {
        config.refund_timeout_slots = slots;
    }
    Ok(())
}
```

- [ ] **Step 4: Wire `lib.rs`** — full file at this point:

```rust
use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

declare_id!("<the id generated in Task 1 — do not change it>");

#[program]
pub mod coinflip {
    use super::*;

    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        admin: Pubkey,
        treasury: Pubkey,
        fee_bps: u16,
        refund_timeout_slots: u64,
    ) -> Result<()> {
        instructions::initialize_config::handle(ctx, admin, treasury, fee_bps, refund_timeout_slots)
    }

    pub fn update_config(
        ctx: Context<UpdateConfig>,
        new_admin: Option<Pubkey>,
        new_treasury: Option<Pubkey>,
        new_fee_bps: Option<u16>,
        new_refund_timeout_slots: Option<u64>,
    ) -> Result<()> {
        instructions::update_config::handle(
            ctx,
            new_admin,
            new_treasury,
            new_fee_bps,
            new_refund_timeout_slots,
        )
    }
}
```

- [ ] **Step 5: Build + commit**

```bash
anchor build
git add -A && git commit -m "feat: config instructions"
```

> **Post-review amendments (applied after Task 6's code review):** handlers are
> `pub(crate) fn handle` and `instructions/mod.rs` uses plain glob re-exports (no
> `#[allow(ambiguous_glob_reexports)]`); both config handlers guard
> admin/treasury against `Pubkey::default()` (`InvalidAuthority`, 6013) and bound
> `refund_timeout_slots` to `[MIN_REFUND_TIMEOUT_SLOTS, MAX_REFUND_TIMEOUT_SLOTS]`
> (`InvalidTimeout`, 6014) via `Config::validate_timeout`; `Game` gained a
> `fee_bps: u16` snapshot field (reserved shrunk to 62) written at create and
> used by settlement, so admin fee changes never retro-apply. Later tasks'
> snippets already reflect all of this.

---

### Task 7: LiteSVM harness + config e2e tests

**Files:**
- Create: `programs/coinflip/tests/fixtures/orao_vrf_cb.so`, `programs/coinflip/tests/common/mod.rs`, `programs/coinflip/tests/e2e_config.rs`

**Test strategy (from planning research):** ORAO's crate has interface-only stubs; the deployed binary holds the logic. So: (a) the dumped mainnet `.so` executes the REAL `request` CPI path when `join_game` runs; (b) ORAO env accounts (`NetworkState`, `Client`) are hand-crafted with the crate's own types via `set_account`; (c) fulfillment is simulated by overwriting the (real, pending) request account with a crafted `Fulfilled` state and settling via `settle_fallback` — which shares `execute_settlement` with the callback, so payout logic coverage is complete; (d) `settle_callback` gets negative tests (its only unique logic is signer/seed validation); the positive callback path is verified on devnet in Task 14.

- [ ] **Step 1: Dump the ORAO program binary and check it in**

```bash
mkdir -p programs/coinflip/tests/fixtures
solana program dump VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU \
  programs/coinflip/tests/fixtures/orao_vrf_cb.so -u m
```
Expected: a `.so` file of a few hundred KB. (If mainnet dump misbehaves in tests later, re-dump from devnet with `-u d` — ORAO deploys the same program id there.)

- [ ] **Step 2: Write `tests/common/mod.rs`**

```rust
#![allow(dead_code)]

use anchor_lang::{AnchorSerialize, Discriminator, InstructionData, ToAccountMetas};
use anchor_spl::{associated_token::get_associated_token_address, token::spl_token};
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
    system_program,
    transaction::{Transaction, TransactionError},
};

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
    svm.airdrop(&payer.pubkey(), 1_000 * LAMPORTS_PER_SOL).unwrap();
    (svm, payer)
}

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
        SolanaAccount { lamports, data, owner, executable: false, rent_epoch: 0 },
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
        SolanaAccount { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 },
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
        SolanaAccount { lamports, data, owner: spl_token::ID, executable: false, rent_epoch: 0 },
    )
    .unwrap();
}

pub fn token_balance(svm: &LiteSVM, address: &Pubkey) -> u64 {
    let account = svm.get_account(address).expect("token account missing");
    spl_token::state::Account::unpack(&account.data).unwrap().amount
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
    write_anchor_account(svm, client_addr, orao_solana_vrf_cb::ID, &client, 10 * LAMPORTS_PER_SOL);

    OraoEnv { network_state: ns_addr, client: client_addr, orao_treasury }
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
        accounts: coinflip::accounts::UpdateConfig { admin, config: config_pda() }
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
```
(Note: field names inside `coinflip::instruction::*` structs are the handler argument names — keep them in sync if you rename arguments. If a litesvm method name differs on the pinned version — e.g. `minimum_balance_for_rent_exemption` — check `docs.rs/litesvm` and adjust the helper only.)

- [ ] **Step 3: Write `tests/e2e_config.rs`**

```rust
mod common;

use common::*;
use solana_sdk::{pubkey::Pubkey, signature::Signer};

#[test]
fn initialize_and_update_config() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    let treasury = Pubkey::new_unique();

    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), admin, treasury, DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();

    // fee update within cap works
    send(&mut svm, &[&payer], &[ix_update_config(admin, None, None, Some(250), None)]).unwrap();
}

#[test]
fn initialize_rejects_fee_above_cap() {
    let (mut svm, payer) = setup();
    let result = send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::new_unique(), 1_001, DEFAULT_TIMEOUT_SLOTS,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::FeeTooHigh);
}

#[test]
fn update_config_rejects_non_admin() {
    let (mut svm, payer) = setup();
    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::new_unique(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();

    let mallory = solana_sdk::signature::Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let result = send(&mut svm, &[&mallory], &[ix_update_config(
        mallory.pubkey(), None, None, Some(0), None,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
}

#[test]
fn initialize_config_is_one_shot() {
    let (mut svm, payer) = setup();
    let ix = ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::new_unique(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    );
    send(&mut svm, &[&payer], &[ix.clone()]).unwrap();
    // second init must fail: the PDA already exists
    assert!(send(&mut svm, &[&payer], &[ix]).is_err());
}

#[test]
fn update_config_rejects_fee_above_cap_and_bad_timeout() {
    let (mut svm, payer) = setup();
    let admin = payer.pubkey();
    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), admin, Pubkey::new_unique(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();
    let result = send(&mut svm, &[&payer], &[ix_update_config(admin, None, None, Some(1_001), None)]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::FeeTooHigh);
    let result = send(&mut svm, &[&payer], &[ix_update_config(admin, None, None, None, Some(0))]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidTimeout);
}

#[test]
fn default_key_authorities_are_rejected() {
    let (mut svm, payer) = setup();
    let result = send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::default(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidAuthority);

    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::new_unique(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();
    let result = send(&mut svm, &[&payer], &[ix_update_config(
        payer.pubkey(), Some(Pubkey::default()), None, None, None,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidAuthority);
}

#[test]
fn admin_rotation_round_trip() {
    let (mut svm, payer) = setup();
    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), Pubkey::new_unique(), DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();
    let new_admin = solana_sdk::signature::Keypair::new();
    svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    // old admin rotates to new
    send(&mut svm, &[&payer], &[ix_update_config(
        payer.pubkey(), Some(new_admin.pubkey()), None, None, None,
    )])
    .unwrap();
    // old admin is now rejected
    let result = send(&mut svm, &[&payer], &[ix_update_config(payer.pubkey(), None, None, Some(200), None)]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::OwnerMismatch);
    // new admin works
    send(&mut svm, &[&new_admin], &[ix_update_config(
        new_admin.pubkey(), None, None, Some(200), None,
    )])
    .unwrap();
}
```

- [ ] **Step 4: Build + run**

```bash
anchor build && cargo test -p coinflip --test e2e_config
```
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test: LiteSVM harness and config e2e"
```

---

### Task 8: create_game

**Files:**
- Create: `programs/coinflip/src/instructions/create_game.rs`
- Modify: `instructions/mod.rs`, `lib.rs`, `tests/common/mod.rs`
- Test: `programs/coinflip/tests/e2e_create_cancel.rs`

- [ ] **Step 1: Write `instructions/create_game.rs`**

```rust
use anchor_lang::prelude::*;
use anchor_spl::{
    token_2022::spl_token_2022::{
        self,
        extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    },
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameCreated,
    state::{Config, Game, GameState, Side},
};

#[event_cpi]
#[derive(Accounts)]
pub struct CreateGame<'info> {
    #[account(mut)]
    pub host: Signer<'info>,
    /// Fee snapshot source; games settle at the fee they were created under.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    /// Fresh keypair account — its pubkey IS the game id (it signs init only).
    #[account(init, payer = host, space = 8 + Game::INIT_SPACE)]
    pub game: Box<Account<'info, Game>>,
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(
        init,
        payer = host,
        seeds = [ESCROW_SEED, game.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = escrow,
        token::token_program = token_program,
    )]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = host_token_account.mint == mint.key() @ CoinflipError::MintMismatch,
        constraint = host_token_account.owner == host.key() @ CoinflipError::OwnerMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Deny-list per the design spec: transfer-fee breaks the payout math,
/// transfer hooks change the wire format, a permanent delegate can drain
/// the escrow. Freeze authority is allowed (USDC) — documented risk.
fn validate_mint(mint_info: &AccountInfo) -> Result<()> {
    if *mint_info.owner == anchor_spl::token::ID {
        return Ok(());
    }
    let data = mint_info.try_borrow_data()?;
    let mint = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&data)
        .map_err(|_| error!(CoinflipError::UnsupportedMintExtension))?;
    let extensions = mint
        .get_extension_types()
        .map_err(|_| error!(CoinflipError::UnsupportedMintExtension))?;
    for extension in extensions {
        match extension {
            ExtensionType::TransferFeeConfig
            | ExtensionType::TransferHook
            | ExtensionType::PermanentDelegate => {
                return err!(CoinflipError::UnsupportedMintExtension)
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn handle(ctx: Context<CreateGame>, side: u8, amount: u64) -> Result<()> {
    require!(amount > 0, CoinflipError::ZeroAmount);
    let side = Side::from_byte(side)?;
    validate_mint(&ctx.accounts.mint.to_account_info())?;

    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.host_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.host.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;

    let game = &mut ctx.accounts.game;
    game.version = Game::LAYOUT_VERSION;
    game.state = GameState::Open.into();
    game.host_side = side.into();
    game.escrow_bump = ctx.bumps.escrow;
    game.host = ctx.accounts.host.key();
    game.joiner = Pubkey::default();
    game.token_mint = ctx.accounts.mint.key();
    game.amount = amount;
    game.fee_bps = ctx.accounts.config.fee_bps;
    game.host_token_account = ctx.accounts.host_token_account.key();
    game.joiner_token_account = Pubkey::default();
    game.joined_at_slot = 0;
    game._reserved = [0; 62];

    emit_cpi!(GameCreated {
        game: game.key(),
        host: game.host,
        mint: game.token_mint,
        amount,
        host_side: game.host_side,
        fee_bps: game.fee_bps,
    });
    Ok(())
}
```

- [ ] **Step 2: Wire up** — `instructions/mod.rs` add `pub mod create_game;` + `pub use create_game::*;`. In `lib.rs`'s `#[program]` module add:

```rust
    pub fn create_game(ctx: Context<CreateGame>, side: u8, amount: u64) -> Result<()> {
        instructions::create_game::handle(ctx, side, amount)
    }
```

- [ ] **Step 3: Add builder to `tests/common/mod.rs`**

```rust
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
    send(svm, &[payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), treasury, DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();

    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10 * LAMPORTS_PER_SOL).unwrap();
    let mint = create_mint(svm, 9);
    let host_token_account = create_token_account(svm, mint, host.pubkey(), amount * 10);

    let game = Keypair::new();
    send(svm, &[&host, &game], &[ix_create_game(
        host.pubkey(), game.pubkey(), mint, host_token_account, 0, amount,
    )])
    .unwrap();

    let escrow = escrow_pda(&game.pubkey());
    GameFixture { host, game, mint, host_token_account, escrow, treasury, amount }
}
```

- [ ] **Step 4: Write `tests/e2e_create_cancel.rs`** (create half; cancel tests come in Task 9)

```rust
mod common;

use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn create_game_escrows_the_stake() {
    let (mut svm, payer) = setup();
    let fixture = setup_open_game(&mut svm, &payer, 5_000_000_000);
    assert_eq!(token_balance(&svm, &fixture.escrow), 5_000_000_000);
    assert_eq!(token_balance(&svm, &fixture.host_token_account), 45_000_000_000);
}

#[test]
fn create_game_rejects_zero_amount() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), treasury, DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(&mut svm, &[&host, &game], &[ix_create_game(
        host.pubkey(), game.pubkey(), mint, host_ta, 0, 0,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::ZeroAmount);
}

#[test]
fn create_game_rejects_invalid_side() {
    let (mut svm, payer) = setup();
    let treasury = solana_sdk::pubkey::Pubkey::new_unique();
    send(&mut svm, &[&payer], &[ix_initialize_config(
        payer.pubkey(), payer.pubkey(), treasury, DEFAULT_FEE_BPS, DEFAULT_TIMEOUT_SLOTS,
    )])
    .unwrap();
    let host = Keypair::new();
    svm.airdrop(&host.pubkey(), 10_000_000_000).unwrap();
    let mint = create_mint(&mut svm, 9);
    let host_ta = create_token_account(&mut svm, mint, host.pubkey(), 100);
    let game = Keypair::new();
    let result = send(&mut svm, &[&host, &game], &[ix_create_game(
        host.pubkey(), game.pubkey(), mint, host_ta, 2, 10,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidSide);
}
```

- [ ] **Step 5: Build + run + commit**

```bash
anchor build && cargo test -p coinflip --test e2e_create_cancel
git add -A && git commit -m "feat: create_game with mint validation"
```
Expected: PASS (3 tests).

---

### Task 9: cancel_game

**Files:**
- Create: `programs/coinflip/src/instructions/cancel_game.rs`
- Modify: `instructions/mod.rs`, `lib.rs`, `tests/common/mod.rs`, `tests/e2e_create_cancel.rs`

- [ ] **Step 1: Write `instructions/cancel_game.rs`**

```rust
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    events::GameCancelled,
    state::{Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct CancelGame<'info> {
    #[account(mut)]
    pub host: Signer<'info>,
    #[account(
        mut,
        close = host,
        has_one = host @ CoinflipError::OwnerMismatch,
    )]
    pub game: Box<Account<'info, Game>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any host-owned account of the game mint (liveness: the recorded one may
    /// have been closed since create).
    #[account(
        mut,
        constraint = host_token_account.owner == game.host @ CoinflipError::OwnerMismatch,
        constraint = host_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<CancelGame>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::Open)?;

    let game_key = ctx.accounts.game.key();
    let seeds: &[&[&[u8]]] = &[&[
        ESCROW_SEED,
        game_key.as_ref(),
        &[ctx.accounts.game.escrow_bump],
    ]];

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.host_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            seeds,
        ),
        ctx.accounts.escrow.amount,
        ctx.accounts.mint.decimals,
    )?;
    token_interface::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        CloseAccount {
            account: ctx.accounts.escrow.to_account_info(),
            destination: ctx.accounts.host.to_account_info(),
            authority: ctx.accounts.escrow.to_account_info(),
        },
        seeds,
    ))?;

    ctx.accounts.game.state = GameState::Cancelled.into();
    emit_cpi!(GameCancelled {
        game: game_key,
        host: ctx.accounts.game.host,
        mint: ctx.accounts.game.token_mint,
        amount: ctx.accounts.game.amount,
    });
    Ok(())
}
```

- [ ] **Step 2: Wire up** — `instructions/mod.rs`: add `pub mod cancel_game;` + `pub use cancel_game::*;`. `lib.rs`:

```rust
    pub fn cancel_game(ctx: Context<CancelGame>) -> Result<()> {
        instructions::cancel_game::handle(ctx)
    }
```

- [ ] **Step 3: Add builder to `tests/common/mod.rs`**

```rust
pub fn ix_cancel_game(f: &GameFixture) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::CancelGame {
            host: f.host.pubkey(),
            game: f.game.pubkey(),
            mint: f.mint,
            escrow: f.escrow,
            host_token_account: f.host_token_account,
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::CancelGame {}.data(),
    }
}
```

- [ ] **Step 4: Append tests to `tests/e2e_create_cancel.rs`**

```rust
#[test]
fn cancel_refunds_host_and_closes_accounts() {
    let (mut svm, payer) = setup();
    let f = setup_open_game(&mut svm, &payer, 5_000_000_000);
    send(&mut svm, &[&f.host], &[ix_cancel_game(&f)]).unwrap();
    assert_eq!(token_balance(&svm, &f.host_token_account), 50_000_000_000);
    assert!(svm.get_account(&f.escrow).map_or(true, |a| a.lamports == 0));
    assert!(svm.get_account(&f.game.pubkey()).map_or(true, |a| a.lamports == 0));
}

#[test]
fn cancel_by_non_host_fails() {
    let (mut svm, payer) = setup();
    let f = setup_open_game(&mut svm, &payer, 1_000);
    let mallory = Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let mut ix = ix_cancel_game(&f);
    ix.accounts[0].pubkey = mallory.pubkey(); // host slot
    let result = send(&mut svm, &[&mallory], &[ix]);
    assert!(result.is_err()); // has_one = host fails
}
```

- [ ] **Step 5: Build + run + commit**

```bash
anchor build && cargo test -p coinflip --test e2e_create_cancel
git add -A && git commit -m "feat: cancel_game"
```
Expected: PASS (5 tests).

> **Post-review amendments (applied after Task 9's code review):** the cancel
> suite grew to 12 tests (stake-cap boundary, liveness refund accounts,
> wrong-mint/non-owned refund rejections, event + rent-delta assertions);
> `ix_cancel_game` delegates to `ix_cancel_game_with_refund_account`;
> `GameCancelled`/`GameRefunded` were enriched pre-ABI for standalone
> indexability (see the Task 5 snippet, already updated). Note: the
> `state = Cancelled` write is dead (Anchor `close` skips serialization) — the
> re-cancel guard is account closure itself; never make that byte load-bearing.

---

### Task 10: join_game (ORAO Request CPI)

**Files:**
- Create: `programs/coinflip/src/instructions/join_game.rs`
- Modify: `instructions/mod.rs`, `lib.rs`, `tests/common/mod.rs`
- Test: `programs/coinflip/tests/e2e_join.rs`

The callback account list is FIXED here, at request time. Order (after ORAO's 4 fixed accounts) — this order must match `SettleCallback`'s struct order in Task 12 exactly:
`game(w), escrow(w), host(w), host_token_account(w), joiner_token_account(w), treasury_token_account(w), mint(ro), token_program(ro), event_authority(ro), program(ro)`.
All six writables are "arbitrary writable" — they must ALSO be appended as writable remaining accounts to the Request CPI (that's how ORAO authorizes them).

- [ ] **Step 1: Write `instructions/join_game.rs`**

```rust
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, system_instruction};
use anchor_spl::{
    associated_token::AssociatedToken,
    token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked},
};
use orao_solana_vrf_cb::{
    cpi as orao_cpi,
    program::OraoVrfCb,
    state::{
        client::{Callback, Client, RemainingAccount},
        network_state::NetworkState,
    },
    RequestParams, CB_CLIENT_ACCOUNT_SEED, CB_CONFIG_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameJoined,
    state::{Config, Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct JoinGame<'info> {
    #[account(mut)]
    pub joiner: Signer<'info>,
    /// Registered as the ORAO client state PDA; signs the Request CPI.
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut)]
    pub game: Box<Account<'info, Game>>,
    /// CHECK: rent receiver for terminal closes; authorized writable for the callback.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = joiner_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
        constraint = joiner_token_account.owner == joiner.key() @ CoinflipError::OwnerMismatch,
    )]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = game.host_token_account @ CoinflipError::MintMismatch)]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: fee-destination authority recorded in config.
    #[account(address = config.treasury @ CoinflipError::OwnerMismatch)]
    pub treasury: AccountInfo<'info>,
    /// Must exist by settlement time — the oracle's callback cannot pay rent.
    #[account(
        init_if_needed,
        payer = joiner,
        associated_token::mint = mint,
        associated_token::authority = treasury,
        associated_token::token_program = token_program,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    pub vrf: Program<'info, OraoVrfCb>,
    #[account(
        mut,
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
    )]
    pub client: Box<Account<'info, Client>>,
    #[account(
        mut,
        seeds = [CB_CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = network_state.bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    /// CHECK: asserted by the CPI.
    #[account(mut, address = network_state.config.treasury)]
    pub orao_treasury: AccountInfo<'info>,
    /// CHECK: created (and PDA-validated against the seed we pass) by the ORAO
    /// CPI itself. The seed is sha256("coinflip-vrf-seed", game, joiner) —
    /// unpredictable pre-join, so the address cannot be grief-pre-funded.
    #[account(mut)]
    pub request: AccountInfo<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handle(ctx: Context<JoinGame>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::Open)?;
    require!(
        ctx.accounts.joiner.key() != ctx.accounts.game.host,
        CoinflipError::HostCannotJoin
    );

    // Matching stake into escrow.
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.joiner_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.joiner.to_account_info(),
            },
        ),
        ctx.accounts.game.amount,
        ctx.accounts.mint.decimals,
    )?;

    // The ORAO Client PDA pays the request fee; the joiner reimburses it so
    // the client balance stays neutral.
    invoke(
        &system_instruction::transfer(
            &ctx.accounts.joiner.key(),
            &ctx.accounts.client.key(),
            ctx.accounts.network_state.config.request_fee,
        ),
        &[
            ctx.accounts.joiner.to_account_info(),
            ctx.accounts.client.to_account_info(),
        ],
    )?;

    // Callback account list — order must match SettleCallback's struct.
    let game_key = ctx.accounts.game.key();
    let callback = Callback::from_instruction_data(&crate::instruction::SettleCallback {})
        .with_remaining_accounts(vec![
            RemainingAccount::arbitrary_writable(game_key),
            RemainingAccount::arbitrary_writable(ctx.accounts.escrow.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.host.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.host_token_account.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.joiner_token_account.key()),
            RemainingAccount::arbitrary_writable(ctx.accounts.treasury_token_account.key()),
            RemainingAccount::readonly(ctx.accounts.mint.key()),
            RemainingAccount::readonly(ctx.accounts.token_program.key()),
            RemainingAccount::readonly(ctx.accounts.event_authority.key()),
            RemainingAccount::readonly(crate::ID),
        ]);

    let mut cpi_accounts = orao_cpi::accounts::Request {
        payer: ctx.accounts.joiner.to_account_info(),
        state: ctx.accounts.config.to_account_info(),
        client: ctx.accounts.client.to_account_info(),
        network_state: ctx.accounts.network_state.to_account_info(),
        treasury: ctx.accounts.orao_treasury.to_account_info(),
        request: ctx.accounts.request.to_account_info(),
        system_program: ctx.accounts.system_program.to_account_info(),
    };
    // Our Config PDA is the registered request authority.
    cpi_accounts.state.is_signer = true;
    let signer_seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[ctx.accounts.config.bump]]];

    let cpi_ctx = CpiContext::new(ctx.accounts.vrf.to_account_info(), cpi_accounts)
        .with_signer(signer_seeds)
        // Arbitrary-writable callback accounts are authorized by being
        // writable accounts 8+ of the Request instruction.
        .with_remaining_accounts(vec![
            ctx.accounts.game.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            ctx.accounts.host.to_account_info(),
            ctx.accounts.host_token_account.to_account_info(),
            ctx.accounts.joiner_token_account.to_account_info(),
            ctx.accounts.treasury_token_account.to_account_info(),
        ]);
    orao_cpi::request(
        cpi_ctx,
        RequestParams::new(game_key.to_bytes()).with_callback(Some(callback)),
    )?;

    let game = &mut ctx.accounts.game;
    game.joiner = ctx.accounts.joiner.key();
    game.joiner_token_account = ctx.accounts.joiner_token_account.key();
    game.joined_at_slot = Clock::get()?.slot;
    game.state = GameState::AwaitingRandomness.into();

    emit_cpi!(GameJoined {
        game: game_key,
        joiner: game.joiner,
        vrf_request: ctx.accounts.request.key(),
    });
    Ok(())
}
```
(Note: `crate::instruction::SettleCallback` doesn't exist until Task 12. To keep this task compiling on its own, add the settle_callback SHELL in this task — Step 2.)

- [ ] **Step 2: Add a compiling shell for `settle_callback`** — create `instructions/settle_callback.rs` with just the accounts prefix and a `RandomnessNotFulfilled` bail; Task 12 completes it:

```rust
use anchor_lang::prelude::*;

use crate::errors::CoinflipError;

#[event_cpi]
#[derive(Accounts)]
pub struct SettleCallback<'info> {
    /// CHECK: completed in the settle_callback task.
    pub client: AccountInfo<'info>,
}

pub(crate) fn handle(_ctx: Context<SettleCallback>) -> Result<()> {
    err!(CoinflipError::RandomnessNotFulfilled)
}
```
Wire into `instructions/mod.rs` (`pub mod settle_callback;` / `pub use settle_callback::*;`) and `lib.rs`:

```rust
    pub fn settle_callback(ctx: Context<SettleCallback>) -> Result<()> {
        instructions::settle_callback::handle(ctx)
    }
```

- [ ] **Step 3: Wire join_game** — `instructions/mod.rs`: `pub mod join_game;` + `pub use join_game::*;`. `lib.rs`:

```rust
    pub fn join_game(ctx: Context<JoinGame>) -> Result<()> {
        instructions::join_game::handle(ctx)
    }
```

- [ ] **Step 4: Add to `tests/common/mod.rs`**

```rust
pub struct JoinedGame {
    pub fixture: GameFixture,
    pub joiner: Keypair,
    pub joiner_token_account: Pubkey,
    pub treasury_token_account: Pubkey,
    pub orao: OraoEnv,
    pub request: Pubkey,
}

pub fn ix_join_game(
    f: &GameFixture,
    orao: &OraoEnv,
    joiner: Pubkey,
    joiner_token_account: Pubkey,
) -> Instruction {
    let treasury_token_account = get_associated_token_address(&f.treasury, &f.mint);
    let request = request_pda(&orao.client, &f.game.pubkey());
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::JoinGame {
            joiner,
            config: config_pda(),
            game: f.game.pubkey(),
            host: f.host.pubkey(),
            mint: f.mint,
            escrow: f.escrow,
            joiner_token_account,
            host_token_account: f.host_token_account,
            treasury: f.treasury,
            treasury_token_account,
            vrf: orao_solana_vrf_cb::ID,
            client: orao.client,
            network_state: orao.network_state,
            orao_treasury: orao.orao_treasury,
            request,
            associated_token_program: anchor_spl::associated_token::ID,
            token_program: spl_token::ID,
            system_program: system_program::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::JoinGame {}.data(),
    }
}

/// Full flow: open game + ORAO env + join. Game ends AwaitingRandomness
/// with a REAL pending request account created by the dumped ORAO program.
pub fn setup_joined_game(svm: &mut LiteSVM, payer: &Keypair, amount: u64) -> JoinedGame {
    let fixture = setup_open_game(svm, payer, amount);
    let orao = setup_orao(svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10 * LAMPORTS_PER_SOL).unwrap();
    let joiner_token_account = create_token_account(svm, fixture.mint, joiner.pubkey(), amount * 10);
    send(svm, &[&joiner], &[ix_join_game(
        &fixture, &orao, joiner.pubkey(), joiner_token_account,
    )])
    .unwrap();
    let treasury_token_account = get_associated_token_address(&fixture.treasury, &fixture.mint);
    let request = request_pda(&orao.client, &fixture.game.pubkey());
    JoinedGame { fixture, joiner, joiner_token_account, treasury_token_account, orao, request }
}
```

- [ ] **Step 5 (carried from Task 9 review): two additional tests**

In `tests/e2e_create_cancel.rs`: `cancel_after_join_fails` — after a full join
(state AwaitingRandomness), host attempts cancel → `InvalidGameState` (this is
the guard between a joined game and the host stealing the joiner's stake — first
testable now). And a Token-2022 ALLOWED-extension happy path: a t22 mint with
`MetadataPointer` flows create → cancel end-to-end (needs the crafter to support
an allowed extension and a token_program parameter on the cancel builder).

- [ ] **Step 5: Write `tests/e2e_join.rs`**

```rust
mod common;

use common::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn join_escrows_stake_and_creates_vrf_request() {
    let (mut svm, payer) = setup();
    let joined = setup_joined_game(&mut svm, &payer, 5_000_000_000);
    // Both stakes escrowed.
    assert_eq!(token_balance(&svm, &joined.fixture.escrow), 10_000_000_000);
    // Real ORAO program created the request account.
    let request = svm.get_account(&joined.request).expect("request account must exist");
    assert_eq!(request.owner, orao_solana_vrf_cb::ID);
}

#[test]
fn host_cannot_join_own_game() {
    let (mut svm, payer) = setup();
    let f = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let result = send(&mut svm, &[&f.host], &[ix_join_game(
        &f, &orao, f.host.pubkey(), f.host_token_account,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::HostCannotJoin);
}

#[test]
fn double_join_fails() {
    let (mut svm, payer) = setup();
    let joined = setup_joined_game(&mut svm, &payer, 1_000);
    let second = Keypair::new();
    svm.airdrop(&second.pubkey(), 10_000_000_000).unwrap();
    let second_ta = create_token_account(&mut svm, joined.fixture.mint, second.pubkey(), 10_000);
    let result = send(&mut svm, &[&second], &[ix_join_game(
        &joined.fixture, &joined.orao, second.pubkey(), second_ta,
    )]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::InvalidGameState);
}

#[test]
fn join_with_wrong_mint_token_account_fails() {
    let (mut svm, payer) = setup();
    let f = setup_open_game(&mut svm, &payer, 1_000);
    let orao = setup_orao(&mut svm);
    let joiner = Keypair::new();
    svm.airdrop(&joiner.pubkey(), 10_000_000_000).unwrap();
    let wrong_mint = create_mint(&mut svm, 9);
    let wrong_ta = create_token_account(&mut svm, wrong_mint, joiner.pubkey(), 10_000);
    let result = send(&mut svm, &[&joiner], &[ix_join_game(&f, &orao, joiner.pubkey(), wrong_ta)]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::MintMismatch);
}
```

- [ ] **Step 6: Build + run + commit**

```bash
anchor build && cargo test -p coinflip --test e2e_join
git add -A && git commit -m "feat: join_game with ORAO callback VRF request"
```
Expected: PASS (4 tests).

> **Post-review amendments (applied after Task 10's code review):** the VRF seed
> is now `sha256("coinflip-vrf-seed", game, joiner)` stored in `Game.vrf_seed`
> (reserved shrunk to 30) — the request account in JoinGame is a CHECK'd
> AccountInfo whose PDA the ORAO CPI itself enforces; the joiner reimburses
> `request_fee + rent(pending request)` sized via `RequestAccount::expected_size`
> so the Client PDA is exactly neutral per join (asserted in tests);
> `orao_treasury` carries a typed error; `token_program` is constrained to the
> mint's owner; the join suite gained lamport-delta, CU-budget, T22-join, and
> three negative constraint tests. Tasks 11-13 derive the request PDA from
> `game.vrf_seed` (snippets already updated). This is the task where the real dumped ORAO binary runs — if the `request` CPI fails with an unexpected ORAO error, print the tx logs (`FailedTransactionMetadata.meta.logs`) and compare against the crate's `error.rs`; the usual suspects are the client balance being too small (raise the funding in `setup_orao`) or a stale fixture (re-dump the `.so`).

---

### Task 11: Settlement core + settle_fallback

**Files:**
- Create: `programs/coinflip/src/instructions/settlement.rs`, `instructions/settle_fallback.rs`
- Modify: `instructions/mod.rs`, `lib.rs`, `tests/common/mod.rs`
- Test: `programs/coinflip/tests/e2e_settle.rs`

- [ ] **Step 1: Write `instructions/settlement.rs`** — shared by callback and fallback:

```rust
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::{
    constants::ESCROW_SEED,
    errors::CoinflipError,
    math::fee_amount,
    state::{Game, GameState, Side},
};

pub(crate) struct SettlementOutcome {
    pub winner: Pubkey,
    pub outcome: u8,
    pub pot: u64,
    pub fee: u64,
}

/// Pays the winner, takes the fee, closes the escrow, marks the game Settled.
/// The pot is the escrow's actual balance so donated dust can never brick the
/// close. Callers close the game account (`close = host`) and emit the event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_settlement<'info>(
    game: &mut Account<'info, Game>,
    escrow: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    host_token_account: &InterfaceAccount<'info, TokenAccount>,
    joiner_token_account: &InterfaceAccount<'info, TokenAccount>,
    treasury_token_account: &InterfaceAccount<'info, TokenAccount>,
    token_program: &Interface<'info, TokenInterface>,
    host: &AccountInfo<'info>,
    randomness: &[u8; 64],
) -> Result<SettlementOutcome> {
    // Fee comes from the game's snapshot, never live config: admin fee
    // changes must not retro-apply to already-created games.
    game.require_state(GameState::AwaitingRandomness)?;

    let outcome = Side::from_randomness(randomness);
    let (winner, winner_token_account) = if game.winner_is_host(outcome)? {
        (game.host, host_token_account)
    } else {
        (game.joiner, joiner_token_account)
    };

    let pot = escrow.amount;
    let fee = fee_amount(pot, game.fee_bps)?;
    let payout = pot.checked_sub(fee).ok_or(CoinflipError::NumericalOverflow)?;

    let game_key = game.key();
    let seeds: &[&[&[u8]]] = &[&[ESCROW_SEED, game_key.as_ref(), &[game.escrow_bump]]];

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            TransferChecked {
                from: escrow.to_account_info(),
                mint: mint.to_account_info(),
                to: winner_token_account.to_account_info(),
                authority: escrow.to_account_info(),
            },
            seeds,
        ),
        payout,
        mint.decimals,
    )?;
    if fee > 0 {
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                token_program.to_account_info(),
                TransferChecked {
                    from: escrow.to_account_info(),
                    mint: mint.to_account_info(),
                    to: treasury_token_account.to_account_info(),
                    authority: escrow.to_account_info(),
                },
                seeds,
            ),
            fee,
            mint.decimals,
        )?;
    }
    token_interface::close_account(CpiContext::new_with_signer(
        token_program.to_account_info(),
        CloseAccount {
            account: escrow.to_account_info(),
            destination: host.clone(),
            authority: escrow.to_account_info(),
        },
        seeds,
    ))?;

    game.state = GameState::Settled.into();
    Ok(SettlementOutcome { winner, outcome: outcome.into(), pot, fee })
}
```

- [ ] **Step 2: Write `instructions/settle_fallback.rs`** — permissionless backstop:

```rust
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use orao_solana_vrf_cb::{
    state::{client::Client, request::RequestAccount},
    CB_CLIENT_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameSettled,
    instructions::settlement::execute_settlement,
    state::{Config, Game},
};

#[event_cpi]
#[derive(Accounts)]
pub struct SettleFallback<'info> {
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
    )]
    pub client: Box<Account<'info, Client>>,
    /// Seed binding: this must be THE request for this game.
    #[account(
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = request.bump,
    )]
    pub request: Box<Account<'info, RequestAccount>>,
    #[account(mut, close = host)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: rent receiver, must be the game's host.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    /// Any host-owned account of the game mint (liveness: recorded one may be closed).
    #[account(
        mut,
        constraint = host_token_account.owner == game.host @ CoinflipError::OwnerMismatch,
        constraint = host_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any joiner-owned account of the game mint.
    #[account(
        mut,
        constraint = joiner_token_account.owner == game.joiner @ CoinflipError::OwnerMismatch,
        constraint = joiner_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = treasury_token_account.owner == config.treasury
            @ CoinflipError::OwnerMismatch,
        constraint = treasury_token_account.mint == game.token_mint
            @ CoinflipError::MintMismatch,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<SettleFallback>) -> Result<()> {
    let randomness = ctx
        .accounts
        .request
        .fulfilled()
        .ok_or(CoinflipError::RandomnessNotFulfilled)?
        .randomness;

    let outcome = execute_settlement(
        &mut ctx.accounts.game,
        &ctx.accounts.escrow,
        &ctx.accounts.mint,
        &ctx.accounts.host_token_account,
        &ctx.accounts.joiner_token_account,
        &ctx.accounts.treasury_token_account,
        &ctx.accounts.token_program,
        &ctx.accounts.host,
        &randomness,
    )?;

    emit_cpi!(GameSettled {
        game: ctx.accounts.game.key(),
        winner: outcome.winner,
        mint: ctx.accounts.game.token_mint,
        outcome: outcome.outcome,
        pot: outcome.pot,
        fee: outcome.fee,
    });
    Ok(())
}
```

- [ ] **Step 3: Wire up** — `instructions/mod.rs`: add `pub mod settlement;`, `pub mod settle_fallback;`, `pub use settle_fallback::*;`. `lib.rs`:

```rust
    pub fn settle_fallback(ctx: Context<SettleFallback>) -> Result<()> {
        instructions::settle_fallback::handle(ctx)
    }
```

- [ ] **Step 4: Add builder to `tests/common/mod.rs`**

```rust
pub fn ix_settle_fallback(j: &JoinedGame, payer: Pubkey) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::SettleFallback {
            payer,
            config: config_pda(),
            client: j.orao.client,
            request: j.request,
            game: j.fixture.game.pubkey(),
            escrow: j.fixture.escrow,
            host: j.fixture.host.pubkey(),
            host_token_account: j.fixture.host_token_account,
            joiner_token_account: j.joiner_token_account,
            treasury_token_account: j.treasury_token_account,
            mint: j.fixture.mint,
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::SettleFallback {}.data(),
    }
}
```

- [ ] **Step 5: Write `tests/e2e_settle.rs`**

```rust
mod common;

use common::*;
use solana_sdk::signature::Signer;

const STAKE: u64 = 5_000_000_000; // 5 tokens at 9 decimals — the spec's example

fn randomness_with_first_byte(byte: u8) -> [u8; 64] {
    let mut r = [7u8; 64];
    r[0] = byte;
    r
}

#[test]
fn settle_pays_host_when_host_side_wins() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    // host_side = Heads (0); randomness[0] even => Heads => host wins.
    write_fulfilled_request(
        &mut svm, j.orao.client, j.fixture.game.pubkey(), randomness_with_first_byte(2),
    );
    send(&mut svm, &[&payer], &[ix_settle_fallback(&j, payer.pubkey())]).unwrap();

    // pot 10, fee 1% = 0.1, payout 9.9 (in base units)
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), 45_000_000_000 + 9_900_000_000);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), 100_000_000);
    assert!(svm.get_account(&j.fixture.escrow).map_or(true, |a| a.lamports == 0));
    assert!(svm.get_account(&j.fixture.game.pubkey()).map_or(true, |a| a.lamports == 0));
}

#[test]
fn settle_pays_joiner_when_host_side_loses() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    // randomness[0] odd => Tails => joiner (host picked Heads) wins.
    write_fulfilled_request(
        &mut svm, j.orao.client, j.fixture.game.pubkey(), randomness_with_first_byte(3),
    );
    send(&mut svm, &[&payer], &[ix_settle_fallback(&j, payer.pubkey())]).unwrap();
    assert_eq!(token_balance(&svm, &j.joiner_token_account), 45_000_000_000 + 9_900_000_000);
    assert_eq!(token_balance(&svm, &j.treasury_token_account), 100_000_000);
}

#[test]
fn settle_before_fulfillment_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    // Request exists (real, pending) but is NOT fulfilled.
    let result = send(&mut svm, &[&payer], &[ix_settle_fallback(&j, payer.pubkey())]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::RandomnessNotFulfilled);
}

#[test]
fn settle_twice_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm, j.orao.client, j.fixture.game.pubkey(), randomness_with_first_byte(0),
    );
    send(&mut svm, &[&payer], &[ix_settle_fallback(&j, payer.pubkey())]).unwrap();
    // Game account is closed; a second settle can't even load it.
    let result = send(&mut svm, &[&payer], &[ix_settle_fallback(&j, payer.pubkey())]);
    assert!(result.is_err());
}
```

- [ ] **Step 6: Build + run + commit**

```bash
anchor build && cargo test -p coinflip --test e2e_settle
git add -A && git commit -m "feat: settlement core and settle_fallback"
```
Expected: PASS (4 tests). Note the payout numbers implement the spec's worked example exactly.

---

### Task 12: settle_callback (real accounts + negative tests)

**Files:**
- Modify: `programs/coinflip/src/instructions/settle_callback.rs` (replace the Task 10 shell), `tests/common/mod.rs`, `tests/e2e_settle.rs`

- [ ] **Step 0 (carried from Task 11 review): two small refactors**

Move the two `require_payout_account` calls from `settle_fallback`'s handler
INSIDE `execute_settlement` (guarding host+joiner there makes the rule
unforgettable for every settlement call site; the callback's address-pinned
accounts satisfy it trivially). And in `join_game`, add a dynamic invariant
check after the network_state account is available:
```rust
    // The refund window must never open before ORAO gives up on the callback,
    // or a player could sabotage their payout account and force a refund.
    require!(
        ctx.accounts.config.refund_timeout_slots
            > ctx.accounts.network_state.config.callback_deadline,
        CoinflipError::InvalidTimeout
    );
```
Also: bump the stale "measured ~42k" CU comment in e2e_settle.rs (real: ~47k),
and rename `settle_fallback_pays_any_winner_owned_account` to
`settle_fallback_pays_ata_when_recorded_account_is_gone`.

- [ ] **Step 1: Replace `instructions/settle_callback.rs`** — ORAO's fixed prefix (client signer, state, network_state, request), then OUR accounts in exactly the order `join_game` declared:

```rust
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use orao_solana_vrf_cb::{
    state::{client::Client, network_state::NetworkState, request::RequestAccount},
    CB_CLIENT_ACCOUNT_SEED, CB_CONFIG_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameSettled,
    instructions::settlement::execute_settlement,
    state::{Config, Game},
};

#[event_cpi]
#[derive(Accounts)]
pub struct SettleCallback<'info> {
    /// Only the ORAO VRF program can produce this PDA's signature — that
    /// signature IS the proof the randomness is genuine.
    #[account(
        signer,
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
        constraint = client.program == crate::ID @ CoinflipError::UnauthorizedVrfClient,
    )]
    pub client: Box<Account<'info, Client>>,
    /// The registered state PDA (ORAO passes it writable).
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [CB_CONFIG_ACCOUNT_SEED],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = network_state.bump,
    )]
    pub network_state: Box<Account<'info, NetworkState>>,
    #[account(
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = request.bump,
    )]
    pub request: Box<Account<'info, RequestAccount>>,
    // ---- our accounts, in join_game's RemainingAccount order ----
    #[account(mut, close = host)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: rent receiver, must be the game's host.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    #[account(mut, address = game.host_token_account @ CoinflipError::MintMismatch)]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = game.joiner_token_account @ CoinflipError::MintMismatch)]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// [SUPERSEDED BY TASK 16: treasury is now a compile-time constant and the
    /// fee account is ATA-pinned — do not copy this comment or the owner
    /// constraint below; see the Task 16 section and the implementing commits.]
    /// LIVE-treasury policy (unified with settle_fallback): the fee RATE is the
    /// player guarantee (snapshotted); the destination is protocol-internal.
    #[account(
        mut,
        constraint = treasury_token_account.owner == config.treasury
            @ CoinflipError::OwnerMismatch,
        constraint = treasury_token_account.mint == game.token_mint
            @ CoinflipError::MintMismatch,
    )]
    pub treasury_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<SettleCallback>) -> Result<()> {
    let randomness = ctx
        .accounts
        .request
        .fulfilled()
        .ok_or(CoinflipError::RandomnessNotFulfilled)?
        .randomness;

    let outcome = execute_settlement(
        &mut ctx.accounts.game,
        &ctx.accounts.escrow,
        &ctx.accounts.mint,
        &ctx.accounts.host_token_account,
        &ctx.accounts.joiner_token_account,
        &ctx.accounts.treasury_token_account,
        &ctx.accounts.token_program,
        &ctx.accounts.host,
        &randomness,
    )?;

    emit_cpi!(GameSettled {
        game: ctx.accounts.game.key(),
        winner: outcome.winner,
        mint: ctx.accounts.game.token_mint,
        outcome: outcome.outcome,
        pot: outcome.pot,
        fee: outcome.fee,
    });
    Ok(())
}
```
(`lib.rs` signature is unchanged from Task 10 — no lib edit needed.)

- [ ] **Step 2: Add negative tests to `tests/e2e_settle.rs`** — nobody without the ORAO client PDA signature can invoke the callback:

```rust
#[test]
fn settle_callback_rejects_unsigned_client() {
    use anchor_lang::{InstructionData, ToAccountMetas};
    use solana_sdk::instruction::Instruction;

    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm, j.orao.client, j.fixture.game.pubkey(), randomness_with_first_byte(2),
    );

    let mut accounts = coinflip::accounts::SettleCallback {
        client: j.orao.client,
        config: config_pda(),
        network_state: j.orao.network_state,
        request: j.request,
        game: j.fixture.game.pubkey(),
        escrow: j.fixture.escrow,
        host: j.fixture.host.pubkey(),
        host_token_account: j.fixture.host_token_account,
        joiner_token_account: j.joiner_token_account,
        treasury_token_account: j.treasury_token_account,
        mint: j.fixture.mint,
        token_program: anchor_spl::token::ID,
        event_authority: event_authority(),
        program: coinflip::ID,
    }
    .to_account_metas(None);
    // We cannot sign as the client PDA — flip the meta to non-signer to even
    // get the tx past sanitization; the program must still reject it.
    for meta in accounts.iter_mut() {
        meta.is_signer = false;
    }
    let ix = Instruction {
        program_id: coinflip::ID,
        accounts,
        data: coinflip::instruction::SettleCallback {}.data(),
    };
    let result = send(&mut svm, &[&payer], &[ix]);
    assert!(result.is_err()); // AccountNotSigner / missing required signature
    // And the pot is untouched:
    assert_eq!(token_balance(&svm, &j.fixture.escrow), 2 * STAKE);
}

#[test]
fn settle_callback_rejects_forged_client_account() {
    use anchor_lang::{InstructionData, ToAccountMetas};
    use solana_sdk::instruction::Instruction;
    use solana_sdk::signature::Keypair;

    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(
        &mut svm, j.orao.client, j.fixture.game.pubkey(), randomness_with_first_byte(2),
    );

    // Mallory signs with a plain keypair placed in the client slot.
    let mallory = Keypair::new();
    svm.airdrop(&mallory.pubkey(), 1_000_000_000).unwrap();
    let accounts = coinflip::accounts::SettleCallback {
        client: mallory.pubkey(),
        config: config_pda(),
        network_state: j.orao.network_state,
        request: j.request,
        game: j.fixture.game.pubkey(),
        escrow: j.fixture.escrow,
        host: j.fixture.host.pubkey(),
        host_token_account: j.fixture.host_token_account,
        joiner_token_account: j.joiner_token_account,
        treasury_token_account: j.treasury_token_account,
        mint: j.fixture.mint,
        token_program: anchor_spl::token::ID,
        event_authority: event_authority(),
        program: coinflip::ID,
    }
    .to_account_metas(None);
    let ix = Instruction {
        program_id: coinflip::ID,
        accounts,
        data: coinflip::instruction::SettleCallback {}.data(),
    };
    let result = send(&mut svm, &[&payer, &mallory], &[ix]);
    assert!(result.is_err()); // seeds constraint fails: not the ORAO client PDA
    assert_eq!(token_balance(&svm, &j.fixture.escrow), 2 * STAKE);
}
```

- [ ] **Step 3: Build + run + commit**

```bash
anchor build && cargo test -p coinflip --test e2e_settle
git add -A && git commit -m "feat: settle_callback with client-signature gate"
```
Expected: PASS (6 tests).

---

### Task 13: refund_timeout

**Files:**
- Create: `programs/coinflip/src/instructions/refund_timeout.rs`
- Modify: `instructions/mod.rs`, `lib.rs`, `tests/common/mod.rs`
- Test: `programs/coinflip/tests/e2e_refund.rs`

- [ ] **Step 1: Write `instructions/refund_timeout.rs`**

```rust
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use orao_solana_vrf_cb::{
    state::{client::Client, request::RequestAccount},
    CB_CLIENT_ACCOUNT_SEED, CB_REQUEST_ACCOUNT_SEED,
};

use crate::{
    constants::{CONFIG_SEED, ESCROW_SEED},
    errors::CoinflipError,
    events::GameRefunded,
    instructions::settlement::require_payout_account,
    state::{Config, Game, GameState},
};

#[event_cpi]
#[derive(Accounts)]
pub struct RefundTimeout<'info> {
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(
        seeds = [CB_CLIENT_ACCOUNT_SEED, crate::ID.as_ref(), config.key().as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = client.bump,
    )]
    pub client: Box<Account<'info, Client>>,
    #[account(
        seeds = [CB_REQUEST_ACCOUNT_SEED, client.key().as_ref(), game.vrf_seed.as_ref()],
        seeds::program = orao_solana_vrf_cb::ID,
        bump = request.bump,
    )]
    pub request: Box<Account<'info, RequestAccount>>,
    #[account(mut, close = host)]
    pub game: Box<Account<'info, Game>>,
    #[account(mut, seeds = [ESCROW_SEED, game.key().as_ref()], bump = game.escrow_bump)]
    pub escrow: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: rent receiver, must be the game's host.
    #[account(mut, address = game.host @ CoinflipError::OwnerMismatch)]
    pub host: AccountInfo<'info>,
    /// Any host-owned account of the game mint (liveness: recorded one may be closed).
    #[account(
        mut,
        constraint = host_token_account.owner == game.host @ CoinflipError::OwnerMismatch,
        constraint = host_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub host_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    /// Any joiner-owned account of the game mint.
    #[account(
        mut,
        constraint = joiner_token_account.owner == game.joiner @ CoinflipError::OwnerMismatch,
        constraint = joiner_token_account.mint == game.token_mint @ CoinflipError::MintMismatch,
    )]
    pub joiner_token_account: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(address = game.token_mint @ CoinflipError::MintMismatch)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub(crate) fn handle(ctx: Context<RefundTimeout>) -> Result<()> {
    ctx.accounts.game.require_state(GameState::AwaitingRandomness)?;
    // Permissionless cranker: refunds may only land on recorded-or-ATA
    // destinations (same rule as settle_fallback; see settlement.rs).
    require_payout_account(
        &ctx.accounts.host_token_account,
        ctx.accounts.game.host_token_account,
        ctx.accounts.game.host,
        ctx.accounts.game.token_mint,
        ctx.accounts.token_program.key(),
    )?;
    require_payout_account(
        &ctx.accounts.joiner_token_account,
        ctx.accounts.game.joiner_token_account,
        ctx.accounts.game.joiner,
        ctx.accounts.game.token_mint,
        ctx.accounts.token_program.key(),
    )?;
    // A fulfilled request must be settled on its outcome, never refunded.
    require!(
        ctx.accounts.request.fulfilled().is_none(),
        CoinflipError::AlreadyFulfilled
    );
    let deadline = ctx
        .accounts
        .game
        .joined_at_slot
        .checked_add(ctx.accounts.config.refund_timeout_slots)
        .ok_or(CoinflipError::NumericalOverflow)?;
    require!(Clock::get()?.slot > deadline, CoinflipError::TimeoutNotReached);

    let game_key = ctx.accounts.game.key();
    let seeds: &[&[&[u8]]] = &[&[
        ESCROW_SEED,
        game_key.as_ref(),
        &[ctx.accounts.game.escrow_bump],
    ]];
    let decimals = ctx.accounts.mint.decimals;

    // Host gets their stake back; the joiner gets the rest (their stake plus
    // any donated dust, so the escrow always drains to zero).
    let host_refund = ctx.accounts.game.amount;
    let joiner_refund = ctx
        .accounts
        .escrow
        .amount
        .checked_sub(host_refund)
        .ok_or(CoinflipError::NumericalOverflow)?;

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.host_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            seeds,
        ),
        host_refund,
        decimals,
    )?;
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.escrow.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.joiner_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            seeds,
        ),
        joiner_refund,
        decimals,
    )?;
    token_interface::close_account(CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        CloseAccount {
            account: ctx.accounts.escrow.to_account_info(),
            destination: ctx.accounts.host.to_account_info(),
            authority: ctx.accounts.escrow.to_account_info(),
        },
        seeds,
    ))?;

    ctx.accounts.game.state = GameState::Refunded.into();
    emit_cpi!(GameRefunded {
        game: game_key,
        host: ctx.accounts.game.host,
        joiner: ctx.accounts.game.joiner,
        mint: ctx.accounts.game.token_mint,
        host_refund,
        joiner_refund,
    });
    Ok(())
}
```

- [ ] **Step 2: Wire up** — `instructions/mod.rs`: `pub mod refund_timeout;` + `pub use refund_timeout::*;`. `lib.rs`:

```rust
    pub fn refund_timeout(ctx: Context<RefundTimeout>) -> Result<()> {
        instructions::refund_timeout::handle(ctx)
    }
```

- [ ] **Step 3: Add builder to `tests/common/mod.rs`**

```rust
pub fn ix_refund_timeout(j: &JoinedGame, payer: Pubkey) -> Instruction {
    Instruction {
        program_id: coinflip::ID,
        accounts: coinflip::accounts::RefundTimeout {
            payer,
            config: config_pda(),
            client: j.orao.client,
            request: j.request,
            game: j.fixture.game.pubkey(),
            escrow: j.fixture.escrow,
            host: j.fixture.host.pubkey(),
            host_token_account: j.fixture.host_token_account,
            joiner_token_account: j.joiner_token_account,
            mint: j.fixture.mint,
            token_program: spl_token::ID,
            event_authority: event_authority(),
            program: coinflip::ID,
        }
        .to_account_metas(None),
        data: coinflip::instruction::RefundTimeout {}.data(),
    }
}
```

- [ ] **Step 4: Write `tests/e2e_refund.rs`**

```rust
mod common;

use common::*;
use solana_sdk::signature::Signer;

const STAKE: u64 = 1_000_000;

#[test]
fn refund_after_timeout_returns_both_stakes() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    let joined_slot = svm.get_sysvar::<solana_sdk::clock::Clock>().slot;
    svm.warp_to_slot(joined_slot + DEFAULT_TIMEOUT_SLOTS + 1);

    send(&mut svm, &[&payer], &[ix_refund_timeout(&j, payer.pubkey())]).unwrap();
    assert_eq!(token_balance(&svm, &j.fixture.host_token_account), STAKE * 10);
    assert_eq!(token_balance(&svm, &j.joiner_token_account), STAKE * 10);
    assert!(svm.get_account(&j.fixture.game.pubkey()).map_or(true, |a| a.lamports == 0));
}

#[test]
fn refund_before_timeout_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    let result = send(&mut svm, &[&payer], &[ix_refund_timeout(&j, payer.pubkey())]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::TimeoutNotReached);
}

#[test]
fn refund_of_fulfilled_request_fails() {
    let (mut svm, payer) = setup();
    let j = setup_joined_game(&mut svm, &payer, STAKE);
    write_fulfilled_request(&mut svm, j.orao.client, j.fixture.game.pubkey(), [1u8; 64]);
    let joined_slot = svm.get_sysvar::<solana_sdk::clock::Clock>().slot;
    svm.warp_to_slot(joined_slot + DEFAULT_TIMEOUT_SLOTS + 1);
    let result = send(&mut svm, &[&payer], &[ix_refund_timeout(&j, payer.pubkey())]);
    assert_coinflip_error(result, coinflip::errors::CoinflipError::AlreadyFulfilled);
}
```

- [ ] **Step 5: Build + run ALL tests + commit**

```bash
anchor build && cargo test -p coinflip
git add -A && git commit -m "feat: refund_timeout"
```
Expected: all unit + e2e suites PASS.

---

### Task 14: Deployment scripts + devnet smoke test

**Files:**
- Create: `scripts/package.json`, `scripts/tsconfig.json`, `scripts/register.ts`, `scripts/smoke.ts`

**Implementation note (post-hoc):** the scope grew during implementation beyond
what's drafted below — `register.ts` gained two more subcommands
(`init-config`, wrapping `initialize_config`; `check-orao`, which fetches
ORAO's `NetworkState` + our `Config` and warns if `refund_timeout_slots`
doesn't clear ORAO's `callback_deadline` by the required margin) — and a
standalone `scripts/smoke.ts` was added for the devnet e2e walkthrough
described in Step 3 (create_game + join_game against the real deployed ORAO
program, then poll for the callback to settle it). Also: the `PROGRAM_ID`
snippet in Step 2 below is broken pseudo-code (a `readFileSync` used as a
truthiness check inside a ternary, which can't compile) — the actual
`register.ts` instead tries `target/deploy/coinflip-keypair.json` then falls
back to `keys/coinflip-keypair.json`, reading only the pubkey out of whichever
exists. All four steps below are done; see `scripts/` for the real code.

- [x] **Step 1: Write `scripts/package.json`**

```json
{
  "name": "coinflip-scripts",
  "private": true,
  "type": "module",
  "scripts": {
    "register": "tsx register.ts register",
    "deposit": "tsx register.ts deposit"
  },
  "dependencies": {
    "@coral-xyz/anchor": "^0.32.1",
    "@orao-network/solana-vrf-cb": "^0.4.0",
    "commander": "^12.0.0"
  },
  "devDependencies": {
    "tsx": "^4.19.0",
    "typescript": "^5.6.0"
  }
}
```

`scripts/tsconfig.json`:
```json
{
  "compilerOptions": {
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "target": "ES2022",
    "strict": true,
    "esModuleInterop": true
  }
}
```

- [x] **Step 2: Write `scripts/register.ts`** — modeled on ORAO's example `cli.ts` (`RegisterBuilder` + a system transfer to the client PDA). The wallet must be the **program's upgrade authority**:

```ts
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { OraoCb, RegisterBuilder, clientAddress } from "@orao-network/solana-vrf-cb";
import { Command } from "commander";
import { readFileSync } from "node:fs";

const PROGRAM_ID = new web3.PublicKey(
  readFileSync("../target/deploy/coinflip-keypair.json") // never printed; only its pubkey is needed
    ? require("@solana/web3.js").Keypair.fromSecretKey(
        new Uint8Array(JSON.parse(readFileSync("../target/deploy/coinflip-keypair.json", "utf8")))
      ).publicKey
    : ""
);

const CONFIG_PDA = web3.PublicKey.findProgramAddressSync(
  [Buffer.from("config")],
  PROGRAM_ID
)[0];
const CONFIG_BUMP = web3.PublicKey.findProgramAddressSync(
  [Buffer.from("config")],
  PROGRAM_ID
)[1];

function provider(cluster: string, keyPath: string): anchor.AnchorProvider {
  const url =
    cluster === "devnet" ? web3.clusterApiUrl("devnet") : web3.clusterApiUrl("mainnet-beta");
  const kp = web3.Keypair.fromSecretKey(
    new Uint8Array(JSON.parse(readFileSync(keyPath, "utf8")))
  );
  return new anchor.AnchorProvider(
    new web3.Connection(url, "confirmed"),
    new anchor.Wallet(kp),
    {}
  );
}

const cli = new Command();
cli.requiredOption("-k, --key <path>", "upgrade-authority keypair path");
cli.option("-c, --cluster <name>", "devnet|mainnet", "devnet");

cli
  .command("register")
  .description("Registers the coinflip program as an ORAO VRF client (state PDA = Config)")
  .action(async (_o, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const vrf = new OraoCb(p);
    const builder = await new RegisterBuilder(vrf, PROGRAM_ID, CONFIG_PDA, [
      Buffer.from("config"),
      Buffer.from([CONFIG_BUMP]),
    ]).build();
    const tx = await builder.rpc();
    console.log("Registered client:", clientAddress(PROGRAM_ID, CONFIG_PDA)[0].toBase58());
    console.log("Tx:", tx);
  });

cli
  .command("deposit")
  .requiredOption("--lamports <n>", "amount to deposit into the client balance")
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const [client] = clientAddress(PROGRAM_ID, CONFIG_PDA);
    const tx = await p.sendAndConfirm(
      new web3.Transaction().add(
        web3.SystemProgram.transfer({
          fromPubkey: p.publicKey,
          toPubkey: client,
          lamports: Number(opts.lamports),
        })
      )
    );
    console.log("Deposited. Tx:", tx);
  });

cli.parseAsync();
```
(If the `RegisterBuilder` constructor signature differs on the published 0.4.x package, mirror the exact call in ORAO's `callback/rust/examples/cpi/cli.ts` — it is the canonical usage.)

- [x] **Step 3: Devnet smoke test (manual, documents the callback happy path the LiteSVM suite can't reach)**

IMPORTANT runbook items: (1) NEVER burn the program upgrade authority without
first running ORAO's `Transfer` to move the client `owner` to a surviving key —
owner-signed `Withdraw` is the only way to recover the Client PDA's accumulating
rent surplus. (2) The Register call must set the ORAO client `owner` to a
team-controlled key — owner-signed `Withdraw` is the only way to recover the
rent surplus that accumulates in the Client PDA (~0.0067 SOL per fulfilled game).

```bash
# one-time
anchor build && anchor deploy --provider.cluster devnet
cd scripts && npm install
npx tsx register.ts -k ~/.config/solana/id.json register
npx tsx register.ts -k ~/.config/solana/id.json deposit --lamports 100000000  # 0.1 SOL
npx tsx register.ts -k ~/.config/solana/id.json init-config   # one-shot; payer must be the upgrade authority
npx tsx register.ts -k ~/.config/solana/id.json check-orao    # confirms callback_deadline + margin < refund_timeout_slots
# create + join a throwaway SPL-mint game with two ephemeral wallets, then watch it settle WITHOUT any settle tx:
npx tsx smoke.ts
solana logs <PROGRAM_ID> -u devnet     # expect the SettleCallback + GameSettled event CPI
```
Expected: after `join_game` confirms, within ~a few slots the ORAO oracle fulfills and the program logs show `settle_callback` executing — the winner's ATA balance changes with no third transaction. `smoke.ts` polls for exactly this (the game account closing) and prints both players' final balances plus an explorer link to the fulfill tx. Record the tx signatures in the README (Task 15).

- [x] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: ORAO register/deposit/init/smoke scripts"
```

---

### Task 15: CI, README, CHANGELOG, final verification

**Files:**
- Create: `.github/workflows/ci.yml`, `README.md`, `CHANGELOG.md`

- [ ] **Step 1: Write `.github/workflows/ci.yml`**

```yaml
name: ci

on:
  push:
    branches: [master, main, "**audit**"]
  pull_request:

env:
  RUST_TOOLCHAIN: 1.93.0
  ANCHOR_VERSION: 0.32.1
  SOLANA_VERSION: 2.3.9

jobs:
  fmt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: ${{ env.RUST_TOOLCHAIN }}
          components: rustfmt
      - run: cargo fmt --check

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          toolchain: ${{ env.RUST_TOOLCHAIN }}
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - name: Install Solana
        run: |
          sh -c "$(curl -sSfL https://release.anza.xyz/v${SOLANA_VERSION}/install)"
          echo "$HOME/.local/share/solana/install/active_release/bin" >> "$GITHUB_PATH"
      - name: Install Anchor
        run: cargo install --git https://github.com/coral-xyz/anchor avm --locked && avm install ${ANCHOR_VERSION} && avm use ${ANCHOR_VERSION}
      - run: anchor build
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

- [ ] **Step 2: Write `README.md`** — cover (Economics notes to include: first joiner for a given (treasury, mint) pays the treasury ATA rent ~0.002 SOL; the joiner pays VRF fee + pending-request rent ~0.0096 SOL per join, never refunded to them: ~0.0019 stays locked in the permanent ORAO request account and ~0.0067 returns to the protocol's ORAO Client PDA on fulfillment (an implicit protocol fee, recoverable via ORAO's owner-signed Withdraw); a host can make their open game unjoinable by closing the recorded host token account — bait-and-burn nuisance, joiners lose only tx fees; a REFUNDED game's request rent is recovered only if ORAO later force-fulfills (then it accrues to the protocol's Client PDA, never back to the joiner); client SDKs must NEVER reuse a game keypair — a resurrected game at the same address re-derives the same vrf_seed per joiner and those joins fail forever. Also include a Limitations note: the e2e
  suite exercises Token-2022 only on create/cancel; joins/settlements are tested
  on classic SPL — a T22 join needs a program-parameterized join builder and
  `get_associated_token_address_with_program_id` for the treasury ATA): what the game is (spec summary + the 5 SOL / 9.9 SOL example), the instruction table from the spec, the ORAO callback flow diagram (create → join(request) → oracle callback → settled; fallback + refund backstops), how to build/test (`anchor build && cargo test`), deployment steps (deploy → `initialize_config` → `scripts register` → `deposit`), and the note that the crank/dealer bot lives in the backend repo. Point to `docs/superpowers/specs/2026-08-18-coinflip-program-design.md` for the full design.

- [ ] **Step 3: Write `CHANGELOG.md`** (Keep a Changelog format)

```markdown
# Changelog

## [Unreleased]

### Added
- Coinflip program v0.1.0: create/cancel/join with ORAO Callback VRF settlement,
  permissionless `settle_fallback` and `refund_timeout` backstops, configurable
  fee (default 1%, cap 10%) paid in the bet token to the treasury ATA.

### Notes for integrators
- Game and escrow accounts close on terminal states; index the events
  (`GameCreated/Joined/Settled/Cancelled/Refunded`, emitted via event CPI).
- Both players' payout token accounts must exist at join time.
- Error codes and event layouts are append-only ABI from here on.
```

- [ ] **Step 4: Final verification (run everything)**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && anchor build && cargo test
```
Expected: all green. Fix anything that isn't before committing.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "chore: CI, README, CHANGELOG"
```

---

## Plan self-review (done at planning time)

- **Spec coverage:** config init/update (T6), create (T8), cancel (T9), join + VRF request + fee reimbursement + treasury ATA (T10), callback settle (T12), fallback settle (T11), refund (T13), events (T5 + emitted in each), fee math + rounding (T3), mint rules (T8), registration/deployment (T14), CI (T15). Crank bot: separate repo per spec — only referenced in README.
- **Known deviation from spec:** spec's `Config.vrf_client` field is NOT stored — the ORAO client PDA is fully determined by seeds `[CB_CLIENT_ACCOUNT_SEED, program_id, config_pda]`, so every instruction validates it by derivation instead (stronger: nothing to keep in sync). The spec should be updated to match.
- **Version pin:** Anchor 0.32.1 (forced by orao-solana-vrf-cb 0.4), NOT the 1.x line the wiki playbook's reference repo uses. The playbook's patterns still apply.
- **Test-strategy honesty:** the positive callback path can't run under LiteSVM (ORAO's fulfill logic is closed-source and oracle-signed); it is covered by the shared settlement core + callback negative tests + the Task 14 devnet smoke test.


---

### Task 16 (post-v0.1 amendment): compile-time treasury constant

Adopted after user review of the treasury model, following DLMM's `fee_owner`
pattern. Replaces `Config.treasury` (admin-rotatable) with a `pub mod treasury`
constant: the real id (`BUs86uMPdNMJ9SiFijb4TABpFduhaEqqESs96pTGadsN`, keypair
in gitignored `keys/treasury-keypair.json`) in non-local builds, and a
committed test key (`programs/coinflip/tests/fixtures/treasury-local.json`)
under a new `local` cargo feature (constants/IDs only — never logic). Config
shrinks by 32 bytes (reserved unchanged; INIT_SPACE 140 → 108); initialize/
update_config drop the treasury arg/rotation arm; join/settle paths validate
the treasury ATA against the constant; rotation-specific tests replaced by a
const-derivation test; e2e `.so` must be built `--features local` (CI + README
+ stale-guard notes updated); scripts read the treasury from the IDL constant.
Full details in the implementing commits and the spec's State section.


---

### Task 17 (post-v0.1 redesign): plain ORAO VRF + crank settlement (callback removed)

User decision after reviewing the callback's carrying costs (frozen account
lists, locally-untestable happy path, ORAO deadline coupling, client
registration ops). Nothing is deployed anywhere, so this is pure code work.

Scope:
- Dependency: swap `orao-solana-vrf-cb` for the plain `orao-solana-vrf` crate
  (same repo/workspace, anchor 0.32; program `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`;
  request PDA seeds `[b"orao-vrf-randomness-request", seed]` — no client in the
  namespace; config PDA `[b"orao-vrf-network-configuration"]`).
- join_game: CPI `request_v2` (joiner = payer, pays fee + request rent directly;
  ORAO treasury from network_state constraint). No callback construction, no
  arbitrary-writable authorization, no Client PDA, no reimbursement machinery.
  Accounts shrink accordingly (host / host_token_account leave the struct —
  they were only ever frozen-list authorization). vrf_seed hashing unchanged.
- settle_callback: DELETED (instruction, tests, shape pins). `settle_fallback`
  renamed `settle` — THE permissionless settlement path, unchanged semantics
  (request account = `RandomnessV2`, fulfilled accessor per the crate).
- refund_timeout: unchanged logic; the request account type changes.
- constants: MIN_REFUND_TIMEOUT_SLOTS 18_000 → 1_500 (~10 min — the old floor
  existed only for the callback-retry deadline); MIN_SETTLE_MARGIN_SLOTS and
  join's deadline-margin invariant REMOVED (no callback deadline exists).
- Harness: dump the plain VRF `.so` (`VRFzZoJ…`, mainnet, provenance README),
  craft its NetworkState, write fulfilled/pending `RandomnessV2` accounts,
  drop the client/registration helpers; the join CPI still runs against the
  real binary; settlement remains fully locally testable (now the ONLY path —
  the whole system is LiteSVM-verifiable, no devnet-only feature).
- scripts: register/deposit commands deleted; check-orao reports the plain
  NetworkState fee; settle-fallback subcommand renamed settle; smoke.ts
  settles via the crank path (poll request fulfillment, send settle, decode
  GameSettled) — the smoke now exercises the REAL end-to-end production flow.
- Docs: spec sections (decisions row, trade-offs, registration section
  removed, instruction table -1 row, flow diagram, randomness safety, timeout
  floor, economics — joiner outlay shrinks to fee + small request rent),
  README (runbook loses register/deposit; crank promoted to the settlement
  operator; UI-flow/latency notes), CHANGELOG.
- Events/errors: append-only rules hold (no deployment yet, but keep the
  discipline); `UnauthorizedVrfClient` becomes dead — retire in docs, keep the
  variant slot.

Followed by Task 18: winner-pays bond (Option A) sized to the new, smaller
joiner outlay.

#### Post-review amendment (2026-08-19)

Quality review of the Task 17 implementation approved the behavior and required
three additions, all applied in the same change (nothing is deployed, so ABI
churn is free):

1. **Client nonce in the VRF seed.** `join_game(nonce: u64, ..)`; the seed
   becomes `sha256("coinflip-vrf-seed", game, joiner, nonce_le)`. Plain VRF's
   request namespace is global, so a front-run request at one nonce's address
   is now recoverable in-protocol: the client retries at `nonce + 1`. The
   residual drops from "permanent block on a (game, joiner) pair" to a
   per-attempt race that costs the attacker ~2.35M lamports each time and the
   joiner ~5k to retry. Clients MUST retry on `Custom(0)`
   (`AccountAlreadyInUse`).
2. **VRF fee cap.** `join_game(.., max_vrf_fee: u64)` rejects an ORAO
   `request_fee` above the joiner's stated ceiling (`VrfFeeTooHigh`, 6016).
   ORAO's fee is live config its authority can raise, and the joiner pays it
   directly, so the ceiling belongs to the joiner rather than to an admin.
3. **`config` dropped from `Settle` and `RefundTimeout`.** It was vestigial
   (the callback-era client PDA derived from it) and unread by both handlers;
   removing it saves an account and ~8k CU per crank transaction. This resolves
   the deferral flagged in the Task 17 implementation report.

Corrected claims (the old wording was false after the dependency swap): a
pre-funded lamport does NOT block a join — ORAO creates the request with
Anchor's `init`, which absorbs it — and the "bait-and-burn" host griefing note
is void, since `join_game` no longer touches host accounts at all (the real
residual is a host front-running a join with `cancel_game`, costing the joiner
a transaction fee).

Measured after the amendment: `join_game` 75_983 CU, `settle` 39_241 CU, 74
tests.
