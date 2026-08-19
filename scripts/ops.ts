// Ops CLI: coinflip config bootstrap, an ORAO health check, and a manual
// `settle` crank. Plain ORAO VRF needs no client registration and no program-
// owned float, so there is nothing to register or top up here — the joiner
// pays ORAO directly at join time.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { Orao } from "@orao-network/solana-vrf";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import { Command } from "commander";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Coinflip } from "../target/types/coinflip.js";
import { IDL_ADDRESS, IDL_RAW, idlConstant, idlConstantPubkey } from "./idl.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

function loadKeypair(path: string): web3.Keypair {
  const raw = JSON.parse(readFileSync(path, "utf8"));
  return web3.Keypair.fromSecretKey(new Uint8Array(raw));
}

// The coinflip program's public key. Only the pubkey is ever read out of
// this file — its secret key is never printed or used as a signer here.
// Prefers the freshly built `target/deploy` keypair, falling back to the
// checked-in `keys/` copy (both are gitignored).
const PROGRAM_KEYPAIR_PATH = [
  join(__dirname, "../target/deploy/coinflip-keypair.json"),
  join(__dirname, "../keys/coinflip-keypair.json"),
].find(existsSync);

if (!PROGRAM_KEYPAIR_PATH) {
  throw new Error(
    "coinflip program keypair not found — expected target/deploy/coinflip-keypair.json " +
      "or keys/coinflip-keypair.json (run `anchor build` or check keys/)"
  );
}

const PROGRAM_ID = loadKeypair(PROGRAM_KEYPAIR_PATH).publicKey;

const [CONFIG_PDA] = web3.PublicKey.findProgramAddressSync([Buffer.from("config")], PROGRAM_ID);

const BPF_LOADER_UPGRADEABLE_ID = new web3.PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

function programDataAddress(programId: web3.PublicKey): web3.PublicKey {
  return web3.PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE_ID
  )[0];
}

if (!IDL_ADDRESS.equals(PROGRAM_ID)) {
  throw new Error(
    `program id mismatch: target/idl/coinflip.json's address (${IDL_ADDRESS.toBase58()}) ` +
      `does not match the keypair's pubkey (${PROGRAM_ID.toBase58()}) — rebuild the IDL ` +
      "(`anchor build`) after redeploying under a new program id"
  );
}

const MIN_REFUND_TIMEOUT_SLOTS = idlConstant("MIN_REFUND_TIMEOUT_SLOTS");
const MAX_REFUND_TIMEOUT_SLOTS = idlConstant("MAX_REFUND_TIMEOUT_SLOTS");
// The fee destination is baked into the program, not stored in Config: an IDL
// built with `--features local` carries the test key, so the IDL must come
// from the same build as the deployed binary.
const TREASURY = idlConstantPubkey("TREASURY");

/**
 * Rejects anything but a plain base-10 non-negative integer string.
 * `Number("1oo")` is `NaN`, which borsh would silently serialize as 0 into a
 * one-shot config account — reject early instead of corrupting it quietly.
 */
function parseIntegerOption(value: string, flagName: string): string {
  if (!/^\d+$/.test(value)) {
    throw new Error(`invalid ${flagName} "${value}" — must be a non-negative integer`);
  }
  return value;
}

const ALLOWED_CLUSTERS = ["devnet", "mainnet"] as const;

function clusterUrl(cluster: string): string {
  if (cluster === "devnet") return web3.clusterApiUrl("devnet");
  if (cluster === "mainnet") return web3.clusterApiUrl("mainnet-beta");
  // Never fall through to a default cluster — an operator typo must not
  // silently point a mainnet-authority key at the wrong network.
  throw new Error(
    `invalid --cluster "${cluster}" — must be one of: ${ALLOWED_CLUSTERS.join(", ")}`
  );
}

function provider(cluster: string, keyPath: string): anchor.AnchorProvider {
  const url = clusterUrl(cluster);
  const wallet = new anchor.Wallet(loadKeypair(keyPath));
  console.log("Cluster:", url);
  console.log("Wallet:", wallet.publicKey.toBase58());
  return new anchor.AnchorProvider(new web3.Connection(url, "confirmed"), wallet, {});
}

function coinflipProgram(p: anchor.AnchorProvider): anchor.Program<Coinflip> {
  return new anchor.Program<Coinflip>(IDL_RAW as unknown as Coinflip, p);
}

const cli = new Command();
cli.description(
  "Ops CLI for the coinflip program. Order: 1) anchor deploy 2) init-config " +
    "3) check-orao, then scripts/smoke.ts. Settlement is the crank's job; " +
    "`settle` here is the manual version of it."
);
cli.requiredOption("-k, --key <path>", "upgrade-authority keypair path");
cli.option("-c, --cluster <name>", "devnet|mainnet", "devnet");

