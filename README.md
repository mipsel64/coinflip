# coinflip

An on-chain 1v1 coinflip game for Solana, built with Anchor. A host picks a
token, an amount, and a side (heads/tails); a second player joins by matching
the stake. The outcome comes from [ORAO VRF](https://github.com/orao-network/solana-vrf):
joining CPIs a randomness request, ORAO's oracle quorum fulfills it a few
seconds later, and a **permissionless** `settle` pays out. Players sign exactly
two transactions — create and join — and never the settlement. The winner
receives the pot minus a configurable protocol fee (default 1%, snapshotted
onto the game at creation so a later fee change never affects games already in
flight).

**Worked example:** each player bets 5 SOL (as wSOL). Pot = 10 SOL. Fee = 1%
of the pot = 0.1 SOL, paid to the treasury. Winner receives 9.9 SOL.

This repository is the on-chain program only. The crank (which sends every
`settle`) and a frontend live in separate, companion repos — see
[Companion repos](#companion-repos).

For the full design rationale (state layout, threat models, accepted
trade-offs), see
[`docs/superpowers/specs/2026-08-18-coinflip-program-design.md`](docs/superpowers/specs/2026-08-18-coinflip-program-design.md).

## Instructions

| # | Instruction | Signer | Behavior |
|---|---|---|---|
| 1 | `initialize_config(admin, fee_bps, refund_timeout_slots)` | the program's **upgrade authority** (verified against ProgramData) | One-time. `fee_bps <= MAX_FEE_BPS`; timeout bounded to `[MIN, MAX]_REFUND_TIMEOUT_SLOTS`; admin must be a non-default key |
| 2 | `update_config(...)` | `admin` | Rotate admin, change `fee_bps` (re-checked against cap) and timeout (re-bounded). Fee changes affect only games created afterwards (snapshot). The treasury is **not** config state — see [Treasury](#treasury) |
| 3 | `create_game(side, amount)` | host + game keypair | `amount > 0`. Validates the mint. Inits `Game` + escrow, transfers the host stake into escrow, records the host's payout token account. State → `Open` |
| 4 | `cancel_game` | host | Requires state `Open`. Refunds the host stake, closes escrow + game (rent to host) |
| 5 | `join_game` | joiner | Requires state `Open`, `joiner != host`. Transfers the matching stake into escrow; ensures the treasury ATA exists; CPIs ORAO's `request_v2` with the joiner as ORAO's payer (they pay the VRF fee and the request account's rent directly — the program holds no VRF float). Records the joiner, `joined_at_slot`, and the refund-timeout snapshot. State → `AwaitingRandomness` |
| 6 | `settle` | anyone (the crank, in practice) | Requires state `AwaitingRandomness` and a **fulfilled** request. Runs the core settlement |
| 7 | `refund_timeout` | anyone | Requires state `AwaitingRandomness`, past `refund_timeout_slots`, and randomness **not** fulfilled. Returns both stakes, no fee |

**Core settlement**: `outcome = randomness[0] & 1` (0 = Heads, 1 = Tails); winner = host if `outcome == host_side` else joiner; `pot` = the escrow's actual balance (so any donated dust goes to the winner); `fee = pot * fee_bps / 10_000` (floored, rate snapshotted at create); fee → treasury ATA, `pot - fee` → winner. State → `Settled`, escrow + game close (rent to host).

## Lifecycle

```
create_game ──▶ Open ──cancel_game──▶ Cancelled (host refunded)
                 │
             join_game (stake + ORAO request_v2 CPI, joiner pays ORAO)
                 │
                 ▼
         AwaitingRandomness ──oracle quorum fulfills (~seconds)──▶ request Fulfilled
                 │                                                       │
                 │                                    crank sends settle (permissionless)
                 │                                                       ▼
                 │                                                    Settled
                 │
                 └──refund_timeout (still unfulfilled + timeout)──▶ Refunded (both repaid)
```

### What a player and a frontend see

1. **join_game** confirms. The game is `AwaitingRandomness`, and the request
   account (`[b"orao-vrf-randomness-request", game.vrf_seed]` under ORAO's
   program) exists but is pending.
2. **ORAO fulfills**, typically within a couple of seconds. The randomness is
   now public on-chain — a frontend can poll or subscribe to that account and
   reveal the coin flip immediately, without waiting for anything of ours.
3. **The crank sends `settle`** on its next poll, and the `GameSettled` event
   is the authoritative record of the payout. Nobody has to trust the crank:
   `settle` is permissionless, so a stalled crank costs time, not funds, and
   any player can send the identical transaction from an explorer.

## Build & test

Toolchain (pinned in `rust-toolchain.toml` / `Anchor.toml`):

- Rust `1.93.0`
- `anchor-cli` `0.32.1` (via [`avm`](https://www.anchor-lang.com/docs/installation))
- Solana CLI `2.3.9`, **with platform-tools forced to `v1.56`** — the default
  platform-tools that ship with Solana CLI 2.3.9 do not reliably build this
  dependency tree (edition2024 shows up transitively through
  `solana-program → blake3 → digest → crypto-common`). See
  `.github/workflows/ci.yml` for the exact forcing sequence and why it's split
  into two `anchor` invocations.

Make sure Anchor's own install is on `PATH`:

```bash
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
```

Build, then test:

```bash
anchor build --no-idl -- --features local --tools-version v1.56
anchor idl build -o target/idl/coinflip.json -t target/types/coinflip.ts -- --features local
cargo test -p coinflip
```

`--features local` swaps the compile-time treasury constant for the committed
test keypair (`programs/coinflip/tests/fixtures/treasury-local.json`) — see
[Treasury](#treasury). The e2e harness scans the loaded `.so` for that key and
refuses to run against a binary built without the feature. **Deployment builds
must not use it** (see [Deployment runbook](#deployment-runbook)).

The build is split into two `anchor` invocations rather than a single
`anchor build -- --tools-version v1.56`: Anchor forwards everything after
`--` both to the real `cargo build-sbf` (which understands
`--tools-version`) *and* to its internal `cargo test --features idl-build`
invocation used to generate the IDL (which does not understand that flag and
fails outright). `--no-idl` skips that internal step so the on-chain build
can be pinned cleanly; `anchor idl build` regenerates the IDL and TypeScript
types separately, with no cargo args involved. See `.github/workflows/ci.yml`
for the same sequence with more detail in comments.

Order matters: the LiteSVM test harness (`tests/common/mod.rs`) loads
`target/deploy/coinflip.so` directly and hard-fails if that artifact is stale
or missing, so the program must be rebuilt before testing.

**`cargo test` is the entry point, not `anchor test`** — `anchor test` boots
an unneeded local validator; every test here runs against LiteSVM in-process,
including the real, checked-in ORAO VRF program binary (see
[Fixture provenance](#fixture-provenance)).

73 tests should pass: 18 unit (fee math + enum round-trips) + 10 config + 14
create/cancel + 10 join + 11 refund + 10 settle.

## Treasury

The fee destination is a **compile-time constant**
(`coinflip::treasury::ID`, exported to the IDL as the `TREASURY` constant),
not a config field: `BUs86uMPdNMJ9SiFijb4TABpFduhaEqqESs96pTGadsN`. Fees always
land in that key's **canonical ATA** for the bet mint, and `settle` pins that
exact derivation — so nobody can scatter fees across other treasury-owned
accounts.

- **Back up `keys/treasury-keypair.json` off-machine**, alongside the program
  keypair (both are gitignored): it is the only key that can move collected
  fees out of the treasury's ATAs. Losing it does *not* stop fee collection —
  that is the trap. Fees keep accruing into an ATA nobody can spend from;
  recovery means changing the constant and upgrading the program, and whatever
  already sits in the old ATA is unspendable forever.
- Under `--features local` the constant becomes the committed test keypair
  `programs/coinflip/tests/fixtures/treasury-local.json`
  (`9wR75bCR1bo68BygzHkgJ3N735u5TmGsVhzjRrFzNUtJ`). That file is a test fixture
  with no value; it exists so the e2e suite has a stable, non-random fee
  destination to derive ATAs against. `scripts/verify-artifact.ts` exists to
  make sure such a build never ships.

### Rotating the treasury = a program upgrade

There is no admin instruction for it. The cost is small (~0.003 SOL in
transaction fees, plus a refundable ~3.7 SOL buffer float while the upgrade
buffer exists; run `solana program extend` first if the binary outgrew its
allocation), and since nothing is frozen at request time, games already in
flight settle fine against the new ATA — the crank just needs the new
derivation (it reads the `TREASURY` constant out of the IDL, so redeploy the
IDL with the binary). One consequence still matters:

- **Drain the old ATAs first, with the old key.** After the upgrade the old
  treasury is just some wallet — nothing in the program refers to it, and its
  collected fees are only reachable by whoever still holds that keypair.

## Deployment runbook

Deployment builds must be **plain** builds — no `local` feature, or the
deployed program would route fees to the test key:

```bash
anchor build --no-idl -- --tools-version v1.56
anchor idl build -o target/idl/coinflip.json -t target/types/coinflip.ts
```

The IDL matters as much as the binary: `scripts/*.ts` (and any downstream
client) read the treasury out of the IDL's `TREASURY` constant, so an IDL
generated with `--features local` would point them at the test key — and
`anchor deploy` **publishes the IDL on-chain by default**, so a local IDL also
poisons every client that calls `Program.fetchIdl`. CI builds both **with** the
feature (it runs the tests; it never deploys).

Then, order matters:

1. `npx tsx scripts/verify-artifact.ts` — reads `target/deploy/coinflip.so` and
   `target/idl/coinflip.json` and refuses (exit 1) if either carries the test
   treasury. Needs no keypair. Do this before every deploy: `local` and plain
   builds are otherwise indistinguishable.
2. `anchor deploy`
3. `npx tsx scripts/verify-artifact.ts --url <rpc>` — the same probes against
   the bytes actually on-chain (fetched from the program's ProgramData), which
   also catches deploying a stale `.so` from a dev tree.
4. **Back up the program keypair off-machine before doing anything else** —
   and `keys/treasury-keypair.json` with it (see [Treasury](#treasury)).
   `keys/coinflip-keypair.json` (gitignored) is the upgrade credential and the
   program id every PDA derives from.
5. `npx tsx scripts/ops.ts -k <upgrade-authority-keypair> init-config`
   — calls `initialize_config`. Must be signed by the program's **upgrade
   authority** (checked against `ProgramData`).
6. `npx tsx scripts/ops.ts -k <keypair> check-orao` — prints ORAO's live
   request fee and treasury and what a joiner will pay out of their own wallet.
7. `npx tsx scripts/smoke.ts` — creates + joins a throwaway game between two
   ephemeral wallets, polls the ORAO request until it is fulfilled, sends
   `settle` (the crank's transaction), and decodes the `GameSettled` event —
   the full production path end to end. See [Indexer note](#indexer-note).
8. Point the crank at the deployment (separate repo). Until it runs, games
   settle only when someone sends `settle` by hand:
   `npx tsx scripts/ops.ts -k <keypair> settle --game <pubkey>`.

There is **no ORAO registration or deposit step** — plain VRF has no client
concept, the joiner pays ORAO directly at join time, and this program holds no
float anywhere. Burning the upgrade authority costs nothing but the ability to
upgrade.

**Pre-mainnet TODO:** add a tag-triggered CI release workflow (plain build →
`verify-artifact` → upload the `.so` and IDL as release assets) so mainnet
artifacts always come from a clean checkout instead of whatever a developer's
tree happened to contain. Until that exists, steps 1–3 above are the only thing
standing between a `--features local` build and mainnet.

## Economics & accepted limitations

Measured against the LiteSVM harness and mainnet's live ORAO configuration
(`5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK`; 500_000-lamport request fee
when this was written — `scripts/ops.ts check-orao` reprints it live).

- **The joiner pays ORAO directly, and gets most of it back.** At join they
  front the request fee (0.0005 SOL) plus rent for the 749-byte pending request
  account (0.0061 SOL) — 0.0066 SOL, plus the 0.000005 SOL transaction fee.
  When ORAO fulfills, it shrinks that account to 137 bytes and returns the
  freed 0.00426 SOL **to the joiner** (they are the request's `client`). Net
  cost of a settled game: ~0.00235 SOL, of which 0.00184 SOL stays permanently
  locked in the immutable fulfilled request account and 0.0005 SOL is ORAO's
  fee. Nothing accrues to this program, and there is no shared balance for join
  spam to drain.
- **A refunded game is the joiner's worst case:** if ORAO never fulfills, the
  full 0.0061 SOL rent stays locked in the pending request (recovered only if
  ORAO fulfills it later).
- **First joiner of a given mint pays the treasury ATA's rent** (~0.00204 SOL) —
  `join_game` creates it `init_if_needed` so that no cranker ever has to.
- **Reimbursing the joiner out of the pot is not implemented** (planned as a
  winner-pays bond). Today the joiner carries the VRF cost of every game.
- **Bait-and-burn nuisance:** a host can make their own open game unjoinable
  by closing their recorded token account before anyone joins. Joiners who
  try lose only transaction fees, not stake.
- **Never reuse a game keypair.** Client SDKs must always generate a fresh
  keypair per game. `Game.vrf_seed` is derived from `sha256(game, joiner)`, and
  ORAO's request accounts are never closed, so a "resurrected" game reusing an
  old pubkey re-derives an already-taken request address for any joiner who
  already played it — that (game, joiner) pair can never join again.
- **wSOL is in scope.** The escrow can be a native (wSOL) token account;
  a permissionless `SyncNative` after a stray lamport transfer only inflates
  the escrow balance, and settlement pays the winner the escrow's *actual*
  balance — so stray donations benefit the winner, never create a shortfall.
- **Freeze-authority mints (e.g. USDC) are accepted**, with a documented
  frozen dead end: if every candidate winner account is frozen once the
  request is already fulfilled, funds are stuck until a thaw — `refund_timeout`
  is blocked by `AlreadyFulfilled` at that point. Accepted for a fun project.
- **Token-2022 test coverage:** the e2e suite exercises T22 end-to-end on
  `create_game`, `cancel_game`, and `join_game` (see `t22_game_full_join`,
  including the T22-derived treasury ATA). Settlement e2e coverage is classic
  SPL only; the settlement code is token-program-agnostic (`TokenInterface` +
  `transfer_checked` throughout), so this is a coverage gap, not a behavioral
  one.

## Randomness & trust

- Outcome = `randomness[0] & 1` (parity of ORAO's fulfilled randomness, which
  is the XOR of a ≥2/3 quorum of oracle signatures — uniform for honest
  oracles). Residual: the *last* oracle to respond could in principle grind
  its own contribution to steer the parity; this is inherent to ORAO's model
  and accepted here.
- ORAO's request accounts live in a **global** namespace
  (`[b"orao-vrf-randomness-request", seed]`, no client component), so anyone
  may create the request for any seed. The VRF seed
  (`sha256("coinflip-vrf-seed", game, joiner)`) is therefore computed at join
  time and not derivable before then: with a predictable seed, an attacker
  could send ORAO's own `request_v2` first and permanently block that game's
  join. A stray lamport at the address is harmless — ORAO creates the account
  with Anchor's `init`, which absorbs a pre-funded balance. Residual: an
  attacker who guesses the intended joiner's wallet can block that specific
  (game, joiner) pair by paying ORAO's fee and rent themselves; the victim
  joins from another wallet.
- Refunds are only possible while the randomness is still secret:
  `refund_timeout` requires the request be **unfulfilled**, so a loser can
  never race a refund after learning the outcome. This is what makes the delay
  between fulfillment and settlement harmless.
- `settle` verifies the request account by PDA derivation from the stored
  `vrf_seed` *and* by Anchor's typed `Account<RandomnessV2>` (ORAO must own it
  and it must carry ORAO's discriminator), so no hand-rolled account can feed
  the settlement forged randomness.
- **Operational recommendation:** monitor the crank's lag (games sitting in
  `AwaitingRandomness` with a fulfilled request) and ORAO's fulfillment
  latency. `scripts/ops.ts check-orao` reports ORAO's live configuration and
  is a reasonable basis for the latter.

## Indexer note

Events (`GameCreated`, `GameJoined`, `GameSettled`, `GameCancelled`,
`GameRefunded`) are emitted via Anchor's `emit_cpi!` (a self-CPI whose
instruction data *is* the event), **not** the older `sol_log_data`/"Program
data:" log convention. This means `anchor-ts`'s `Program.addEventListener`
(which only parses text logs) will **never** fire for them. Indexers must
instead decode `meta.innerInstructions`: find the inner instruction targeting
this program whose data starts with the 8-byte `EVENT_IX_TAG_LE`, strip that
prefix, and run it through `program.coder.events.decode(...)`. See
`scripts/smoke.ts`'s `findGameSettledEvent` for a complete, working
reference implementation, and the design spec for more detail. Because game
and escrow accounts close on every terminal state, these events are the
durable history for any indexer or frontend.

## Fixture provenance

`programs/coinflip/tests/fixtures/orao_vrf.so` is the real, deployed ORAO VRF
program binary, dumped from mainnet so the LiteSVM test suite exercises the
genuine `request_v2` CPI path instead of an interface-only stub. See
`programs/coinflip/tests/fixtures/README.md` for the exact dump command,
program id, sha256, and re-dump instructions if ORAO ever upgrades it.
Fulfillment itself is written into the request account by the harness — it
needs the oracle quorum's ed25519 signatures, which LiteSVM cannot produce.

## Companion repos

This repository is the on-chain program only. Two companion pieces live
elsewhere:

- **Crank/dealer bot** — *the settlement operator*. Polls for games in
  `AwaitingRandomness` and sends the permissionless `settle` once their request
  is fulfilled, or `refund_timeout` once the window opens. It holds no
  authority and collects nothing for itself (fees go to the treasury's ATA
  either way), so it needs only a fee-funded hot wallet. Funds safety never
  depends on it: anyone can send the same instructions from an explorer; a
  stalled crank costs time, not money.
- **Frontend** — wraps native SOL to wSOL for players, builds transactions,
  and consumes the events above for game history/leaderboards. It can reveal
  the outcome as soon as ORAO fulfills the request (the randomness is public
  on-chain then) rather than waiting for the crank's `settle`.

### Downstream contract (read this before wiring a client)

- **`config.treasury` is gone.** `Config` holds `admin`, `fee_bps`,
  `refund_timeout_slots` only; a client that fetches it looking for a treasury
  gets a decode/undefined error. Read the IDL's `TREASURY` constant instead.
- **Do not pass `treasury` or `networkState` to `join_game`.** The IDL pins
  both by address/PDA, so anchor-ts (>= 0.30) resolves them — and the
  `treasury_token_account` derived from the former — itself; passing them
  explicitly is a type error. You **must** pass `oraoTreasury` (read it from
  ORAO's `NetworkState`) and `request` (`[b"orao-vrf-randomness-request",
  sha256("coinflip-vrf-seed", game, joiner)]` under ORAO's program).
- **`join_game` no longer takes `host` or `hostTokenAccount`.** They were only
  ever there to authorize the callback's frozen account list.
- **`settle_fallback`/`settle_callback` are gone**, replaced by a single
  `settle`. Its `request` account is IDL-resolvable (anchor-ts reads
  `game.vrf_seed` for you); non-anchor clients derive it from the seed above.
- **Token-2022 games must pass `tokenProgram` explicitly.** anchor-ts cannot
  infer a mint's owning program, and the treasury ATA derivation depends on it:
  omit it and the client derives the classic-SPL address, which the program
  rejects. (Same for `settle`'s `treasury_token_account`, which non-anchor
  clients derive themselves.)
- **Non-anchor clients must read the constant, never hardcode it** — the IDL is
  the single source, and `scripts/verify-artifact.ts` is what guarantees the
  published IDL matches the deployed binary.
- **Mismatches fail closed**, never silently: a client built against the wrong
  IDL hits `OwnerMismatch` on `join_game` (the address-pinned treasury account)
  or `InvalidPayoutAccount` on `settle` (the ATA pin). No fee ever goes
  anywhere else.

## Further reading

See
[`docs/superpowers/specs/2026-08-18-coinflip-program-design.md`](docs/superpowers/specs/2026-08-18-coinflip-program-design.md)
for the full design: state layout, mint validation rules, error catalog, and
the reasoning behind every accepted trade-off above.
