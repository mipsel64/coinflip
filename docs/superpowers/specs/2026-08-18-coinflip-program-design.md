# Coinflip Program — Design

**Date:** 2026-08-18 (randomness redesign 2026-08-19)
**Status:** Approved design, pre-implementation
**References:** wiki [[Solana Program Authoring Playbook]], [[Meteora DLMM Program]] (`~/projects/meteora/DLMM`) as the pattern reference implementation; [ORAO VRF docs](https://github.com/orao-network/solana-vrf/blob/master/rust/README.md).

## Summary

An on-chain 1v1 coinflip game on Solana. A host creates a game by choosing a token,
an amount, and a side (heads/tails). A second player joins by matching the stake.
The outcome comes from ORAO VRF: the join CPIs a randomness request, the oracle
quorum fulfills it a few seconds later, and a permissionless `settle` — sent by
the crank, or by anyone — pays out. Players sign exactly two transactions
(create, join); settlement is never theirs to send. The winner receives the pot
minus a configurable protocol fee (default 1% of the pot). **The loser is out
their stake and nothing else**: every join-time incidental lands on the winner,
via a bond the host posts at create (see Economics).

Example: each player bets 5 SOL (as wSOL). Pot = 10 SOL. Fee = 1% = 0.1 SOL to the
treasury. Winner receives 9.9 SOL.

## Decisions taken

| Decision | Choice | Why |
|---|---|---|
| Framework | Anchor, playbook-scaled | Production habits without production bloat; user learning Anchor |
| Randomness | ORAO **plain** VRF (`orao-solana-vrf` 0.7, program `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`), settled by a permissionless crank | Provably fair, with no frozen account lists, no client registration, and no oracle-side deadline coupled to our refund window; the whole system stays locally testable |
| Assets | Any SPL token via `TokenInterface`; native SOL as wSOL (frontend wraps) | One escrow code path covers legacy SPL + plain Token-2022 |
| Game identity | Fresh keypair account; the game's pubkey **is** the game id | Simpler than a PDA + counter; the VRF seed is `sha256("coinflip-vrf-seed", game, joiner, nonce)` stored at join |
| Account serialization | Plain `#[account]` (Borsh), **not** zero-copy | Deliberate deviation from playbook Phase 1: both account types are small and fixed-size; the rest of the discipline (InitSpace assert, version byte, reserved bytes, u8 enums) is kept |
| Fee rounding | Floor (rounds down, in the winner's favor) | Explicit choice per playbook Phase 2; documented here so it is never re-litigated |
| Incidental costs | **Winner pays.** The host posts a lamport bond at create; a losing host reimburses the joiner's ORAO costs out of it at settlement | The loser must be out only their stake. Costs fall due before a winner exists, and the pot is tokens while the costs are lamports, so it is engineered as a reimbursement rather than a deduction |

Accepted trade-offs of plain VRF + crank settlement (vs. the Callback VRF this
design originally used; superseded 2026-08-19 — see the plan's Task 17):

- **A third transaction exists.** Settlement is `settle`, sent by the crank
  (see below) once the request is fulfilled — typically a couple of seconds
  after the join. Neither player signs it, and it is permissionless, so nobody
  can withhold it: any observer (including the winner) can send it from an
  explorer. The crank shortens the wait; it is never load-bearing for safety.
- **What that buys.** No frozen account list (the callback's account set was
  fixed at request time, before the winner was known, and a treasury rotation
  invalidated in-flight games); no ORAO client registration signed by the
  upgrade authority and no program-owned SOL float; no `callback_deadline` as a
  live, fail-closed dependency of the join path; and the happy path becomes
  locally testable — the whole system is LiteSVM-verifiable, with no
  devnet-only step.
- **Requests are globally namespaced.** Plain VRF derives the request PDA from
  `[b"orao-vrf-randomness-request", seed]` with no per-client component, so a
  predictable seed would be a real front-running vector; the seed hash and its
  client-chosen nonce answer that (see Randomness safety).
- **The joiner is ORAO's payer.** They pay the request fee and the request
  account's rent from their own wallet, directly to ORAO — no program-held
  float, no shared balance to drain, and the rent ORAO frees at fulfillment
  comes back to them rather than to the protocol. If they then *lose*, the
  host's bond refunds what ORAO kept, so the outlay is the winner's cost either
  way (see Economics).

## Architecture

Anchor workspace (`anchor init` layout), single program `coinflip`.

- `rust-toolchain.toml` pins the Rust channel; `Anchor.toml` `[toolchain]` pins Anchor
  and Solana versions; `[profile.release]` sets `overflow-checks = true`, `lto = "fat"`,
  `codegen-units = 1` (playbook Phase 0).
- **Anchor is pinned to 0.32.1**, not the 1.x line the playbook's reference repo uses:
  `orao-solana-vrf` 0.7 pins `anchor-lang = "0.32.1"` and its account types appear in
  our `Accounts` structs. Revisit when ORAO ships a 1.x-compatible release.
- ORAO VRF via the `orao-solana-vrf` crate (CPI feature); the ORAO `.so` is checked
  in for tests. There is **no deployment-time registration step**: plain VRF has no
  client concept, so `anchor deploy` + `initialize_config` is the whole bootstrap.

## State

### `Config` — singleton PDA, seeds `["config"]`

| Field | Type | Notes |
|---|---|---|
| `version` | `u8` | Layout version, starts at 1 |
| `bump` | `u8` | |
| `admin` | `Pubkey` | Can call `update_config` |
| `fee_bps` | `u16` | Default 100 (1%); hard cap `MAX_FEE_BPS = 1000` (10%) |
| `refund_timeout_slots` | `u64` | Slots after join before `refund_timeout` is allowed; bounded to [`MIN_REFUND_TIMEOUT_SLOTS`, `MAX_REFUND_TIMEOUT_SLOTS`]. The MIN is 1_500 slots (~10 min): ORAO's quorum answers in seconds and the crank settles as soon as it does, so the floor only has to cover a fulfillment outage — there is no oracle-side deadline to clear |
| `_reserved` | `[u8; 64]` | Zeroed tail for future fields |

The **treasury is a compile-time constant** (DLMM's `fee_owner` pattern): a
`pub mod treasury` exposing a `const ID: Pubkey` whose real id is baked into non-local
builds and swapped for a committed test key under the `local` feature — per the
playbook rule that a feature flag may change **constants and IDs only**, never
logic. Fees flow to the constant treasury's ATA per mint, pinned by that
derivation at settlement. Rotating the treasury = a program upgrade (~0.003 SOL
in fees plus a refundable ~3.7 SOL buffer float; `solana program extend` first
if the binary grew past its allocation), and because no account list is frozen
at request time, games already in flight settle fine against the new ATA.

`Config` is **read-only** everywhere except `initialize_config`/`update_config`:
`join_game` reads it purely to snapshot `refund_timeout_slots` onto the game. It
signs nothing — plain VRF's request is signed by the joiner (the payer), so the
program has no PDA authority in the VRF flow at all.

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
| `vrf_seed` | `[u8; 32]` | `sha256("coinflip-vrf-seed", game, joiner, nonce_le)`, computed and stored at join; the ORAO request PDA derives from it. Stored, so `settle`/`refund_timeout` never need the nonce |
| `refund_timeout_slots` | `u64` | Snapshot of `config.refund_timeout_slots` at join — this game's refund window is fixed the moment the joiner commits, immune to later config changes |
| `bond_lamports` | `u64` | The winner-pays bond the host parked in this account at create, above its rent: `2 * ORAO request_fee + rent(fulfilled request)`. Public before anyone joins, so a frontend can show the reimbursement a joiner is guaranteed. Caps any reimbursement; the remainder sweeps back to the host when the account closes |
| `joiner_sunk_lamports` | `u64` | What the join cost the joiner in lamports and never comes back on its own (`request_fee + rent(fulfilled request)`), recorded at join. 0 until joined |
| `_reserved` | `[u8; 22]` | |

The **VRF seed is `sha256("coinflip-vrf-seed", game_pubkey, joiner_pubkey,
nonce_le)`**, computed at join from the joiner's `nonce` argument and stored in
`Game.vrf_seed`. It is unique per (game, joiner, nonce) and — unlike the game
pubkey alone — unpredictable before a joiner commits. That matters because
ORAO's request PDAs are globally namespaced: anyone may create the request for
any seed, so a predictable seed would let an attacker front-run the join with
ORAO's own `request_v2` and block that game. The nonce is the in-protocol
recovery: a blocked join retries at `nonce + 1`, an address the attacker must
guess and win all over again
(`e2e_join::front_run_request_is_recovered_by_bumping_the_nonce` pins both
halves). A stray lamport at the address is *not* enough to block anything —
plain VRF creates the account with Anchor's `init`, which absorbs a pre-funded
balance. Residual: a **per-attempt** race for an adversary who knows
(game, joiner, nonce) and can outrun the join transaction — never a permanent
block, and asymmetric in cost (≈2.35M lamports per attempt for the attacker,
who burns ORAO's fee plus rent on a request nobody will settle, against ~5k
lamports for the joiner to retry). Accepted. Enums stored as `u8`, defined `#[repr(u8)]` with
`num_enum::TryFromPrimitive`; every read converts with `try_from(..).map_err(..)`.
Discriminant 0 of each enum is the correct default meaning (`Open`, `Heads`).
`#[derive(InitSpace)]` plus `const_assert_eq!(T::INIT_SPACE, N)` on both types.

### Escrow — token account PDA, seeds `["escrow", game_pubkey]`

Created at `create_game` with `token::authority = escrow` (the account is its own
authority); the program signs transfers out via `invoke_signed` with the escrow seeds.
Holds both stakes. Closed on every terminal transition.

## Instructions

`lib.rs` contains `declare_id!` and the `#[program]` module only; every body is one
delegating call (playbook Phase 4). One file per instruction. `settle` and
`refund_timeout` share the payout-account rules in `settlement.rs`.

| # | Instruction | Signer | Behavior |
|---|---|---|---|
| 1 | `initialize_config(admin, fee_bps, refund_timeout_slots)` | the program's **upgrade authority** (verified against ProgramData) | One-time. `fee_bps <= MAX_FEE_BPS`; timeout bounded to [MIN, MAX]_REFUND_TIMEOUT_SLOTS; admin must be a non-default key |
| 2 | `update_config(...)` | `admin` | Rotate admin, change `fee_bps` (re-checked against cap) and timeout (re-bounded). Fee changes affect only games created afterwards (snapshot). The treasury is a compile-time constant — rotating it is a program upgrade, not a config change |
| 3 | `create_game(side, amount)` | host + game keypair | `amount > 0`. Validates mint (see Token rules). Inits `Game` + escrow, `transfer_checked` host stake into escrow, records host token account. Ensures the treasury ATA for this mint exists (`init_if_needed`, **payer = host** — the host chose the mint) and system-transfers the **bond** into the game account, sized from ORAO's live `request_fee` (read-only `network_state`). State = Open |
| 4 | `cancel_game` | host | Requires state == Open. Refund host stake, close escrow + game (rent **and the whole bond** to host) |
| 5 | `join_game(nonce, max_vrf_fee)` | joiner | Requires state == Open, `joiner != host`. Transfer matching stake into escrow; require ORAO's live `request_fee <= max_vrf_fee`. CPI ORAO `request_v2` with seed = the hashed `vrf_seed` (salted by `nonce`), **joiner as ORAO's payer** — they fund the request fee and the request account's rent straight from their wallet, so this program never holds VRF float. Record joiner, joiner token account, `joined_at_slot`, a snapshot of `refund_timeout_slots`, and `joiner_sunk_lamports`. Touches no treasury account at all. State = AwaitingRandomness |
| 6 | `settle` | anyone (the crank, in practice) | Requires state == AwaitingRandomness and that the `RandomnessV2` account for the stored `vrf_seed` is **fulfilled**. Runs core settlement (below), including the joiner reimbursement — so it carries the joiner's **wallet** (`address = game.joiner`) alongside their token account. Takes no `Config`: nothing in settlement reads it |
| 7 | `refund_timeout` | anyone | Requires state == AwaitingRandomness, `current_slot > joined_at_slot + refund_timeout_slots`, and randomness NOT fulfilled. Return each stake to its player, no fee. Close escrow + game, rent **and the whole bond** to host — nobody won, so nobody owes a reimbursement and the joiner's sunk ORAO costs stay sunk |

**Core settlement** (6): verify the request account is the ORAO PDA for
`[b"orao-vrf-randomness-request", game.vrf_seed]`; `outcome = fulfilled_randomness[0] & 1`
(0 = Heads, 1 = Tails); winner = host if outcome == host_side else joiner;
`pot = the escrow's actual balance` (donated dust goes to the winner and can never brick the close); `fee = pot * game.fee_bps / 10_000` (rate snapshotted at create; u128 widening, floor); fee →
treasury ATA, `pot - fee` → the winner's recorded token account or their canonical ATA (see the liveness rule). Set state = Settled
before transfers, close escrow + game, rent to host.

Then the reimbursement leg, in lamports: **if the host won**, move
`min(joiner_sunk_lamports, bond_lamports)` from the game account straight to the
joiner's wallet — a direct debit of a program-owned account, not a CPI, and it
must happen inside the handler because `close = host` sweeps whatever is left
afterwards. If the **joiner** won, nothing moves: they kept the pot and bore
their own costs, and the whole bond goes home with the host. `GameSettled`
reports the amount as `joiner_reimbursed`.

### Game flow

```
create_game ──▶ Open ──cancel_game──▶ Cancelled (host refunded)
                 │
             join_game (stake + ORAO request_v2 CPI, joiner pays ORAO)
                 │
                 ▼
         AwaitingRandomness ──oracle quorum fulfills (~seconds)──▶ request Fulfilled
                 │                                                       │
                 │                                       crank sends settle (permissionless)
                 │                                                       ▼
                 │                                                    Settled
                 │
                 └──refund_timeout (still unfulfilled + timeout)──▶ Refunded (both repaid)
```

## Randomness safety

- Seed = `sha256("coinflip-vrf-seed", game, joiner, nonce)`, unpredictable
  pre-join and re-rollable by the joiner (see State section). ORAO's request
  PDA is `[b"orao-vrf-randomness-request", seed]` under the VRF program — a
  **global** namespace with no client component, so unpredictability is what
  keeps a third party from creating our request first, and the nonce is what
  keeps a successful front-run from being permanent.
- `join_game` takes `max_vrf_fee` and rejects an ORAO `request_fee` above it.
  ORAO's fee is live config that its authority may raise at any time and the
  joiner pays it directly out of their wallet, so the joiner — not this
  program, and not an admin — sets the ceiling. Fails closed with
  `VrfFeeTooHigh` rather than silently overcharging a player.
- `settle` and `refund_timeout` verify the passed request account is the ORAO
  PDA derived from the stored `vrf_seed`, and — via Anchor's typed
  `Account<RandomnessV2>` — that it is owned by the ORAO program and carries
  ORAO's own discriminator. Randomness that the VRF program did not write
  cannot reach the settlement math.
- Neither player can influence or withhold the outcome: the request doesn't
  exist until join, the oracle fulfills autonomously, and `settle` is
  permissionless — a stalling crank delays nothing that the winner (or anyone)
  cannot send themselves.
- Settlement is decoupled from fulfillment in time, which is safe in the one
  direction that matters: once the request is fulfilled, `refund_timeout` is
  blocked (`AlreadyFulfilled`), so a loser who reads the public randomness can
  never race a refund to turn a loss into a push. The reverse — settling a game
  whose randomness is not yet public — is impossible by construction.
  The only informed refund left requires oracle collusion — learning the
  randomness *before* it is fulfilled on-chain and racing a refund through in
  an outage tail — which is dominated by the existing oracle-trust assumption
  below: an adversary with that access can steer the outcome outright, which is
  strictly better for them than forcing a push.
- The outcome bit is `randomness[0] & 1`. ORAO's fulfilled randomness is the XOR of a
  ≥2/3 quorum of oracle ed25519 signatures, so the parity is uniform for honest
  oracles. Residual trust assumption: the *last* oracle to respond sees the others'
  contributions and controls all 64 bytes of its own equally, so it could in
  principle grind its nonce to steer the result — hashing the full 64 bytes would not
  help. This is inherent to ORAO's model, accepted for this project.

## Crank / dealer bot

The crank is the **settlement operator**: every game resolves through a
transaction it sends. It needs no authority of any kind — both instructions it
sends are permissionless — so it is an availability component, not a trust one.

- Lives in the separate coinflip backend repo (**not** in this repo — this repo is
  the program only). Noted here as a required companion component.
- Loop: `getProgramAccounts` filtered on `state == AwaitingRandomness` (games stay
  on-chain until a terminal state closes them), and for each one:
  - request account fulfilled → send `settle`
  - unfulfilled and past `refund_timeout_slots` → send `refund_timeout`
- Pays only transaction fees (~5_000 lamports per settle); it collects the
  protocol fee for the treasury, never for itself, so it needs a funded hot
  wallet and nothing else. Run more than one instance if you like: the loser of
  the race just wastes a transaction (the game account is already closed).
- Funds safety never depends on the crank existing — it only shortens the wait.
  Any player can send the identical `settle` from an explorer, and
  `refund_timeout` remains the backstop if ORAO never answers.
- Frontends should not wait for it to reveal the outcome: the randomness is
  public in the request account the moment ORAO fulfills, so a client can show
  the result (and reconcile with the `GameSettled` event afterwards).

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
    a freezable escrow can strand a game; a frozen payout account fails `settle`
    until a thaw (or until the winner's ATA — if that one is unfrozen — is used
    instead). Accepted for a fun project.
- Both players' payout token accounts are recorded at create/join and must be
  OWNED by the respective player — staking from a delegated third party's
  account is rejected so winnings always land in the winner's own account.
- Liveness rule: `cancel_game` accepts any host-owned account of the game mint
  (the host signs). `settle` and `refund_timeout` are permissionless, so their
  player accounts are tightened to **the recorded account or the player's
  canonical ATA** (owner+mint checked as well) — a closed recorded account can
  never strand funds (the ATA is permissionlessly re-creatable), and a
  third-party cranker cannot route a payout into some other, possibly
  delegated, player-owned account. Accepted residual: a cranker may still
  prefer the player's ATA over a healthy recorded account — both are
  player-chosen destinations, so the blast radius is a delegate the player
  themselves approved on their own ATA.
- Both players' accounts are passed at settlement even though only the winner
  is paid (the caller does not have to decide the outcome) — if the loser
  closed every candidate account, anyone can re-create their ATA to unblock.
- Frozen-mint dead end (accepted USDC-style risk, documented): if the mint has a
  freeze authority and every candidate winner account is frozen while the
  request is already fulfilled, the pot and rents are stuck — `refund_timeout`
  is blocked by `AlreadyFulfilled` and no transfer can succeed until a thaw.
- Fees are collected in the bet token, into the **constant treasury's canonical
  ATA** for that mint (existence ensured at create, so no cranker ever has to pay
  rent for it; `settle` pins the ATA derivation itself, so a permissionless
  cranker cannot scatter fees across other treasury-owned accounts).

## Math & errors

- No bare arithmetic in value paths: `checked_*` everywhere; the fee multiply widens
  to `u128`. `overflow-checks = true` as the net, not the plan (playbook Phase 2).
- Single `#[error_code]` enum, append-only, `#[msg]` and `PartialEq` on every variant.
  Initial set: `FeeTooHigh`, `InvalidGameState`, `InvalidSide`, `ZeroAmount`,
  `HostCannotJoin`, `MintMismatch`, `UnsupportedMintExtension`,
  `RandomnessNotFulfilled`, `AlreadyFulfilled`, `UnauthorizedVrfClient`,
  `TimeoutNotReached`, `NumericalOverflow`, `OwnerMismatch`, `InvalidAuthority`,
  `InvalidTimeout`, `InvalidPayoutAccount`, `VrfFeeTooHigh`.
  `UnauthorizedVrfClient` (6009) is
  **retired** with the callback it guarded — nothing raises it any more, and its
  slot stays reserved because codes are ABI.
- Every Anchor `constraint` carries `@ TypedError`.

## Events

`#[event_cpi]` + `emit_cpi!` (playbook Phase 7), one `events.rs`:
`GameCreated`, `GameJoined`,
`GameSettled { game, winner, mint, outcome, pot, fee, joiner_reimbursed }`
(`joiner_reimbursed` is the lamports paid out of the host's bond to a losing
joiner; 0 when the joiner won),
`GameCancelled { game, host, mint, amount }`,
`GameRefunded { game, host, joiner, mint, host_refund, joiner_refund }` (joiner refund includes any donated dust). Game + escrow accounts are
closed on terminal states, so events are the durable history for any
indexer/frontend — terminal events carry enough to be interpreted standalone.

Indexer note: events are emitted via `emit_cpi!` (self-CPI instruction data),
NOT program logs — anchor-ts's `addEventListener`/`EventParser` only scan log
text and will never fire for them. Decode `meta.innerInstructions`: strip the
8-byte `EVENT_IX_TAG_LE`, then `program.coder.events.decode(...)` (see
`scripts/smoke.ts` `findGameSettledEvent` for a working reference).

## Repo layout

```
Anchor.toml  rust-toolchain.toml
programs/coinflip/src/
  lib.rs                # declare_id! + #[program], delegating bodies only
  constants.rs          # seeds, MAX_FEE_BPS, defaults — #[constant] where clients need them
  errors.rs  events.rs  math.rs
  state/{mod,config,game}.rs
  instructions/{mod,initialize_config,update_config,create_game,cancel_game,
                join_game,settlement,settle,refund_timeout}.rs
scripts/                # ops CLI (config bootstrap, ORAO check, manual settle),
                        # devnet smoke test, deploy-artifact verification
tests/                  # LiteSVM e2e (Rust)
.github/workflows/ci.yml
```

## Testing

- **Unit:** fee math under `proptest` (fee ≤ pot, payout + fee == pot, monotonic in
  fee_bps); enum byte round-trips.
- **E2E (LiteSVM, Rust):** real program `.so` artifacts checked in, including ORAO's
  VRF. `join_game` runs the genuine `request_v2` CPI against that binary;
  fulfillment is written directly into the request account (LiteSVM cannot
  produce the oracle quorum's ed25519 signatures), and every settlement path
  then runs for real. There is no devnet-only code path.
  Scenarios:
  - happy path: create (host's outlay pinned exactly: both rents + treasury ATA
    rent + bond + tx fee) → join (joiner's outlay pinned exactly: ORAO fee +
    request rent + tx fee) → crafted fulfillment → `settle` pays out, balances
    and fee exact
  - the bond, end to end: a losing joiner's wallet delta across `settle` equals
    `joiner_sunk_lamports` exactly and the host sweeps the remainder; a winning
    joiner's delta is 0 and the host recovers the whole bond; an under-bonded
    game (ORAO's fee raised between create and join) reimburses exactly
    `bond_lamports`; cancel and refund both return the full bond to the host
  - `settle` fails when the request is unfulfilled, when the game is already
    settled, and when a foreign game's request is substituted
  - a request front-run for the same seed blocks the join; a stray lamport at
    the request address does not
  - cancel before join; cancel after join must fail
  - join with wrong amount/mint fails; host self-join fails; double-join fails
  - refund_timeout before timeout or when fulfilled fails; after timeout refunds
    both exactly
  - fee_bps > cap rejected; non-admin update_config rejected
  - mint with transfer-fee extension rejected at create
- **CI:** GitHub Actions — `cargo fmt --check`, `clippy -D warnings`, unit + LiteSVM
  tests, pinned toolchains.

## Economics

**The loser pays their stake and nothing else.** Every join-time incidental
ends up on the winner. Numbers below are measured against the LiteSVM harness
and mainnet's live ORAO config (`5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK`,
request fee 500_000 lamports at the time of writing).

**Who fronts what.** The host, at create:

| Item | Lamports | Recovered? |
|---|---|---|
| Game account rent (268 bytes) | 2_756_160 | Yes — returned by `close` on every terminal state |
| Escrow rent (165 bytes) | 2_039_280 | Yes — returned when the escrow closes |
| Treasury ATA rent (first game of a mint only) | 2_039_280 | No |
| **Bond** (`2 * request_fee + 1_844_400`) | 2_844_400 | Yes, unless they win — then it pays the joiner (see below) |
| Transaction fee (2 signatures) | 10_000 | No |

The joiner, at join:

| Item | Lamports | Recovered? |
|---|---|---|
| ORAO request fee | 500_000 | Only via the bond, and only if they lose |
| Pending request rent (749 bytes) | 6_103_920 | 4_259_520 comes back from ORAO itself: it shrinks the account to 137 bytes at fulfillment and returns the freed rent to the request's **client**, which is the joiner |
| Rent left locked in the fulfilled request | 1_844_400 | Only via the bond, and only if they lose |
| Transaction fee | 5_000 | No |

So the joiner fronts **6_608_920 lamports (~0.0066 SOL)** and, after ORAO's own
refund, is 2_344_400 lamports down — exactly the `joiner_sunk_lamports` the
program records at join.

**Who ends up paying.** Net lamport cost of a settled game, incidentals only
(both players' stakes are the pot, and the winner takes it minus the fee):

| Outcome | Host's net cost | Joiner's net cost |
|---|---|---|
| Host wins | tx fees + first-of-mint ATA rent + the joiner's 2_344_400 reimbursement | **0** (made whole, minus their own 5_000 tx fee) |
| Joiner wins | tx fees + first-of-mint ATA rent (bond returns in full) | their own 2_344_400 + tx fee — borne by the winner, as intended |

**Under-bonded games.** The bond is sized from ORAO's fee at *create*; ORAO's
authority may raise it before the join. The reimbursement then caps at
`bond_lamports` rather than eating into the rents the `close` owes the host, and
the joiner absorbs the difference. The bond is a public field on the open game,
so a frontend can show "reimbursement guaranteed up to X" before anyone joins —
alongside `max_vrf_fee`, which is the joiner's own ceiling on the same risk. The
2x multiplier is the slack: a fee that doubles between create and join is still
fully covered. No join is ever rejected for it.

**No winner, no reimbursement.** `refund_timeout` returns both stakes and sweeps
the full bond back to the host; the joiner's ORAO costs stay sunk (and if ORAO
never fulfills, the whole 6_103_920 stays locked in the pending request). Same
for `cancel_game`, where no joiner ever existed.

Compute: `create_game` ~73.5k CU (stake transfer + escrow init + treasury-ATA
init + the bond transfer + event — the ATA init is most of it and is a no-op
after the first game of a mint), `join_game` ~48.3k CU (stake transfer + the
ORAO CPI), `settle` ~41.3k CU (request PDA derivation + two transfers + close +
reimbursement + event).

## Out of scope (v1)

- Frontend/client SDK and the crank/dealer bot (separate backend repo; this repo is
  the program only).
- Native-SOL lamport escrow (frontend wraps to wSOL).
- Leaderboards / game history accounts (events carry history).
- Multi-player or multi-round games.
- Reimbursing the joiner's VRF outlay out of the *pot* itself. The winner-pays
  bond (see Economics) covers it in lamports instead; deducting it from the
  token pot would need a price oracle.
