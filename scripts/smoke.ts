// Devnet smoke test: create_game + join_game against a REAL deployed ORAO
// VRF, then watch for the callback to settle the game with no third
// transaction. LiteSVM (the Rust test suite) fabricates ORAO's accounts, so
// this is the only place the actual callback happy path gets exercised.
//
// Prerequisites (see register.ts): the program is deployed, registered as an
// ORAO client, the client is deposited with SOL, and initialize_config has
// run. The provider wallet just needs enough SOL to fund two ephemeral
// players and pay rent/fees.
//
// Event decoding: the program emits events via `emit_cpi!` (a self-CPI whose
// instruction data IS the event), not the older `sol_log_data`/"Program
// data:" log convention — so `Program.addEventListener` (which only parses
// text logs) can never see it. Instead, once the poll below tells us the
// game closed, we fetch that one closing transaction and pick the event out
// of its `innerInstructions`, exactly like the Rust test suite's
// `find_cpi_event` helper (tests/common/mod.rs) does. That's also more
// reliable than a log subscription here: no WebSocket needs to stay open
// across the whole (up to 3-minute) poll loop, just one HTTP call after we
// already know settlement happened.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { OraoCb, clientAddress, requestAccountAddress } from "@orao-network/solana-vrf-cb";
import {
  createMint,
  getAssociatedTokenAddressSync,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Coinflip } from "../target/types/coinflip.js";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));

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

interface RawIdlConstant {
  name: string;
  type: string;
  value: string;
}
interface RawIdl {
  address: string;
  constants?: RawIdlConstant[];
}

const IDL_RAW = require("../target/idl/coinflip.json") as RawIdl;

/** Reads a `#[constant]` pubkey straight out of the IDL instead of hardcoding it. */
function idlConstantPubkey(name: string): web3.PublicKey {
  const found = IDL_RAW.constants?.find((c) => c.name === name);
  if (!found) {
    throw new Error(
      `IDL constant "${name}" not found in target/idl/coinflip.json — rebuild the IDL ` +
        "(`anchor build`), or this #[constant] was renamed/removed in the program source"
    );
  }
  return new web3.PublicKey(found.value);
}

// Compile-time fee destination: an IDL built with `--features local` carries
// the test key, so the IDL must come from the same build as the deployment.
const TREASURY = idlConstantPubkey("TREASURY");

const idlAddress = new web3.PublicKey(IDL_RAW.address);
if (!idlAddress.equals(PROGRAM_ID)) {
  throw new Error(
    `program id mismatch: target/idl/coinflip.json's address (${idlAddress.toBase58()}) ` +
      `does not match the keypair's pubkey (${PROGRAM_ID.toBase58()}) — rebuild the IDL ` +
      "(`anchor build`) after redeploying under a new program id"
  );
}

const [CONFIG_PDA] = web3.PublicKey.findProgramAddressSync([Buffer.from("config")], PROGRAM_ID);

/** Mirrors join_game's own derivation: sha256("coinflip-vrf-seed", game, joiner). */
function vrfSeedFor(game: web3.PublicKey, joiner: web3.PublicKey): Buffer {
  return createHash("sha256")
    .update(Buffer.from("coinflip-vrf-seed"))
    .update(game.toBuffer())
    .update(joiner.toBuffer())
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
}

