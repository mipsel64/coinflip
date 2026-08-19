// Devnet smoke test for the FULL production flow: create_game + join_game
// against a REAL deployed ORAO VRF, poll the request account until the oracle
// quorum fulfills it, then send `settle` — which is exactly what the crank
// does. LiteSVM (the Rust test suite) fabricates the fulfillment, so this is
// the only place a genuine oracle response drives a settlement.
//
// Prerequisites (see ops.ts): the program is deployed and initialize_config
// has run. There is nothing to register with ORAO and no program-owned float
// to top up — the joiner pays ORAO directly. The provider wallet just needs
// enough SOL to fund two ephemeral players and pay rent/fees.
//
// Event decoding: the program emits events via `emit_cpi!` (a self-CPI whose
// instruction data IS the event), not the older `sol_log_data`/"Program
// data:" log convention — so `Program.addEventListener` (which only parses
// text logs) can never see it. Instead we fetch the settle transaction we
// just sent and pick the event out of its `innerInstructions`, exactly like
// the Rust test suite's `find_cpi_event` helper (tests/common/mod.rs) does.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { Orao, randomnessAccountAddress } from "@orao-network/solana-vrf";
import {
  createMint,
  getAssociatedTokenAddressSync,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Coinflip } from "../target/types/coinflip.js";
import { IDL_ADDRESS, IDL_RAW, idlConstantPubkey } from "./idl.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * `8 + RandomnessV2::FULFILLED_SIZE` — what ORAO shrinks a request account to
 * when it fulfills. Its rent is the part of the joiner's outlay that only the
 * host's bond can return, so it sets both the bond's size and the joiner's fee
 * ceiling (see `maxVrfFee` below).
 */
const FULFILLED_REQUEST_LEN = 137;
const POLL_INTERVAL_MS = 2_000;
const POLL_TIMEOUT_MS = 3 * 60_000;
const STAKE_AMOUNT = new anchor.BN(1_000_000); // 0.001 token (9 decimals)
const PLAYER_FUNDING_LAMPORTS = 0.05 * web3.LAMPORTS_PER_SOL;
const KEYS_PATH = join(__dirname, ".smoke-keys.json");

// anchor_lang::event::EVENT_IX_TAG_LE — the fixed 8-byte tag every
// `emit_cpi!`'d self-CPI instruction's data is prefixed with.
const EVENT_IX_TAG_LE = Buffer.from([0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d]);

function loadKeypair(path: string): web3.Keypair {
  const raw = JSON.parse(readFileSync(path, "utf8"));
  return web3.Keypair.fromSecretKey(new Uint8Array(raw));
}

/**
 * Persists ephemeral keypairs to a gitignored scratch file so a mid-run crash
 * never strands funds. Never overwrites a leftover file from a previous run —
 * that would be exactly the moment its keys are still needed for recovery —
 * so an existing file is renamed aside (with a timestamp) first.
 */
function saveKeys(path: string, keys: Record<string, web3.Keypair>): void {
  if (existsSync(path)) {
    const backupPath = path.replace(/\.json$/, `.${Date.now()}.json`);
    renameSync(path, backupPath);
    console.log(`Existing ${path} found — moved it to ${backupPath} first.`);
  }
  const data: Record<string, number[]> = {};
  for (const [name, kp] of Object.entries(keys)) {
    data[name] = Array.from(kp.secretKey);
  }
  writeFileSync(path, JSON.stringify(data, null, 2));
}

const PROGRAM_KEYPAIR_PATH = [
  join(__dirname, "../target/deploy/coinflip-keypair.json"),
  join(__dirname, "../keys/coinflip-keypair.json"),
].find(existsSync);
if (!PROGRAM_KEYPAIR_PATH) {
  throw new Error(
    "coinflip program keypair not found — expected target/deploy/coinflip-keypair.json " +
      "or keys/coinflip-keypair.json"
  );
}
const PROGRAM_ID = loadKeypair(PROGRAM_KEYPAIR_PATH).publicKey;

// Compile-time fee destination: an IDL built with `--features local` carries
// the test key, so the IDL must come from the same build as the deployment
// (scripts/verify-artifact.ts checks exactly that).
const TREASURY = idlConstantPubkey("TREASURY");

if (!IDL_ADDRESS.equals(PROGRAM_ID)) {
  throw new Error(
    `program id mismatch: target/idl/coinflip.json's address (${IDL_ADDRESS.toBase58()}) ` +
      `does not match the keypair's pubkey (${PROGRAM_ID.toBase58()}) — rebuild the IDL ` +
      "(`anchor build`) after redeploying under a new program id"
  );
}

