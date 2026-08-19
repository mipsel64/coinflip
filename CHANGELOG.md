# Changelog

All notable changes to this project are documented in this file, in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format.

## [Unreleased]

### Added

- Coinflip program v0.1.0: an on-chain 1v1 coinflip game.
  - `initialize_config` / `update_config`: singleton `Config` PDA holding
    admin, a configurable protocol fee (default 1%, hard cap 10%), and a
    bounded `refund_timeout_slots`.
  - Treasury as a compile-time constant (`coinflip::treasury::ID`, exported to
    the IDL as `TREASURY`) rather than admin-rotatable config state: `settle`
    pins the fee account to the treasury's **canonical ATA** by derivation, so
    a permissionless cranker cannot scatter fees across other treasury-owned
    accounts. Rotating the treasury is a program upgrade.
    The `local` cargo feature swaps it for a committed test keypair (constants
    and IDs only, never logic) — e2e builds use it, deployments must not, and
    a build script warns loudly whenever it is on.
  - `create_game` / `cancel_game`: host opens a game against any accepted
    SPL/Token-2022 mint, escrowing the stake, creating the treasury's ATA for
    that mint, and posting the winner-pays bond; cancellable before a joiner
    arrives (which returns rent and bond in full).
  - `join_game(nonce, max_vrf_fee)`: joiner matches the stake and CPIs ORAO
    VRF's `request_v2` as ORAO's own payer — the VRF fee and the request
    account's rent come straight out of the joiner's wallet, so the program
    holds no float and needs no ORAO registration. `nonce` salts the VRF seed
    (retry knob for a front-run request address); `max_vrf_fee` bounds what
    ORAO may charge.
  - `settle`: permissionless settlement once ORAO has fulfilled the request.
    Sent by the crank in practice; anyone can send it. Also pays the losing
    joiner's reimbursement out of the host's bond.
  - `refund_timeout`: permissionless backstop returning both stakes if
    randomness is never fulfilled within the timeout.
  - Deny-by-default mint validation (rejects transfer-fee, transfer-hook,
    permanent-delegate, pausable, confidential-transfer-fee, non-transferable,
    and any unrecognized Token-2022 extension; freeze authority is allowed,
    with the risk documented).
  - Events (`GameCreated`, `GameJoined`, `GameSettled`, `GameCancelled`,
    `GameRefunded`) emitted via `emit_cpi!`, since game and escrow accounts
    close on every terminal state.
  - Ops CLI (`scripts/ops.ts`) for config init, an ORAO health/cost check, and
    a manual `settle`; `scripts/smoke.ts` plays a real devnet game end to end
    (create → join → poll ORAO for fulfillment → `settle` → decode
    `GameSettled`), which is exactly the production crank flow.
  - `scripts/verify-artifact.ts`: proves a build is deployable before (and,
    with `--url`, after) it ships — byte-probes `target/deploy/coinflip.so` or
    the on-chain ProgramData for the treasury constant it actually carries, and
    rejects a `--features local` binary or IDL. Needs no keypair; exits 1 on
    any failure. Shared IDL access lives in `scripts/idl.ts`.

### Changed

- **Randomness moved from ORAO Callback VRF to plain ORAO VRF with crank
  settlement** (nothing was deployed, so this is a pre-release redesign rather
  than a breaking change to a live program):
  - Dependency `orao-solana-vrf-cb` 0.4 → `orao-solana-vrf` 0.7 (program
    `VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU` →
    `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`).
  - `settle_callback` **deleted**; `settle_fallback` renamed `settle` and
    promoted from backstop to the one settlement path. The instruction count
    drops from 8 to 7.
  - `join_game` loses `host`, `host_token_account`, and the ORAO client
    accounts, along with the fee-reimbursement transfer and the
    `refund_timeout_slots >= callback_deadline + MIN_SETTLE_MARGIN_SLOTS`
    invariant. The joiner is now ORAO's payer.
  - `MIN_REFUND_TIMEOUT_SLOTS` 18_000 → 1_500 (~10 min); `MIN_SETTLE_MARGIN_SLOTS`
    removed — both existed only to clear the callback-retry deadline.
  - `UnauthorizedVrfClient` (6009) is retired but keeps its slot; error codes
    stay append-only.
  - No ORAO client registration or deposit: `scripts/register.ts` is now
    `scripts/ops.ts` without its `register`/`deposit` subcommands, and
    `settle-fallback` is now `settle`.
  - Joiner economics improved: they front ~0.0066 SOL at join and ORAO returns
    ~0.00426 SOL of it to them when it fulfills (the freed request rent), where
    the callback design routed that surplus to a program-owned Client PDA.
  - `join_game` gained `nonce` and `max_vrf_fee` arguments (see above), and
    `settle`/`refund_timeout` dropped the `config` account neither of them
    reads — one account and ~8k CU less in every crank transaction.
  - New error `VrfFeeTooHigh` (6016).
