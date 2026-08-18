# Coinflip Program — Design

**Date:** 2026-08-18
**Status:** Approved design, pre-implementation
**References:** wiki [[Solana Program Authoring Playbook]], [[Meteora DLMM Program]] (`~/projects/meteora/DLMM`) as the pattern reference implementation.

## Summary

An on-chain 1v1 coinflip game on Solana. A host creates a game by choosing a token,
an amount, and a side (heads/tails). A second player joins by matching the stake.
The outcome comes from ORAO VRF. The winner receives the pot minus a configurable
protocol fee (default 1% of the pot).

Example: each player bets 5 SOL (as wSOL). Pot = 10 SOL. Fee = 1% = 0.1 SOL to the
treasury. Winner receives 9.9 SOL.

## Decisions taken

| Decision | Choice | Why |
|---|---|---|
| Framework | Anchor, playbook-scaled | Production habits without production bloat; user learning Anchor |
| Randomness | ORAO VRF (CPI) | Provably fair, simple Anchor integration, built for this use case |
| Assets | Any SPL token via `TokenInterface`; native SOL as wSOL (frontend wraps) | One escrow code path covers legacy SPL + plain Token-2022 |
| Game identity | Fresh keypair account; the game's pubkey **is** the game id | Simpler than PDA + nonce; client generates an ephemeral keypair that signs only at creation |
| Account serialization | Plain `#[account]` (Borsh), **not** zero-copy | Deliberate deviation from playbook Phase 1: both account types are small and fixed-size; the rest of the discipline (InitSpace assert, version byte, reserved bytes, u8 enums) is kept |
| Fee rounding | Floor (rounds down, in the winner's favor) | Explicit choice per playbook Phase 2; documented here so it is never re-litigated |

## Architecture

Anchor workspace (`anchor init` layout), single program `coinflip`.

- `rust-toolchain.toml` pins the Rust channel; `Anchor.toml` `[toolchain]` pins Anchor
  and Solana versions; `[profile.release]` sets `overflow-checks = true`, `lto = "fat"`,
  `codegen-units = 1` (playbook Phase 0).
- ORAO VRF via the `orao-solana-vrf` crate (CPI feature). Exact crate/oracle versions
  are pinned during implementation; the ORAO program `.so` is checked in for tests.

## State

### `Config` — singleton PDA, seeds `["config"]`

| Field | Type | Notes |
|---|---|---|
| `version` | `u8` | Layout version, starts at 1 |
| `bump` | `u8` | |
| `admin` | `Pubkey` | Can call `update_config` |
| `treasury` | `Pubkey` | Authority whose ATA receives fees |
| `fee_bps` | `u16` | Default 100 (1%); hard cap `MAX_FEE_BPS = 1000` (10%) |
| `refund_timeout_slots` | `u64` | Slots after join before `refund_timeout` is allowed |
| `_reserved` | `[u8; 64]` | Zeroed tail for future fields |

### `Game` — keypair account (signs at `create_game`, key discarded after)

| Field | Type | Notes |
|---|---|---|
| `version` | `u8` | |
| `state` | `u8` | `GameState`: Open=0, AwaitingRandomness=1, Settled=2, Cancelled=3, Refunded=4 |
| `host_side` | `u8` | `Side`: Heads=0, Tails=1. Joiner implicitly takes the other side |
| `escrow_bump` | `u8` | |
| `host` | `Pubkey` | |
| `joiner` | `Pubkey` | `Pubkey::default()` until joined |
| `token_mint` | `Pubkey` | |
| `amount` | `u64` | Per-player stake, in base units |
| `vrf_force` | `[u8; 32]` | ORAO force (randomness seed), set at join |
| `joined_at_slot` | `u64` | Set at join; drives the refund timeout |
| `_reserved` | `[u8; 64]` | |

Enums stored as `u8`, defined `#[repr(u8)]` with `num_enum::TryFromPrimitive`; every
read converts with `try_from(..).map_err(..)`. Discriminant 0 of each enum is the
correct default meaning (`Open`, `Heads`). `#[derive(InitSpace)]` plus
`const_assert_eq!(T::INIT_SPACE, N)` on both types.

### Escrow — token account PDA, seeds `["escrow", game_pubkey]`

Created at `create_game` with `token::authority = escrow` (the account is its own
authority); the program signs transfers out via `invoke_signed` with the escrow seeds.
Holds both stakes. Closed on every terminal transition.

## Instructions

`lib.rs` contains `declare_id!` and the `#[program]` module only; every body is one
delegating call (playbook Phase 4). One file per instruction.

| # | Instruction | Signer | Behavior |
|---|---|---|---|
| 1 | `initialize_config(fee_bps, refund_timeout_slots)` | deployer | One-time. `fee_bps <= MAX_FEE_BPS`. Admin/treasury = provided keys |
| 2 | `update_config(...)` | `admin` | Rotate admin/treasury, change `fee_bps` (re-checked against cap) and timeout |
| 3 | `create_game(side, amount)` | host + game keypair | `amount > 0`. Validates mint (see Token rules). Inits `Game` + escrow, `transfer_checked` host stake into escrow. State = Open |
| 4 | `cancel_game` | host | Requires state == Open. Refund host stake, close escrow + game (rent to host) |
| 5 | `join_game(force)` | joiner | Requires state == Open, `joiner != host`. Transfer matching stake into escrow. CPI ORAO `Request` with `force` — joiner pays the VRF fee. Store `force`, `joiner`, `joined_at_slot`. State = AwaitingRandomness |
| 6 | `settle` | anyone | Requires state == AwaitingRandomness and the ORAO randomness account for the stored `force` is fulfilled. `outcome = fulfilled_randomness[0] & 1` (0 = Heads, 1 = Tails). Winner = host if outcome == host_side else joiner. `pot = 2 * amount`; `fee = pot * fee_bps / 10_000` (u128 widening, floor); fee → treasury ATA (init-if-needed, payer = settle caller), `pot - fee` → winner ATA. Close escrow + game, rent to host. State transitions to Settled before transfers are made |
| 7 | `refund_timeout` | anyone | Requires state == AwaitingRandomness and `current_slot > joined_at_slot + refund_timeout_slots` and randomness NOT fulfilled. Return each stake to its player, no fee. Close escrow + game, rent to host |

### Game flow

```
create_game ──▶ Open ──cancel_game──▶ Cancelled (host refunded)
                 │
             join_game (ORAO request, joiner pays VRF fee)
                 │
                 ▼
         AwaitingRandomness ──settle (fulfilled)──▶ Settled (winner paid, fee taken)
                 │
                 └──refund_timeout (unfulfilled + timeout)──▶ Refunded (both repaid)
```

## Randomness safety

- The joiner supplies the 32-byte `force`. ORAO's `Request` CPI **creates** the
  randomness account for that force — if it already exists (someone precomputed the
  outcome), account creation fails and the join aborts. A joiner therefore cannot
  submit a force whose randomness is already known.
- `settle` and `refund_timeout` verify the passed randomness account is the ORAO PDA
  derived from the stored `vrf_force`, owned by the ORAO program.
- The host cannot influence the outcome after creation: the force doesn't exist until
  join, and settlement is permissionless so neither player can withhold it.

## Token rules

- `InterfaceAccount` + `Interface<TokenInterface>`; all moves via `transfer_checked`
  with mint decimals.
- Mint validation at `create_game` (deny-by-default per playbook Phase 6):
  - **Reject** mints with `TransferFeeConfig` or `TransferHook` extensions — they
    break the stake/payout math.
  - **Allow** freeze authority (rejecting it would exclude USDC). Documented risk:
    a freezable escrow can strand a game; `refund_timeout` does not help against a
    frozen token account. Accepted for a fun project.
- Fees are collected in the bet token, into the treasury's ATA for that mint.

## Math & errors

- No bare arithmetic in value paths: `checked_*` everywhere; the fee multiply widens
  to `u128`. `overflow-checks = true` as the net, not the plan (playbook Phase 2).
- Single `#[error_code]` enum, append-only, `#[msg]` and `PartialEq` on every variant.
  Initial set: `FeeTooHigh`, `InvalidGameState`, `InvalidSide`, `ZeroAmount`,
  `HostCannotJoin`, `MintMismatch`, `UnsupportedMintExtension`,
  `RandomnessNotFulfilled`, `RandomnessAccountMismatch`, `TimeoutNotReached`,
  `NumericalOverflow`.
- Every Anchor `constraint` carries `@ TypedError`.

## Events

`#[event_cpi]` + `emit_cpi!` (playbook Phase 7), one `events.rs`:
`GameCreated`, `GameJoined`, `GameSettled { game, winner, side, pot, fee }`,
`GameCancelled`, `GameRefunded`. Game + escrow accounts are closed on terminal
states, so events are the durable history for any indexer/frontend.

## Repo layout

```
Anchor.toml  rust-toolchain.toml
programs/coinflip/src/
  lib.rs                # declare_id! + #[program], delegating bodies only
  constants.rs          # seeds, MAX_FEE_BPS, defaults — #[constant] where clients need them
  errors.rs  events.rs  math.rs
  state/{mod,config,game}.rs
  instructions/{mod,initialize_config,update_config,create_game,
                cancel_game,join_game,settle,refund_timeout}.rs
tests/                  # LiteSVM e2e (Rust)
.github/workflows/ci.yml
```

## Testing

- **Unit:** fee math under `proptest` (fee ≤ pot, payout + fee == pot, monotonic in
  fee_bps); enum byte round-trips.
- **E2E (LiteSVM, Rust):** real program `.so` artifacts checked in, including ORAO's.
  Oracle fulfillment is simulated by writing the fulfilled randomness account bytes
  directly (LiteSVM account override). Scenarios:
  - happy path: create → join → settle, balances and fee exact
  - cancel before join; cancel after join must fail
  - join with wrong amount/mint fails; host self-join fails; double-join fails
  - settle before fulfillment fails; settle twice fails
  - refund_timeout before timeout fails; after timeout refunds both exactly
  - fee_bps > cap rejected; non-admin update_config rejected
  - mint with transfer-fee extension rejected at create
- **CI:** GitHub Actions — `cargo fmt --check`, `clippy -D warnings`, unit + LiteSVM
  tests, pinned toolchains.

## Out of scope (v1)

- Frontend/client SDK (separate repo; this repo is the program only).
- Native-SOL lamport escrow (frontend wraps to wSOL).
- Leaderboards / game history accounts (events carry history).
- Multi-player or multi-round games.
