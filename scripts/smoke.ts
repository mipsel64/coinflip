// Devnet smoke test: create_game + join_game against a REAL deployed ORAO
// VRF, then watch for the callback to settle the game with no third
// transaction. LiteSVM (the Rust test suite) fabricates ORAO's accounts, so
// this is the only place the actual callback happy path gets exercised.
//
// Prerequisites (see register.ts): the program is deployed, registered as an
// ORAO client, the client is deposited with SOL, and initialize_config has
// run. The provider wallet just needs enough SOL to fund two ephemeral
// players and pay rent/fees.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { OraoCb, clientAddress, requestAccountAddress } from "@orao-network/solana-vrf-cb";
import {
  createMint,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
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

function loadKeypair(path: string): web3.Keypair {
  const raw = JSON.parse(readFileSync(path, "utf8"));
  return web3.Keypair.fromSecretKey(new Uint8Array(raw));
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

const [CONFIG_PDA] = web3.PublicKey.findProgramAddressSync([Buffer.from("config")], PROGRAM_ID);

function escrowPda(game: web3.PublicKey): web3.PublicKey {
  return web3.PublicKey.findProgramAddressSync(
    [Buffer.from("escrow"), game.toBuffer()],
    PROGRAM_ID
  )[0];
}

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
  const wallet = new anchor.Wallet(loadKeypair(walletPath));
  return new anchor.AnchorProvider(new web3.Connection(url, "confirmed"), wallet, {
    commitment: "confirmed",
  });
}

async function main() {
  const provider = loadProvider();
  const connection = provider.connection;
  const walletPayer = provider.wallet.payer;
  if (!walletPayer) {
    throw new Error("provider wallet has no local payer keypair");
  }

  const idl = require("../target/idl/coinflip.json") as Coinflip;
  const program = new anchor.Program<Coinflip>(idl, provider);
  const vrf = new OraoCb(provider);

  console.log("Wallet:", provider.wallet.publicKey.toBase58());
  console.log("Program:", PROGRAM_ID.toBase58());

  const config = await program.account.config.fetch(CONFIG_PDA);
  console.log("Treasury:", config.treasury.toBase58());

  // ---- throwaway mint + two funded players ----
  console.log("\nCreating throwaway mint...");
  const mint = await createMint(connection, walletPayer, provider.wallet.publicKey, null, 9);

  const host = web3.Keypair.generate();
  const joiner = web3.Keypair.generate();
  console.log("Host:", host.publicKey.toBase58());
  console.log("Joiner:", joiner.publicKey.toBase58());

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
  const game = web3.Keypair.generate();
  const escrow = escrowPda(game.publicKey);
  console.log("\nGame:", game.publicKey.toBase58());

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

  const joinTx = await program.methods
    .joinGame()
    .accounts({
      joiner: joiner.publicKey,
      game: game.publicKey,
      host: host.publicKey,
      mint,
      joinerTokenAccount: joinerTokenAccount.address,
      hostTokenAccount: hostTokenAccount.address,
      treasury: config.treasury,
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
    console.log(
      "Timed out waiting for the callback. If ORAO has fulfilled the request but the " +
        "callback didn't run (check with `npx tsx register.ts check-orao`), a permissionless " +
        "settle_fallback crank can finish it:"
    );
    console.log(
      JSON.stringify(
        {
          cranker: "<any funded keypair>",
          config: CONFIG_PDA.toBase58(),
          client: client.toBase58(),
          request: request.toBase58(),
          game: gamePda.toBase58(),
          escrow: escrow.toBase58(),
          host: host.publicKey.toBase58(),
          hostTokenAccount: hostTokenAccount.address.toBase58(),
          joinerTokenAccount: joinerTokenAccount.address.toBase58(),
          treasuryTokenAccount: "<treasury's ATA for this mint>",
          mint: mint.toBase58(),
          tokenProgram: TOKEN_PROGRAM_ID.toBase58(),
        },
        null,
        2
      )
    );
    console.log(
      "Note: settle_fallback requires the ORAO request to already be fulfilled — it will " +
        "fail with RandomnessNotFulfilled before that (see refund_timeout for the eventual " +
        "liveness path)."
    );
  }
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