cli
  .command("init-config")
  .description(
    "Calls initialize_config — payer MUST be the program's upgrade authority " +
      "(the deployer gate in initialize_config.rs)"
  )
  .option("--admin <pubkey>", "admin authority (defaults to the wallet)")
  .option("--fee-bps <n>", "protocol fee in basis points", "100")
  .option(
    "--refund-timeout-slots <n>",
    `slots after join before refund_timeout is allowed (bounded to ` +
      `[${MIN_REFUND_TIMEOUT_SLOTS}, ${MAX_REFUND_TIMEOUT_SLOTS}])`,
    MIN_REFUND_TIMEOUT_SLOTS.toString()
  )
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const program = coinflipProgram(p);
    const admin = opts.admin ? new web3.PublicKey(opts.admin) : p.wallet.publicKey;
    const feeBps = Number(parseIntegerOption(opts.feeBps, "--fee-bps"));
    const refundTimeoutSlots = new anchor.BN(
      parseIntegerOption(opts.refundTimeoutSlots, "--refund-timeout-slots")
    );

    const tx = await program.methods
      .initializeConfig(admin, feeBps, refundTimeoutSlots)
      .accounts({
        payer: p.wallet.publicKey,
        programData: programDataAddress(PROGRAM_ID),
      })
      .rpc();
    console.log("Config initialized:", CONFIG_PDA.toBase58());
    console.log("Treasury (compile-time constant):", TREASURY.toBase58());
    console.log("Tx:", tx);
  });

cli
  .command("check-orao")
  .description(
    "Fetches ORAO's NetworkState and our Config, and reports what a joiner " +
      "will pay ORAO out of their own wallet at join time"
  )
  .action(async (_opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const vrf = new Orao(p);
    const networkState = await vrf.getNetworkState();
    const program = coinflipProgram(p);
    const config = await program.account.config.fetch(CONFIG_PDA);

    // `8 + RandomnessV2::PENDING_SIZE` — what ORAO's RequestV2 allocates. ORAO
    // shrinks the account to its fulfilled size on fulfillment and returns the
    // freed rent to the request's client, i.e. to the joiner.
    const PENDING_REQUEST_LEN = 749;
    const FULFILLED_REQUEST_LEN = 137;
    const [pendingRent, fulfilledRent] = await Promise.all([
      p.connection.getMinimumBalanceForRentExemption(PENDING_REQUEST_LEN),
      p.connection.getMinimumBalanceForRentExemption(FULFILLED_REQUEST_LEN),
    ]);

    console.log("ORAO program:", vrf.programId.toBase58());
    console.log("ORAO treasury:", networkState.config.treasury.toBase58());
    console.log("ORAO request_fee:", networkState.config.requestFee.toString(), "lamports");
    console.log("ORAO fulfillment authorities:", networkState.config.fulfillmentAuthorities.length);
    console.log("Pending request rent (paid by the joiner):", pendingRent, "lamports");
    console.log("Returned to the joiner on fulfillment:", pendingRent - fulfilledRent, "lamports");
    console.log("Our refund_timeout_slots:", config.refundTimeoutSlots.toString());
    console.log(
      "Joiner's join-time lamport outlay (excl. tx fee and first-of-mint treasury ATA rent):",
      networkState.config.requestFee.addn(pendingRent).toString()
    );

    // The floor is a program constant, so this can only trip if the deployed
    // binary and the IDL this script reads disagree.
    if (config.refundTimeoutSlots.lt(MIN_REFUND_TIMEOUT_SLOTS)) {
      console.warn(
        `WARNING: refund_timeout_slots (${config.refundTimeoutSlots.toString()}) is below ` +
          `MIN_REFUND_TIMEOUT_SLOTS (${MIN_REFUND_TIMEOUT_SLOTS.toString()}) — the IDL and the ` +
          "deployed program are out of sync."
      );
      // Non-zero so this can gate a monitor/runbook, not just a human reading stdout.
      process.exitCode = 1;
    }
  });

cli
  .command("settle")
  .description(
    "Manually runs the permissionless `settle` instruction on one game — the " +
      "same call the crank makes once ORAO fulfills the request"
  )
  .requiredOption("--game <pubkey>", "the game account's pubkey")
  .option(
    "--host-token-account <pubkey>",
    "override the host's payout account (defaults to the one recorded on the game)"
  )
  .option(
    "--joiner-token-account <pubkey>",
    "override the joiner's payout account (defaults to the one recorded on the game)"
  )
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const program = coinflipProgram(p);

    const gamePubkey = new web3.PublicKey(opts.game);
    const game = await program.account.game.fetch(gamePubkey);

    const mintInfo = await p.connection.getAccountInfo(game.tokenMint);
    if (!mintInfo) {
      throw new Error(`mint ${game.tokenMint.toBase58()} not found`);
    }

    // Must pass the mint's actual owning token program: a Token-2022 game's
    // treasury ATA lives at a different address than the classic-SPL one.
    const treasuryTokenAccount = getAssociatedTokenAddressSync(
      game.tokenMint,
      TREASURY,
      true,
      mintInfo.owner
    );
    const hostTokenAccount = opts.hostTokenAccount
      ? new web3.PublicKey(opts.hostTokenAccount)
      : game.hostTokenAccount;
    const joinerTokenAccount = opts.joinerTokenAccount
      ? new web3.PublicKey(opts.joinerTokenAccount)
      : game.joinerTokenAccount;

    const tx = await program.methods
      .settle()
      .accounts({
        cranker: p.wallet.publicKey,
        game: gamePubkey,
        host: game.host,
        hostTokenAccount,
        joinerTokenAccount,
        treasuryTokenAccount,
        mint: game.tokenMint,
        tokenProgram: mintInfo.owner,
        program: PROGRAM_ID,
      })
      .rpc();
    console.log("settle tx:", tx);
  });

await cli.parseAsync().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
