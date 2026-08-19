# coinflip

An on-chain 1v1 coinflip game for Solana, built with Anchor. A host picks a
token, an amount, and a side (heads/tails); a second player joins by matching
the stake. The outcome comes from [ORAO Callback VRF](https://github.com/orao-network/solana-vrf/blob/master/callback/README.md):
the oracle fulfills the randomness *and* invokes the program's settlement
callback in the same flow, so in the normal case the game resolves with no
further transaction from either player. The winner receives the pot minus a
configurable protocol fee (default 1%, snapshotted onto the game at creation
so a later fee change never affects games already in flight).

**Worked example:** each player bets 5 SOL (as wSOL). Pot = 10 SOL. Fee = 1%
of the pot = 0.1 SOL, paid to the treasury. Winner receives 9.9 SOL.

This repository is the on-chain program only. A crank/dealer bot (closes
stuck games) and a frontend live in separate, companion repos — see
[Companion repos](#companion-repos).

For the full design rationale (state layout, threat models, accepted
trade-offs), see
[`docs/superpowers/specs/2026-08-18-coinflip-program-design.md`](docs/superpowers/specs/2026-08-18-coinflip-program-design.md).

## Instructions

| # | Instruction | Signer | Behavior |
|---|---|---|---|
| 1 | `initialize_config(admin, treasury, fee_bps, refund_timeout_slots)` | the program's **upgrade authority** (verified against ProgramData) | One-time. `fee_bps <= MAX_FEE_BPS`; timeout bounded to `[MIN, MAX]_REFUND_TIMEOUT_SLOTS`; admin/treasury must be non-default keys |
| 2 | `update_config(...)` | `admin` | Rotate admin/treasury, change `fee_bps` (re-checked against cap) and timeout (re-bounded). Fee changes affect only games created afterwards (snapshot). Both settle paths always pay the *current* treasury |
| 3 | `create_game(side, amount)` | host + game keypair | `amount > 0`. Validates the mint. Inits `Game` + escrow, transfers the host stake into escrow, records the host's payout token account. State → `Open` |
| 4 | `cancel_game` | host | Requires state `Open`. Refunds the host stake, closes escrow + game (rent to host) |
| 5 | `join_game` | joiner | Requires state `Open`, `joiner != host`. Transfers the matching stake into escrow; ensures the treasury ATA exists; reimburses the ORAO VRF fee + pending-request rent from joiner → the program's ORAO Client PDA; CPIs ORAO's `Request` with a callback targeting `settle_callback`. Records the joiner and `joined_at_slot`. State → `AwaitingRandomness` |
| 6 | `settle_callback` | the ORAO Client PDA (via CPI) | The normal path: ORAO's oracle fulfills randomness and invokes this callback directly. Runs the shared core settlement |
| 7 | `settle_fallback` | anyone | Backstop for a failed/ignored callback. Requires state `AwaitingRandomness` and a fulfilled request. Runs the same core settlement |
| 8 | `refund_timeout` | anyone | Requires state `AwaitingRandomness`, past `refund_timeout_slots`, and randomness **not** fulfilled. Returns both stakes, no fee |

**Core settlement** (shared by `settle_callback`/`settle_fallback`): `outcome = randomness[0] & 1` (0 = Heads, 1 = Tails); winner = host if `outcome == host_side` else joiner; `pot` = the escrow's actual balance (so any donated dust goes to the winner); `fee = pot * fee_bps / 10_000` (floored, rate snapshotted at create); fee → treasury ATA, `pot - fee` → winner. State → `Settled`, escrow + game close (rent to host).

## Lifecycle

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
anchor build --no-idl -- --tools-version v1.56
anchor idl build -o target/idl/coinflip.json -t target/types/coinflip.ts
cargo test -p coinflip
```

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
including the real, checked-in ORAO VRF callback program binary (see
[Fixture provenance](#fixture-provenance)).

78 tests should pass: 18 unit (fee math + enum round-trips) + 9 config + 14
create/cancel + 12 join + 11 refund + 14 settle.

## Deployment runbook

Order matters:

1. `anchor deploy`
2. **Back up the program keypair off-machine before doing anything else.**
   `keys/coinflip-keypair.json` (gitignored) is not just a deploy credential —
   registering with ORAO (step 4) funds the ORAO **Client PDA**, which is
   derived from this exact program id. Losing the keypair after registration
   doesn't just lose upgrade authority, it strands the funded Client PDA.
3. `npx tsx scripts/register.ts -k <upgrade-authority-keypair> init-config`
   — calls `initialize_config`. Must be signed by the program's **upgrade
   authority** (checked against `ProgramData`).
4. `npx tsx scripts/register.ts -k <upgrade-authority-keypair> register`
   — one-time ORAO client registration (client program = this program, state
   PDA = our `Config`). This allocates the ORAO Client PDA and sets its
   `owner` to whatever wallet you pass as `-k` — use a durable,
   team-controlled key, not a throwaway one.
5. `npx tsx scripts/register.ts -k <keypair> deposit --lamports <n>`
   — funds the Client PDA's SOL balance (pays VRF request fees + request
   rent; kept roughly self-funding by joiner reimbursement thereafter).
6. `npx tsx scripts/register.ts -k <keypair> check-orao` — confirms
   `callback_deadline + MIN_SETTLE_MARGIN_SLOTS < refund_timeout_slots`
   before anyone joins a game.
7. `npx tsx scripts/smoke.ts` — creates + joins a throwaway game between two
   ephemeral wallets and watches it settle **without** a third transaction
   (polls for the game account closing, then decodes the `GameSettled` event
   — see [Indexer note](#indexer-note)).

**Never burn the program's upgrade authority** without first running ORAO's
`Transfer` instruction to move the Client PDA's `owner` to a surviving,
team-controlled key. An owner-signed `Withdraw` is the *only* way to recover
the Client PDA's accumulating rent surplus (see
[Economics](#economics--accepted-limitations) below) — burn the upgrade
authority first and that surplus is gone forever.

## Economics & accepted limitations

- **Joiner's VRF cost, never refunded to them:** each `join_game` reimburses
  the ORAO VRF request fee *plus* the pending-request account's rent
  (~0.0096 SOL total) from the joiner to the Client PDA, so the Client PDA's
  balance stays roughly neutral per join regardless of join volume. Of that:
  ~0.0019 SOL stays **permanently locked** in the (immutable, closed-forever)
  ORAO request account, and ~0.0067 SOL returns to the Client PDA on
  fulfillment — an implicit protocol fee, recoverable only via ORAO's
  owner-signed `Withdraw`. If a game ends in `Refunded` instead, that rent is
  recovered only if ORAO later force-fulfills the request (and even then it
  accrues to the Client PDA, never back to the joiner).
- **First joiner per `(treasury, mint)` pays the treasury ATA's rent**
  (~0.002 SOL) — `join_game` creates it `init_if_needed`.
- **Bait-and-burn nuisance:** a host can make their own open game unjoinable
  by closing their recorded token account before anyone joins. Joiners who
  try lose only transaction fees, not stake.
- **Never reuse a game keypair.** Client SDKs must always generate a fresh
  keypair per game. `Game.vrf_seed` is derived from `sha256(game, joiner)`;
  a "resurrected" game reusing an old pubkey re-derives the same seed for any
  joiner who already tried it, and ORAO's request-account creation for that
  seed then fails forever for that (game, joiner) pair.
- **wSOL is in scope.** The escrow can be a native (wSOL) token account;
  a permissionless `SyncNative` after a stray lamport transfer only inflates
  the escrow balance, and settlement pays the winner the escrow's *actual*
  balance — so stray donations benefit the winner, never create a shortfall.
- **Freeze-authority mints (e.g. USDC) are accepted**, with a documented
  frozen dead end: if every candidate winner account is frozen once the
  request is already fulfilled, funds are stuck until a thaw — `refund_timeout`
  is blocked by `AlreadyFulfilled` at that point. Accepted for a fun project.
- **Token-2022 test coverage:** the e2e suite exercises T22 on
  `create_game`/`cancel_game`. Join and settlement e2e coverage is classic
  SPL only — a T22 join needs a program-parameterized join builder and
  `get_associated_token_address_with_program_id` for the treasury ATA. T22
  settlement is exercised only indirectly, through the shared settlement core
  logic (both callback and fallback settle call the same function regardless
  of token program).

## Randomness & trust

- Outcome = `randomness[0] & 1` (parity of ORAO's fulfilled randomness, which
  is the XOR of a ≥2/3 quorum of oracle signatures — uniform for honest
  oracles). Residual: the *last* oracle to respond could in principle grind
  its own contribution to steer the parity; this is inherent to ORAO's model
  and accepted here.
- The VRF seed (`sha256("coinflip-vrf-seed", game, joiner)`) is computed at
  join time, not derivable before then — this prevents an attacker from
  pre-funding the request PDA with a stray lamport to grief-block a game
  (ORAO's account creation for an already-funded address fails).
- Refunds are only possible while the randomness is still secret:
  `refund_timeout` requires the request be **unfulfilled**, so a loser can
  never race a refund after learning the outcome.
- `join_game` enforces, dynamically at every join, that
  `refund_timeout_slots >= callback_deadline + MIN_SETTLE_MARGIN_SLOTS` —
  where `callback_deadline` is read live from ORAO's `NetworkState`. This is
  a hard live dependency (fail-closed): if ORAO's deadline creeps up on the
  configured refund timeout, joins reject until an admin raises
  `refund_timeout_slots` via `update_config`.
- **Operational recommendation:** run a standing monitor that alerts when
  ORAO's `callback_deadline` approaches `refund_timeout_slots -
  MIN_SETTLE_MARGIN_SLOTS`. `scripts/register.ts check-orao` performs this
  check on demand and is a good basis for such a monitor.

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

`programs/coinflip/tests/fixtures/orao_vrf_cb.so` is the real, deployed ORAO
VRF Callback program binary, dumped from mainnet so the LiteSVM test suite
exercises the genuine `request`/`settle_fallback`/`settle_callback` CPI paths
instead of interface-only stubs. See
`programs/coinflip/tests/fixtures/README.md` for the exact dump command,
program id, sha256, and re-dump instructions if ORAO ever upgrades it.

## Companion repos

This repository is the on-chain program only. Two companion pieces live
elsewhere:

- **Crank/dealer bot** — polls for games stuck in `AwaitingRandomness` and
  sends the permissionless `settle_fallback` / `refund_timeout` as needed.
  Funds safety never depends on this bot existing; it only shortens the
  worst case (anyone can send the same instructions from an explorer).
- **Frontend** — wraps native SOL to wSOL for players, builds transactions,
  and consumes the events above for game history/leaderboards.

## Further reading

See
[`docs/superpowers/specs/2026-08-18-coinflip-program-design.md`](docs/superpowers/specs/2026-08-18-coinflip-program-design.md)
for the full design: state layout, mint validation rules, error catalog, and
the reasoning behind every accepted trade-off above.
