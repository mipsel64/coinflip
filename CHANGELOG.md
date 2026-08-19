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
    the IDL as `TREASURY`) rather than admin-rotatable config state: both
    settle paths pin the fee account to the treasury's **canonical ATA** by
    derivation, so a frozen callback account list and a later fallback crank
    can never disagree, and a permissionless cranker cannot scatter fees across
    other treasury-owned accounts. Rotating the treasury is a program upgrade.
    The `local` cargo feature swaps it for a committed test keypair (constants
    and IDs only, never logic) — e2e builds use it, deployments must not, and
    a build script warns loudly whenever it is on.
  - `create_game` / `cancel_game`: host opens a game against any accepted
    SPL/Token-2022 mint, escrowing the stake; cancellable before a joiner
    arrives.
  - `join_game`: joiner matches the stake and CPIs an ORAO Callback VRF
    `Request`, reimbursing the VRF fee and pending-request rent so the
    program's ORAO Client PDA balance stays neutral per join.
  - `settle_callback` / `settle_fallback`: ORAO's oracle normally settles the
    game directly via callback (no third transaction); `settle_fallback` is
    a permissionless backstop for a fulfilled-but-uncallbacked request. Both
    share one core settlement function.
  - `refund_timeout`: permissionless backstop returning both stakes if
    randomness is never fulfilled within the timeout.
  - Deny-by-default mint validation (rejects transfer-fee, transfer-hook,
    permanent-delegate, pausable, confidential-transfer-fee, non-transferable,
    and any unrecognized Token-2022 extension; freeze authority is allowed,
    with the risk documented).
  - Events (`GameCreated`, `GameJoined`, `GameSettled`, `GameCancelled`,
    `GameRefunded`) emitted via `emit_cpi!`, since game and escrow accounts
    close on every terminal state.
  - Ops CLI (`scripts/register.ts`, `scripts/smoke.ts`) for one-time ORAO
    client registration/deposit, config init, a live devnet smoke test, and a
    manual `settle_fallback` crank invocation.
  - `scripts/verify-artifact.ts`: proves a build is deployable before (and,
    with `--url`, after) it ships — byte-probes `target/deploy/coinflip.so` or
    the on-chain ProgramData for the treasury constant it actually carries, and
    rejects a `--features local` binary or IDL. Needs no keypair; exits 1 on
    any failure. Shared IDL access lives in `scripts/idl.ts`.

### Notes for integrators

- Game and escrow accounts close on terminal states — the events above are
  the durable history; index them rather than relying on account state.
- Events are emitted via `emit_cpi!` (self-CPI instruction data), not program
  logs. `anchor-ts`'s `addEventListener`/`EventParser` will never fire for
  them — decode `meta.innerInstructions` instead (see `scripts/smoke.ts`'s
  `findGameSettledEvent` for a working reference).
- There is no `config.treasury` field: read the fee destination from the IDL's
  `TREASURY` constant (or let anchor-ts resolve the `treasury` account for
  `join_game`, which the IDL pins by address). Fees only ever go to that key's
  canonical ATA for the bet mint — Token-2022 games must pass `tokenProgram`
  explicitly so the client derives the same address the program does. See the
  README's "Downstream contract".
- Both players' payout token accounts must exist at join time — the
  callback's account list is fixed when the VRF request is made, before the
  winner is known, and the callback itself cannot create accounts.
- Error codes (`errors.rs`) and event layouts (`events.rs`) are append-only
  ABI from this point forward.