const [CONFIG_PDA] = web3.PublicKey.findProgramAddressSync([Buffer.from("config")], PROGRAM_ID);

/** Mirrors join_game's own derivation: sha256("coinflip-vrf-seed", game, joiner, nonce_le). */
function vrfSeedFor(game: web3.PublicKey, joiner: web3.PublicKey, nonce: anchor.BN): Buffer {
  return createHash("sha256")
    .update(Buffer.from("coinflip-vrf-seed"))
    .update(game.toBuffer())
    .update(joiner.toBuffer())
    .update(nonce.toArrayLike(Buffer, "le", 8))
    .digest();
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function loadProvider(): anchor.AnchorProvider {
  const walletPath = process.env.ANCHOR_WALLET ?? join(homedir(), ".config/solana/id.json");
  const url = process.env.ANCHOR_PROVIDER_URL ?? web3.clusterApiUrl("devnet");

  // This script plays a real game with real funds — refuse anything that
  // doesn't look like devnet unless the operator explicitly opts in.
  if (!url.includes("devnet")) {
    if (process.env.SMOKE_ALLOW_NON_DEVNET !== "1") {
      throw new Error(
        `refusing to run against a non-devnet URL (${url}) — this script creates games and ` +
          "moves real funds. Set SMOKE_ALLOW_NON_DEVNET=1 to override if this is intentional."
      );
    }
    console.warn(
      `\n*** WARNING: SMOKE_ALLOW_NON_DEVNET=1 is set — running against a NON-DEVNET ` +
        `cluster (${url}) with REAL funds. ***\n`
    );
  }

  const wallet = new anchor.Wallet(loadKeypair(walletPath));
  // First output, before anything else happens.
  console.log("Cluster:", url);
  console.log("Wallet:", wallet.publicKey.toBase58());
  return new anchor.AnchorProvider(new web3.Connection(url, "confirmed"), wallet, {
    commitment: "confirmed",
  });
}

interface GameSettledEvent {
  game: web3.PublicKey;
  winner: web3.PublicKey;
  mint: web3.PublicKey;
  outcome: number;
  pot: anchor.BN;
  fee: anchor.BN;
  joinerReimbursed: anchor.BN;
}

/** Finds and decodes the GameSettled event emitted by transaction `signature`. */
async function findGameSettledEvent(
  connection: web3.Connection,
  program: anchor.Program<Coinflip>,
  signature: string
): Promise<GameSettledEvent | null> {
  const tx = await connection.getTransaction(signature, {
    maxSupportedTransactionVersion: 0,
  });
  if (!tx?.meta?.innerInstructions) return null;

  const accountKeys = tx.transaction.message.getAccountKeys({
    accountKeysFromLookups: tx.meta.loadedAddresses,
  });
  for (const inner of tx.meta.innerInstructions) {
    for (const ix of inner.instructions) {
      const programId = accountKeys.get(ix.programIdIndex);
      if (!programId?.equals(PROGRAM_ID)) continue;
      const raw = anchor.utils.bytes.bs58.decode(ix.data);
      if (!raw.subarray(0, 8).equals(EVENT_IX_TAG_LE)) continue;
      const decoded = program.coder.events.decode(
        anchor.utils.bytes.base64.encode(raw.subarray(8))
      );
      // `Program` camelCases the IDL internally before building the events
      // coder (verified: `program.idl.events[].name` is "gameSettled", not
      // the "GameSettled" the raw IDL/Rust struct uses), so the decoded
      // event's name comes back lowercase-first too.
      if (decoded?.name === "gameSettled") {
        return decoded.data as GameSettledEvent;
      }
    }
  }
  return null;
}

async function main() {
  const provider = loadProvider();
  const connection = provider.connection;
  const walletPayer = provider.wallet.payer;
  if (!walletPayer) {
    throw new Error("provider wallet has no local payer keypair");
  }

  const program = new anchor.Program<Coinflip>(IDL_RAW as unknown as Coinflip, provider);
  const vrf = new Orao(provider);

  console.log("Program:", PROGRAM_ID.toBase58());

  const config = await program.account.config.fetch(CONFIG_PDA);
  console.log("Treasury:", TREASURY.toBase58());
  console.log("Fee bps:", config.feeBps);

  // ---- ephemeral keys, persisted BEFORE any funding happens ----
  const host = web3.Keypair.generate();
  const joiner = web3.Keypair.generate();
  const game = web3.Keypair.generate();
  saveKeys(KEYS_PATH, { host, joiner, game });
  console.log(
    `\nSaved ephemeral keys to ${KEYS_PATH} — if this run crashes partway, recover any ` +
      "stranded funds from these keys before discarding them."
  );
  console.log("Host:", host.publicKey.toBase58());
  console.log("Joiner:", joiner.publicKey.toBase58());
  console.log("Game:", game.publicKey.toBase58());

  // ---- throwaway mint + two funded players ----
  console.log("\nCreating throwaway mint...");
  const mint = await createMint(connection, walletPayer, provider.wallet.publicKey, null, 9);

  for (const player of [host, joiner]) {
    const tx = await provider.sendAndConfirm(
      new web3.Transaction().add(
        web3.SystemProgram.transfer({
          fromPubkey: provider.wallet.publicKey,
          toPubkey: player.publicKey,
          lamports: PLAYER_FUNDING_LAMPORTS,
        })
      )
    );
    console.log(`Funded ${player.publicKey.toBase58()} with 0.05 SOL:`, tx);
  }

  const hostTokenAccount = await getOrCreateAssociatedTokenAccount(
    connection,
    walletPayer,
    mint,
    host.publicKey
  );
  const joinerTokenAccount = await getOrCreateAssociatedTokenAccount(
    connection,
    walletPayer,
    mint,
    joiner.publicKey
  );
  const stakeUnits = BigInt(STAKE_AMOUNT.muln(10).toString());
  await mintTo(connection, walletPayer, mint, hostTokenAccount.address, walletPayer, stakeUnits);
  await mintTo(connection, walletPayer, mint, joinerTokenAccount.address, walletPayer, stakeUnits);
  console.log("Minted stake tokens to both players.");

  // ---- create_game ----
  // The host pays for everything the game needs before it is joinable: both
  // rents, the treasury ATA for this mint (once per mint), and the winner-pays
  // BOND — lamports parked in the game account so that a losing host can
  // reimburse the joiner's ORAO costs. Unspent bond comes back to the host when
  // the game account closes (cancel, refund, or a host win).
  //
  // `treasury`, `treasuryTokenAccount` and `networkState` are not passed: the
  // IDL pins them (to the program's constant, that constant's ATA, and ORAO's
  // config PDA), so anchor-ts resolves them itself.
  console.log();
  // The host bounds their own lockup, the mirror of the joiner's max_vrf_fee:
  // ORAO's authority can raise the fee between this read and the transaction
  // landing, and the bond is sized from it. 2x the bond we expect tolerates a
  // fee doubling; anything beyond that, the host would rather not create.
  const [preCreateNetworkState, fulfilledRentLamports] = await Promise.all([
    vrf.getNetworkState(),
    connection.getMinimumBalanceForRentExemption(FULFILLED_REQUEST_LEN),
  ]);
  const expectedBond = preCreateNetworkState.config.requestFee
    .muln(2)
    .addn(fulfilledRentLamports);
  const maxBond = expectedBond.muln(2);
  const hostLamportsBefore = await connection.getBalance(host.publicKey);
  const createTx = await program.methods
    .createGame(0, STAKE_AMOUNT, maxBond) // side = Heads
    .accounts({
      host: host.publicKey,
      game: game.publicKey,
      mint,
      hostTokenAccount: hostTokenAccount.address,
      tokenProgram: TOKEN_PROGRAM_ID,
      program: PROGRAM_ID,
    })
    .signers([host, game])
    .rpc();
  console.log("create_game tx:", createTx);
  const bondLamports = (await program.account.game.fetch(game.publicKey)).bondLamports;
  console.log("Host bond posted into the game account:", bondLamports.toString(), "lamports");
  console.log(
    "Host's total create-time lamport outlay (rents + ATA + bond + fees):",
    hostLamportsBefore - (await connection.getBalance(host.publicKey))
  );

  // ---- join_game ----
  // Nonce 0 is the normal case. If someone front-runs the request account at
  // this seed the join fails with the system program's AccountAlreadyInUse
  // (Custom(0)) and the recovery is simply to retry with nonce + 1, which is a
  // different request address — a real client should loop over nonces on that
  // error rather than giving up. Nothing else about the join changes.
  const NONCE = new anchor.BN(0);
  const networkStateAccount = await vrf.getNetworkState();
  const oraoTreasury = networkStateAccount.config.treasury;
  // Bound what ORAO may charge, at exactly the ceiling that keeps the
  // reimbursement whole: the joiner sinks `request_fee + fulfilled rent`, and
  // the host's bond caps what comes back, so
  //     request_fee <= bond_lamports - fulfilled_rent  <=>  sunk <= bond.
  // Joining under this cap makes under-reimbursement unreachable — and the bond
  // is read off the game account the joiner is about to join, which is exactly
  // what a real client already fetches before deciding to join.
  let maxVrfFee = bondLamports.subn(fulfilledRentLamports);
  if (maxVrfFee.lt(networkStateAccount.config.requestFee)) {
    console.warn(
      `WARNING: ORAO's fee (${networkStateAccount.config.requestFee}) exceeds what this game's ` +
        `bond covers (${maxVrfFee}) — ORAO raised it since create. A real client should skip ` +
        "this game instead of joining under-reimbursed; this smoke joins anyway, and the " +
        "go/no-go number below will show the shortfall."
    );
    maxVrfFee = networkStateAccount.config.requestFee;
  }
  const vrfSeed = vrfSeedFor(game.publicKey, joiner.publicKey, NONCE);
  const request = randomnessAccountAddress(vrfSeed);
  // Pass the mint's owning program: a Token-2022 game's treasury ATA sits at a
  // different address than the classic-SPL one. `scripts/ops.ts` is the
  // canonical reference for the derivations a crank needs.
  const treasuryTokenAccount = getAssociatedTokenAddressSync(
    mint,
    TREASURY,
    true,
    TOKEN_PROGRAM_ID
  );

  const joinerLamportsBefore = await connection.getBalance(joiner.publicKey);
  const joinTx = await program.methods
    .joinGame(NONCE, maxVrfFee)
    .accounts({
      joiner: joiner.publicKey,
      game: game.publicKey,
      mint,
      joinerTokenAccount: joinerTokenAccount.address,
      // `networkState` is not passed: the IDL pins it to ORAO's config PDA, so
      // anchor-ts resolves it itself. The join touches no treasury account at
      // all — create_game made the ATA.
      oraoTreasury,
      request,
      tokenProgram: TOKEN_PROGRAM_ID,
      program: PROGRAM_ID,
    })
    .signers([joiner])
    .rpc();
  console.log("join_game tx:", joinTx);
  console.log("VRF request:", request.toBase58());
  console.log("ORAO request fee paid by the joiner:", networkStateAccount.config.requestFee.toString());
  console.log(
    "Joiner's join-time lamport outlay (fee + request rent + tx fee):",
    joinerLamportsBefore - (await connection.getBalance(joiner.publicKey))
  );
  // Recorded by the program: what a LOSING joiner gets back out of the bond.
  const joinerSunk = (await program.account.game.fetch(game.publicKey)).joinerSunkLamports;
  console.log("...recorded as reimbursable if they lose:", joinerSunk.toString());

  // treasuryTokenAccount was created by create_game, so it's guaranteed to
  // exist by now — this is our pre-settlement baseline for the fee delta.
  const treasuryBalanceBefore = await connection.getTokenAccountBalance(treasuryTokenAccount);

  // ---- poll for fulfillment (what the crank does) ----
  const gamePda = game.publicKey;
  console.log(
    `\nPolling the VRF request every ${POLL_INTERVAL_MS / 1000}s for up to ` +
      `${POLL_TIMEOUT_MS / 60_000} minutes...`
  );
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  let randomness: Uint8Array | null = null;
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL_MS);
    try {
      randomness = (await vrf.getRandomness(vrfSeed)).getFulfilledRandomness();
    } catch {
      // The account can lag the join by a slot or two at this commitment.
      randomness = null;
    }
    if (randomness !== null) break;
    process.stdout.write(".");
  }
  console.log();

  if (randomness === null) {
    // No fulfillment means no settlement is even possible — the eventual
    // liveness path is refund_timeout, not settle.
    process.exitCode = 1;
    console.log(
      "Timed out waiting for ORAO to fulfill the request. `settle` cannot run until it is " +
        "fulfilled (it fails with RandomnessNotFulfilled); after refund_timeout_slots the " +
        "permissionless refund_timeout unwinds the game instead. Inspect with:"
    );
    console.log("  npx tsx ops.ts -k <keypair> check-orao");
    console.log(`  Request account: ${request.toBase58()}`);
    return;
  }

  console.log("ORAO fulfilled the request — the outcome is already readable off-chain.");
  console.log("  outcome:", randomness[0] % 2 === 0 ? "Heads" : "Tails");

  // ---- settle (the crank's transaction) ----
  const joinerLamportsBeforeSettle = await connection.getBalance(joiner.publicKey);
  const settleTx = await program.methods
    .settle()
    .accounts({
      cranker: provider.wallet.publicKey,
      game: gamePda,
      host: host.publicKey,
      // The joiner's wallet: the reimbursement target when the host wins.
      joiner: joiner.publicKey,
      hostTokenAccount: hostTokenAccount.address,
      joinerTokenAccount: joinerTokenAccount.address,
      treasuryTokenAccount,
      mint,
      tokenProgram: TOKEN_PROGRAM_ID,
      program: PROGRAM_ID,
    })
    .rpc();
  console.log("settle tx:", settleTx);

  if ((await connection.getAccountInfo(gamePda)) !== null) {
    console.log("Game account still exists after settle — it should have been closed.");
    process.exitCode = 1;
  }

  const hostBalance = await connection.getTokenAccountBalance(hostTokenAccount.address);
  const joinerBalance = await connection.getTokenAccountBalance(joinerTokenAccount.address);
  console.log("Host token balance:", hostBalance.value.uiAmountString);
  console.log("Joiner token balance:", joinerBalance.value.uiAmountString);

  const event = await findGameSettledEvent(connection, program, settleTx);
  if (event) {
    console.log("GameSettled event:", {
      winner: event.winner.toBase58(),
      outcome: event.outcome === 0 ? "Heads" : "Tails",
      pot: event.pot.toString(),
      fee: event.fee.toString(),
      joinerReimbursed: event.joinerReimbursed.toString(),
    });
    // The loser pays their stake and nothing else: if the host won, the bond
    // just refunded the joiner's ORAO costs; if the joiner won, they kept the
    // pot and bore those costs themselves.
    console.log(
      "Joiner's lamport delta across settle:",
      (await connection.getBalance(joiner.publicKey)) - joinerLamportsBeforeSettle
    );

    // ---- GO / NO-GO ----
    // The joiner's whole round trip: before the join, after the settlement.
    // If the host won, this must be exactly -(their join tx fee) — every other
    // lamport came back, half from ORAO's fulfillment refund and half from the
    // host's bond. That first half is the ONE link LiteSVM cannot verify (the
    // Rust suite models ORAO's refund rather than executing it), and the bond's
    // size is derived from it, so this number is what turns the model into a
    // measurement. Anything more negative than a transaction fee means ORAO's
    // refund behavior is not what the bond assumes: do not ship.
    const joinerRoundTrip =
      (await connection.getBalance(joiner.publicKey)) - joinerLamportsBefore;
    const hostWon = event.winner.equals(host.publicKey);
    console.log(
      `\nGO/NO-GO — joiner's end-to-end lamport net (pre-join -> post-settle): ${joinerRoundTrip}`
    );
    if (hostWon) {
      console.log(
        "  Host won, so this must be exactly -5000 (the joiner's single join transaction fee)."
      );
      if (joinerRoundTrip !== -5_000) {
        console.log("  NO-GO: the losing joiner paid more than their transaction fee.");
        process.exitCode = 1;
      } else {
        console.log("  GO: the losing joiner paid their stake and their tx fee, nothing else.");
      }
    } else {
      // A winning joiner is not reimbursed by design; they took the pot.
      console.log(
        "  Joiner won, so they correctly bore their own ORAO costs — re-run until the host " +
          "wins to exercise the reimbursement leg (it is a coin flip)."
      );
    }
  } else {
    console.log("Could not decode a GameSettled event from the settle transaction.");
    // The event is the verifiable proof this smoke test exists to produce —
    // a missing decode is a failure, not a footnote.
    process.exitCode = 1;
  }

  const treasuryBalanceAfter = await connection.getTokenAccountBalance(treasuryTokenAccount);
  const treasuryDelta =
    BigInt(treasuryBalanceAfter.value.amount) - BigInt(treasuryBalanceBefore.value.amount);
  console.log("Treasury token balance delta:", treasuryDelta.toString());

  console.log(
    "\nCompute units: inspect the settle transaction on the explorer:\n" +
      `  https://explorer.solana.com/tx/${settleTx}?cluster=devnet`
  );
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