/** Finds and decodes the GameSettled event out of the transaction that closed `gamePda`. */
async function findGameSettledEvent(
  connection: web3.Connection,
  program: anchor.Program<Coinflip>,
  gamePda: web3.PublicKey
): Promise<GameSettledEvent | null> {
  const [sigInfo] = await connection.getSignaturesForAddress(gamePda, { limit: 1 });
  if (!sigInfo) return null;
  const tx = await connection.getTransaction(sigInfo.signature, {
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
  const vrf = new OraoCb(provider);

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
  console.log();
  const createTx = await program.methods
    .createGame(0, STAKE_AMOUNT) // side = Heads
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

  // ---- join_game ----
  const vrfSeed = vrfSeedFor(game.publicKey, joiner.publicKey);
  const [client] = clientAddress(PROGRAM_ID, CONFIG_PDA);
  const [request] = requestAccountAddress(client, vrfSeed);
  const networkStateAccount = await vrf.getNetworkState();
  const oraoTreasury = networkStateAccount.config.treasury;
  const treasuryTokenAccount = getAssociatedTokenAddressSync(mint, TREASURY, true);

  const joinTx = await program.methods
    .joinGame()
    .accounts({
      joiner: joiner.publicKey,
      game: game.publicKey,
      host: host.publicKey,
      mint,
      joinerTokenAccount: joinerTokenAccount.address,
      hostTokenAccount: hostTokenAccount.address,
      // `treasury` is not passed: the IDL pins it to the program's constant, so
      // anchor-ts resolves it (and the treasury ATA derived from it) itself.
      oraoTreasury,
      request,
      tokenProgram: TOKEN_PROGRAM_ID,
      program: PROGRAM_ID,
    })
    .signers([joiner])
    .rpc();
  console.log("join_game tx:", joinTx);
  console.log(
    `Inspect logs: solana logs ${PROGRAM_ID.toBase58()} -u devnet ` +
      "(watch for settle_callback + GameSettled once ORAO fulfills)"
  );

  // treasuryTokenAccount is init_if_needed'd by join_game, so it's guaranteed
  // to exist by now — this is our pre-settlement baseline for the fee delta.
  const treasuryBalanceBefore = await connection.getTokenAccountBalance(treasuryTokenAccount);

  // ---- poll for settlement ----
  const gamePda = game.publicKey;
  console.log(
    `\nPolling game account every ${POLL_INTERVAL_MS / 1000}s for up to ` +
      `${POLL_TIMEOUT_MS / 60_000} minutes...`
  );
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  let settled = false;
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL_MS);
    const info = await connection.getAccountInfo(gamePda);
    if (info === null) {
      settled = true;
      break;
    }
    process.stdout.write(".");
  }
  console.log();

  if (settled) {
    console.log("Game account closed — settled via the ORAO callback (no third tx needed).");
    const hostBalance = await connection.getTokenAccountBalance(hostTokenAccount.address);
    const joinerBalance = await connection.getTokenAccountBalance(joinerTokenAccount.address);
    console.log("Host token balance:", hostBalance.value.uiAmountString);
    console.log("Joiner token balance:", joinerBalance.value.uiAmountString);

    const event = await findGameSettledEvent(connection, program, gamePda);
    if (event) {
      console.log("GameSettled event:", {
        winner: event.winner.toBase58(),
        outcome: event.outcome === 0 ? "Heads" : "Tails",
        pot: event.pot.toString(),
        fee: event.fee.toString(),
      });
    } else {
      console.log(
        "Could not decode a GameSettled event from the closing transaction " +
          "(check the explorer link below)."
      );
      // The event is the verifiable proof this smoke test exists to produce —
      // a missing decode is a failure, not a footnote.
      process.exitCode = 1;
    }

    const treasuryBalanceAfter = await connection.getTokenAccountBalance(treasuryTokenAccount);
    const treasuryDelta =
      BigInt(treasuryBalanceAfter.value.amount) - BigInt(treasuryBalanceBefore.value.amount);
    console.log("Treasury token balance delta:", treasuryDelta.toString());

    // The most recent tx touching the program right after settlement is almost
    // certainly ORAO's fulfill (the one that CPIs into our settle_callback) —
    // point at it directly instead of just the program's activity page.
    const [latest] = await connection.getSignaturesForAddress(PROGRAM_ID, { limit: 1 });
    console.log(
      "\nCompute units: inspect the settle_callback CPI's compute units on the explorer:\n" +
        (latest
          ? `  https://explorer.solana.com/tx/${latest.signature}?cluster=devnet`
          : `  https://explorer.solana.com/address/${PROGRAM_ID.toBase58()}?cluster=devnet`)
    );
  } else {
    // The callback not firing within the window is exactly the failure mode
    // this script exists to catch — it must not exit 0.
    process.exitCode = 1;
    console.log(
      "Timed out waiting for the callback. The permissionless settle_fallback crank " +
        "derives everything it needs (request, escrow, client, treasury ATA) " +
        "from the game account itself — run:"
    );
    console.log(
      `  npx tsx register.ts -k <keypair> settle-fallback --game ${gamePda.toBase58()}`
    );
    console.log(
      "Note: settle_fallback requires the ORAO request to already be fulfilled — it will " +
        "fail with RandomnessNotFulfilled before that (check with " +
        "`npx tsx register.ts -k <keypair> check-orao`; see refund_timeout for the eventual " +
        "liveness path)."
    );
  }
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
