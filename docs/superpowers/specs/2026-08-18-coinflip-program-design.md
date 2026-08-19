# Coinflip Program — Design

**Date:** 2026-08-18
**Status:** Approved design, pre-implementation
**References:** wiki [[Solana Program Authoring Playbook]], [[Meteora DLMM Program]] (`~/projects/meteora/DLMM`) as the pattern reference implementation; [ORAO Callback VRF docs](https://github.com/orao-network/solana-vrf/blob/master/callback/README.md).

## Summary

An on-chain 1v1 coinflip game on Solana. A host creates a game by choosing a token,
an amount, and a side (heads/tails). A second player joins by matching the stake.
The outcome comes from ORAO Callback VRF: the oracle fulfills the randomness and
invokes our settle callback in the same flow, so in the normal case the game
resolves with no further transaction from either player. The winner receives the
pot minus a configurable protocol fee (default 1% of the pot).

Example: each player bets 5 SOL (as wSOL). Pot = 10 SOL. Fee = 1% = 0.1 SOL to the
treasury. Winner receives 9.9 SOL.

## Decisions taken

| Decision | Choice | Why |
|---|---|---|
| Framework | Anchor, playbook-scaled | Production habits without production bloat; user learning Anchor |
| Randomness | ORAO **Callback** VRF (`orao-solana-vrf-cb`, program `VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU`) | Provably fair; oracle invokes our settle callback on fulfillment — no third transaction in the normal flow |
| Assets | Any SPL token via `TokenInterface`; native SOL as wSOL (frontend wraps) | One escrow code path covers legacy SPL + plain Token-2022 |
| Game identity | Fresh keypair account; the game's pubkey **is** the game id | Simpler than PDA + nonce; the pubkey also serves as the unique VRF seed |
| Account serialization | Plain `#[account]` (Borsh), **not** zero-copy | Deliberate deviation from playbook Phase 1: both account types are small and fixed-size; the rest of the discipline (InitSpace assert, version byte, reserved bytes, u8 enums) is kept |
| Fee rounding | Floor (rounds down, in the winner's favor) | Explicit choice per playbook Phase 2; documented here so it is never re-litigated |

Accepted trade-offs of the callback variant (vs. plain VRF + manual settle):

- One-time registration with ORAO signed by the program's **upgrade authority**, and
  a **Client PDA balance** that pays VRF fees — kept self-funding by having the
  joiner reimburse the fee at join.
- The callback's account list is fixed at request time; the winner is unknown then,
  so **both** players' token accounts are passed and must exist at join.
- If the callback fails repeatedly, ORAO retries with increasing intervals and,
  past its `callback_deadline`, fulfills the randomness **ignoring the callback**
  — so no callback-side failure can permanently strand a game: it always
  degrades into the permissionless `settle_fallback` path. This property is what
  makes the two-path design safe, and is why `MIN_REFUND_TIMEOUT_SLOTS` must
  exceed that deadline.

## Architecture

Anchor workspace (`anchor init` layout), single program `coinflip`.

- `rust-toolchain.toml` pins the Rust channel; `Anchor.toml` `[toolchain]` pins Anchor
  and Solana versions; `[profile.release]` sets `overflow-checks = true`, `lto = "fat"`,
  `codegen-units = 1` (playbook Phase 0).
- **Anchor is pinned to 0.32.1**, not the 1.x line the playbook's reference repo uses:
  `orao-solana-vrf-cb` 0.4 pins `anchor-lang = "0.32.1"` and its account types appear in
  our `Accounts` structs. Revisit when ORAO ships a 1.x-compatible release.
- ORAO Callback VRF via the `orao-solana-vrf-cb` crate (CPI feature); the ORAO `.so`
  is checked in for tests.

### ORAO client registration (deployment step, not a program instruction)

After deploy, the upgrade authority runs a script that sends ORAO's `Register`
instruction: client program = `coinflip`, state PDA = our `Config` PDA (it signs
`Request` CPIs). This allocates the **Client PDA** (owned by the VRF program), which
is then funded with SOL for fees/rent. The Client PDA address is not stored anywhere:
the program derives and validates it from seeds wherever it verifies callbacks or
routes fee reimbursements (see the Config section below).

## State

### `Config` — singleton PDA, seeds `["config"]`

| Field | Type | Notes |
|---|---|---|
| `version` | `u8` | Layout version, starts at 1 |
| `bump` | `u8` | |
| `admin` | `Pubkey` | Can call `update_config` |
| `treasury` | `Pubkey` | Authority whose ATA receives fees |
| `fee_bps` | `u16` | Default 100 (1%); hard cap `MAX_FEE_BPS = 1000` (10%) |
| `refund_timeout_slots` | `u64` | Slots after join before `refund_timeout` is allowed; bounded to [`MIN_REFUND_TIMEOUT_SLOTS`, `MAX_REFUND_TIMEOUT_SLOTS`]. The MIN (18_000 slots ≈ 2h) must exceed ORAO's callback-retry deadline (crate default 9_000): otherwise a player could sabotage their recorded payout account, block the callback, and force a refund before ORAO falls back to fulfilling without it |
| `_reserved` | `[u8; 64]` | Zeroed tail for future fields |

`Config` doubles as the registered VRF **state PDA**: it signs `Request` CPIs and is
passed (writable) into every callback by the VRF program. The ORAO Client PDA is not
stored: it is fully determined by seeds `[CB_CLIENT_ACCOUNT_SEED, program_id, config]`
under the ORAO program, so every instruction validates it by derivation — stronger
than a stored field, with nothing to keep in sync.

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
| `fee_bps` | `u16` | Fee snapshot taken from `Config` at create; settlement uses this, so later admin fee changes never apply to already-created games |
| `host_token_account` | `Pubkey` | Payout target, recorded at create |
| `joiner_token_account` | `Pubkey` | Payout target, recorded at join |
| `joined_at_slot` | `u64` | Set at join; drives the refund timeout |
| `vrf_seed` | `[u8; 32]` | `sha256("coinflip-vrf-seed", game, joiner)`, computed and stored at join; the ORAO request PDA derives from it |
| `_reserved` | `[u8; 30]` | |

The **VRF seed is `sha256("coinflip-vrf-seed", game_pubkey, joiner_pubkey)`**,
computed at join and stored in `Game.vrf_seed`. It is unique per game (a game
joins at most once) and — unlike the game pubkey alone — unpredictable before a
joiner commits, so nobody can grief-block a game by pre-funding its
publicly-derivable request PDA with one lamport (which makes ORAO's account
creation fail forever). Residual: the seed is deterministic in
(game, joiner), so an adversary who knows an intended joiner's wallet can
precompute and pre-fund that pair's address, blocking that wallet from that
game (the victim recovers by joining from another wallet; no attacker profit).
Accepted; a client-chosen nonce folded into the hash would close it fully. Enums stored as `u8`, defined `#[repr(u8)]` with
`num_enum::TryFromPrimitive`; every read converts with `try_from(..).map_err(..)`.
Discriminant 0 of each enum is the correct default meaning (`Open`, `Heads`).
`#[derive(InitSpace)]` plus `const_assert_eq!(T::INIT_SPACE, N)` on both types.

### Escrow — token account PDA, seeds `["escrow", game_pubkey]`

Created at `create_game` with `token::authority = escrow` (the account is its own
authority); the program signs transfers out via `invoke_signed` with the escrow seeds.
Holds both stakes. Closed on every terminal transition.

## Instructions

`lib.rs` contains `declare_id!` and the `#[program]` module only; every body is one
delegating call (playbook Phase 4). One file per instruction. `settle_callback` and
`settle_fallback` share one core settlement function.

| # | Instruction | Signer | Behavior |
|---|---|---|---|
| 1 | `initialize_config(admin, treasury, fee_bps, refund_timeout_slots)` | deployer (first caller — initialize immediately after deploy) | One-time. `fee_bps <= MAX_FEE_BPS`; timeout bounded to [MIN, MAX]_REFUND_TIMEOUT_SLOTS; admin/treasury must be non-default keys |
| 2 | `update_config(...)` | `admin` | Rotate admin/treasury, change `fee_bps` (re-checked against cap) and timeout (re-bounded). Fee changes affect only games created afterwards (snapshot). Treasury rotation: BOTH settle paths pay the CURRENT `config.treasury` (the fee *rate* is the player guarantee and is snapshotted; the *destination* is protocol-internal). Runbook: create the new treasury's token accounts for every active mint BEFORE rotating — in-flight callbacks fail until then, degrade to ORAO's fulfill-without-callback, and settle via fallback |
| 3 | `create_game(side, amount)` | host + game keypair | `amount > 0`. Validates mint (see Token rules). Inits `Game` + escrow, `transfer_checked` host stake into escrow, records host token account. State = Open |
| 4 | `cancel_game` | host | Requires state == Open. Refund host stake, close escrow + game (rent to host) |
| 5 | `join_game` | joiner | Requires state == Open, `joiner != host`. Transfer matching stake into escrow; ensure treasury ATA exists (`init_if_needed`, payer = joiner). Transfer the current VRF fee PLUS the pending request account's rent (sized via ORAO's own `RequestAccount::expected_size`) in lamports joiner → Client PDA, so the shared Client balance is exactly neutral per join and cannot be drained by cheap join spam. CPI ORAO `Request` (seed = game pubkey, `Config` PDA signs, Client PDA pays) with a request-level callback targeting `settle_callback` and carrying: game, escrow, host + joiner token accounts, treasury ATA, mint, token program. Record joiner, joiner token account, `joined_at_slot`. State = AwaitingRandomness |
| 6 | `settle_callback` | ORAO (Client PDA signs via CPI) | Accounts per ORAO's required order: Client PDA (signer, validated by seed derivation under the ORAO program), `Config` (writable), `NetworkState`, fulfilled request account, then our accounts. Runs core settlement (below) |
| 7 | `settle_fallback` | anyone | Backstop for a failed/ignored callback. Requires state == AwaitingRandomness and the request account for seed = game pubkey is **fulfilled**. Runs the same core settlement |
| 8 | `refund_timeout` | anyone | Requires state == AwaitingRandomness, `current_slot > joined_at_slot + refund_timeout_slots`, and randomness NOT fulfilled. Return each stake to its player, no fee. Close escrow + game, rent to host |

**Core settlement** (shared by 6 and 7): verify the request account is the ORAO PDA
for seed = game pubkey under our client; `outcome = fulfilled_randomness[0] & 1`
(0 = Heads, 1 = Tails); winner = host if outcome == host_side else joiner;
`pot = the escrow's actual balance` (donated dust goes to the winner and can never brick the close); `fee = pot * game.fee_bps / 10_000` (rate snapshotted at create; u128 widening, floor); fee →
treasury ATA, `pot - fee` → winner's recorded token account. Set state = Settled
before transfers, close escrow + game, rent to host.

### Game flow

```
create_game ──▶ Open ──cancel_game──▶ Cancelled (host refunded)
                 │
             join_game (stake + VRF fee reimbursement, ORAO Request CPI)
                 │
                 ▼
         AwaitingRandomness ──oracle fulfills──▶ settle_callback ──▶ Settled
                 │                                    (normal path, no extra tx)
                 ├──settle_fallback (fulfilled, callback failed/ignored)──▶ Settled
                 │
                 └──refund_timeout (unfulfilled + timeout)──▶ Refunded (both repaid)
```

## Randomness safety

- Seed = `sha256("coinflip-vrf-seed", game, joiner)`, unique per game and
  unpredictable pre-join (see State section). ORAO's `Request` CPI creates the
  request account for that seed; a pre-existing account makes the join fail.
- `settle_callback` accepts only the ORAO Client PDA as a signer, validated by seed
  derivation (`[CB_CLIENT_ACCOUNT_SEED, program_id, config]` under the ORAO program) —
  only the VRF program can produce that signature, so nobody can invoke the callback
  with forged randomness.
- `settle_fallback` and `refund_timeout` verify the passed request account is the
  ORAO PDA derived from seed = game pubkey, owned by the ORAO program.
- Neither player can influence or withhold the outcome: the request doesn't exist
  until join, the oracle settles autonomously, and both fallbacks are permissionless.
- The outcome bit is `randomness[0] & 1`. ORAO's fulfilled randomness is the XOR of a
  ≥2/3 quorum of oracle ed25519 signatures, so the parity is uniform for honest
  oracles. Residual trust assumption: the *last* oracle to respond sees the others'
  contributions and controls all 64 bytes of its own equally, so it could in
  principle grind its nonce to steer the result — hashing the full 64 bytes would not
  help. This is inherent to ORAO's model, accepted for this project.

## Crank / dealer bot

The callback resolves games in the normal case, but a game can sit in
`AwaitingRandomness` if the callback fails or fulfillment lags. Because both
backstop instructions are permissionless, a small off-chain **crank** closes that
gap without any special authority:

- Lives in the separate coinflip backend repo (**not** in this repo — this repo is
  the program only). Noted here as a required companion component.
- Loop: `getProgramAccounts` filtered on `state == AwaitingRandomness` (games stay
  on-chain until a terminal state closes them), and for each stuck game:
  - request account fulfilled → send `settle_fallback`
  - unfulfilled and past `refund_timeout_slots` → send `refund_timeout`
- Only acts on games idle past a grace period, so it never races the callback.
- Funds safety therefore never depends on the crank existing — it only shortens the
  worst case; any player can send the same instructions from the explorer.

## Token rules

- `InterfaceAccount` + `Interface<TokenInterface>`; all moves via `transfer_checked`
  with mint decimals.
- Mint validation at `create_game` (deny-by-default per playbook Phase 6):
  - **Reject** mints with `TransferFeeConfig` or `TransferHook` extensions — they
    break the stake/payout math — `PermanentDelegate` (could drain the escrow),
    `Pausable` (a pause authority stops every transfer for the mint at once —
    strictly worse than per-account freeze), `ConfidentialTransferFeeConfig`
    (implies transfer fees), and `NonTransferable` (a game that can never pay
    out). Validation is allow-listed with deny-by-default: extensions not
    explicitly known-safe (metadata/group pointers, interest-bearing and scaled
    display, confidential transfer mint, mint close authority, default account
    state) are rejected, so a dependency bump can never silently admit a new
    extension.
  - Native SOL via **wSOL is in scope**: the escrow becomes a native token
    account; permissionless `SyncNative` after stray lamport transfers only
    inflates the escrow balance, which settlement pays to the winner (pot =
    actual escrow balance) — a donation vector, never a shortfall.
  - **Allow** freeze authority (rejecting it would exclude USDC). Documented risk:
    a freezable escrow can strand a game; a frozen payout account also fails the
    callback, leaving `settle_fallback` (with a thawed account) as the recovery
    path. Accepted for a fun project.
- Both players' payout token accounts are recorded at create/join, must exist
  then (the callback cannot create accounts — no rent payer in the oracle's tx),
  and must be OWNED by the respective player — staking from a delegated third
  party's account is rejected so winnings always land in the winner's own
  account.
- Liveness rule: `settle_callback` pays the exact recorded accounts (its account
  list is frozen at request time). `cancel_game` accepts any host-owned account
  of the game mint (the host signs). `settle_fallback` and `refund_timeout` are
  permissionless, so their player accounts are tightened to **the recorded
  account or the player's canonical ATA** (owner+mint checked as well) — a
  closed recorded account can never strand funds (the ATA is permissionlessly
  re-creatable), and a third-party cranker cannot route a payout into some
  other, possibly delegated, player-owned account. Accepted residual: a cranker
  may still prefer the player's ATA over a healthy recorded account — both are
  player-chosen destinations, so the blast radius is a delegate the player
  themselves approved on their own ATA.
- Both players' accounts are required at settlement even though only the winner
  is paid (the account set is fixed before the outcome is known) — if the loser
  closed every candidate account, anyone can re-create their ATA to unblock.
- Frozen-mint dead end (accepted USDC-style risk, documented): if the mint has a
  freeze authority and every candidate winner account is frozen while the
  request is already fulfilled, the pot and rents are stuck — `refund_timeout`
  is blocked by `AlreadyFulfilled` and no transfer can succeed until a thaw.
- Fees are collected in the bet token, into the treasury's ATA for that mint
  (existence ensured at join).

## Math & errors

- No bare arithmetic in value paths: `checked_*` everywhere; the fee multiply widens
  to `u128`. `overflow-checks = true` as the net, not the plan (playbook Phase 2).
- Single `#[error_code]` enum, append-only, `#[msg]` and `PartialEq` on every variant.
  Initial set: `FeeTooHigh`, `InvalidGameState`, `InvalidSide`, `ZeroAmount`,
  `HostCannotJoin`, `MintMismatch`, `UnsupportedMintExtension`,
  `RandomnessNotFulfilled`, `AlreadyFulfilled`, `UnauthorizedVrfClient`,
  `TimeoutNotReached`, `NumericalOverflow`, `OwnerMismatch`, `InvalidAuthority`,
  `InvalidTimeout`.
- Every Anchor `constraint` carries `@ TypedError`.

## Events

`#[event_cpi]` + `emit_cpi!` (playbook Phase 7), one `events.rs`:
`GameCreated`, `GameJoined`, `GameSettled { game, winner, mint, outcome, pot, fee }`,
`GameCancelled { game, host, mint, amount }`,
`GameRefunded { game, host, joiner, mint, amount }`. Game + escrow accounts are
closed on terminal states, so events are the durable history for any
indexer/frontend — terminal events carry enough to be interpreted standalone.

## Repo layout

```
Anchor.toml  rust-toolchain.toml
programs/coinflip/src/
  lib.rs                # declare_id! + #[program], delegating bodies only
  constants.rs          # seeds, MAX_FEE_BPS, defaults — #[constant] where clients need them
  errors.rs  events.rs  math.rs
  state/{mod,config,game}.rs
  instructions/{mod,initialize_config,update_config,create_game,cancel_game,
                join_game,settle_callback,settle_fallback,refund_timeout}.rs
scripts/                # one-time ORAO Register + Client PDA funding
tests/                  # LiteSVM e2e (Rust)
.github/workflows/ci.yml
```

## Testing

- **Unit:** fee math under `proptest` (fee ≤ pot, payout + fee == pot, monotonic in
  fee_bps); enum byte round-trips.
- **E2E (LiteSVM, Rust):** real program `.so` artifacts checked in, including ORAO's
  callback VRF. The oracle is simulated by overriding ORAO's `NetworkState` account
  to list a test keypair as an authorized oracle, then sending a real `Fulfill` —
  which exercises the genuine fulfill → callback CPI path. If that proves brittle,
  the fallback is overriding the request account to fulfilled and testing
  `settle_fallback`; both paths share the core settlement, so coverage holds.
  Scenarios:
  - happy path: create → join (fee reimbursed to Client PDA) → oracle fulfill →
    callback settles, balances and fee exact
  - `settle_fallback` on a fulfilled request settles identically; fails when
    unfulfilled or already settled
  - callback with a wrong/unregistered client signer rejected
  - cancel before join; cancel after join must fail
  - join with wrong amount/mint fails; host self-join fails; double-join fails
  - refund_timeout before timeout or when fulfilled fails; after timeout refunds
    both exactly
  - fee_bps > cap rejected; non-admin update_config rejected
  - mint with transfer-fee extension rejected at create
- **CI:** GitHub Actions — `cargo fmt --check`, `clippy -D warnings`, unit + LiteSVM
  tests, pinned toolchains.

## Out of scope (v1)

- Frontend/client SDK and the crank/dealer bot (separate backend repo; this repo is
  the program only).
- Native-SOL lamport escrow (frontend wraps to wSOL).
- Leaderboards / game history accounts (events carry history).
- Multi-player or multi-round games.
- Automated Client PDA balance monitoring (joiner reimbursement keeps it roughly
  neutral; admin tops up if drift occurs).