- **Winner-pays incidentals: the loser now pays their stake and nothing else**
  (also pre-release, nothing deployed):
  - `create_game` posts a **bond** into the game account —
    `2 * ORAO request_fee + rent(fulfilled request)`, ~0.00284 SOL at today's
    fee — recorded as `Game.bond_lamports`. It reads ORAO's `network_state`
    (read-only) to size it.
  - `join_game` records `Game.joiner_sunk_lamports` (`request_fee +
    rent(fulfilled request)`): what the join costs the joiner that neither
    ORAO's own fulfillment refund nor anything else returns.
  - `settle` reimburses `min(joiner_sunk, bond)` from the game account to the
    joiner's wallet **when the host wins**, and takes the joiner's wallet as a
    new account (`joiner`, pinned to `game.joiner`) to do it. When the joiner
    wins they keep the pot and their own costs, and the whole bond sweeps back
    to the host. `cancel_game` and `refund_timeout` return the bond in full.
  - `GameSettled` gains a trailing `joiner_reimbursed: u64`.
  - **The treasury ATA moved from `join_game` to `create_game`**, payer joiner
    → host: `join_game` drops `treasury`, `treasury_token_account`, and
    `associated_token_program` (three accounts); `create_game` gains those
    three plus `network_state`, and pays for the one-per-mint ATA init.
    Measured compute: `create_game` 70_621, `join_game` 48_376 (was ~76k),
    `settle` 39_889 (both create and settle for a zero-bump-miss mint).
  - `Game` grows two `u64`s (plus `request_bump` below): `INIT_SPACE` 244 →
    260 bytes, `_reserved` 22 → 21.
  - Under-bonded edge (ORAO raises its fee between create and join): the
    reimbursement caps at the bond, no join is rejected, and both figures are
    public on the open game so a frontend can show the guaranteed amount. A
    client that wants the risk gone joins with
    `max_vrf_fee = game.bond_lamports - rent(8 + FULFILLED_SIZE)`, the exact
    ceiling at which `sunk <= bond` holds.
  - `create_game` gained a third argument, `max_bond: u64` — the host's ceiling
    on their own lockup, symmetric with the joiner's `max_vrf_fee`. New error
    `BondTooHigh` (6017).
  - `GameCreated` gains a trailing `bond_lamports: u64`, so an indexer knows
    what was promised without having seen the (now closed) game account.
  - `Game` also gained `request_bump: u8` (after `vrf_seed`): the canonical bump
    of the ORAO request PDA, recorded at join so `settle`/`refund_timeout`
    derive that address with one hash instead of a bump search. `_reserved`
    shrank 22 → 21 to keep `INIT_SPACE` at 260. Settlement compute no longer
    depends on which VRF seed a game drew; what remains is the treasury ATA's
    own bump search, which is fixed per mint (39_889 CU for a zero-miss mint,
    ~1.5k per miss above that), so a given market always pays the same.

### Notes for integrators

- Game and escrow accounts close on terminal states — the events above are
  the durable history; index them rather than relying on account state.
- Events are emitted via `emit_cpi!` (self-CPI instruction data), not program
  logs. `anchor-ts`'s `addEventListener`/`EventParser` will never fire for
  them — decode `meta.innerInstructions` instead (see `scripts/smoke.ts`'s
  `findGameSettledEvent` for a working reference).
- There is no `config.treasury` field: read the fee destination from the IDL's
  `TREASURY` constant (or let anchor-ts resolve the `treasury` account for
  `create_game`, which the IDL pins by address). Fees only ever go to that key's
  canonical ATA for the bet mint — Token-2022 games must pass `tokenProgram`
  explicitly so the client derives the same address the program does. See the
  README's "Downstream contract".
- Settlement is a third transaction, and it is permissionless: the crank sends
  it, but a client that wants the result sooner can read the ORAO request
  account directly — the randomness is public the moment ORAO fulfills, before
  `settle` lands.
- Both players' payout token accounts are recorded at create/join; `settle`
  and `refund_timeout` accept the recorded account **or** the player's
  canonical ATA, so a closed account never strands funds.
- Crank authors: `settle` now needs the joiner's **wallet** (`game.joiner`)
  alongside their token account — it is where the bond reimbursement lands.
  `refund_timeout` is unchanged.
- Deployers: `scripts/smoke.ts` prints a **GO/NO-GO** line — a losing joiner's
  end-to-end lamport net, which must be exactly `-5000` (their join
  transaction fee). It is the only check that empirically confirms ORAO refunds
  the pending→fulfilled rent to the request payer, which is the assumption the
  bond's size rests on and the one link LiteSVM models rather than executes.
  See the README's deployment runbook.
- Error codes (`errors.rs`) and event layouts (`events.rs`) are append-only
  ABI from this point forward.
