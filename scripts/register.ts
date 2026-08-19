// ORAO Callback-VRF client registration/deposit + coinflip config bootstrap.
// Modeled on ORAO's own `callback/rust/examples/cpi/cli.ts`.
//
// IMPORTANT (see the runbook in the plan): never burn the program's upgrade
// authority without first running ORAO's `Transfer` to move the client
// `owner` to a surviving, team-controlled key — owner-signed `Withdraw` is
// the only way to recover the Client PDA's accumulating rent surplus
// (~0.0067 SOL per fulfilled game). `register` below sets that owner to
// whatever wallet you pass as `-k`, so use a team-controlled key, not a
// throwaway one.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { OraoCb, RegisterBuilder, clientAddress } from "@orao-network/solana-vrf-cb";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";
import { Command } from "commander";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Coinflip } from "../target/types/coinflip.js";

const require = createRequire(import.meta.url);
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

const [CONFIG_PDA, CONFIG_BUMP] = web3.PublicKey.findProgramAddressSync(
  [Buffer.from("config")],
  PROGRAM_ID
);

const BPF_LOADER_UPGRADEABLE_ID = new web3.PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

function programDataAddress(programId: web3.PublicKey): web3.PublicKey {
  return web3.PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE_ID
  )[0];
}

// Raw (snake_case) IDL, loaded once. `anchor.Program` camelCases it internally
// when building method/account namespaces, but we also read it here directly
// (its `address` and `constants` fields) before any of that conversion.
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

const idlAddress = new web3.PublicKey(IDL_RAW.address);
if (!idlAddress.equals(PROGRAM_ID)) {
  throw new Error(
    `program id mismatch: target/idl/coinflip.json's address (${idlAddress.toBase58()}) ` +
      `does not match the keypair's pubkey (${PROGRAM_ID.toBase58()}) — rebuild the IDL ` +
      "(`anchor build`) after redeploying under a new program id"
  );
}

/** Reads a `#[constant]` value straight out of the IDL instead of hardcoding it. */
function idlConstant(name: string): anchor.BN {
  const found = IDL_RAW.constants?.find((c) => c.name === name);
  if (!found) {
    throw new Error(
      `IDL constant "${name}" not found in target/idl/coinflip.json — rebuild the IDL ` +
        "(`anchor build`), or this #[constant] was renamed/removed in the program source"
    );
  }
  return new anchor.BN(found.value);
}

/** Same, for a `Pubkey` constant (the IDL stores it as a base58 string). */
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

const MIN_SETTLE_MARGIN_SLOTS = idlConstant("MIN_SETTLE_MARGIN_SLOTS");
const MIN_REFUND_TIMEOUT_SLOTS = idlConstant("MIN_REFUND_TIMEOUT_SLOTS");
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
    "3) register 4) deposit 5) check-orao, then scripts/smoke.ts"
);
cli.requiredOption("-k, --key <path>", "upgrade-authority keypair path");
cli.option("-c, --cluster <name>", "devnet|mainnet", "devnet");

cli
  .command("register")
  .description("Registers the coinflip program as an ORAO VRF client (state PDA = Config)")
  .action(async (_opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const vrf = new OraoCb(p);
    const builder = await new RegisterBuilder(vrf, PROGRAM_ID, CONFIG_PDA, [
      Buffer.from("config"),
      Buffer.from([CONFIG_BUMP]),
    ]).build();
    const tx = await builder.rpc();
    console.log("Registered client:", clientAddress(PROGRAM_ID, CONFIG_PDA)[0].toBase58());
    console.log("Tx:", tx);
    console.log(
      "\nIMPORTANT: the client owner is now this wallet. Only an owner-signed " +
        "Withdraw can recover the Client PDA's rent surplus — keep this key, or " +
        "run ORAO's Transfer to move ownership to a surviving team key before " +
        "ever retiring it."
    );
  });

cli
  .command("deposit")
  .description("Deposits SOL into the ORAO client's balance (pays request fees + request rent)")
  .requiredOption("--lamports <n>", "amount to deposit into the client balance")
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const [client] = clientAddress(PROGRAM_ID, CONFIG_PDA);
    const tx = await p.sendAndConfirm(
      new web3.Transaction().add(
        web3.SystemProgram.transfer({
          fromPubkey: p.publicKey,
          toPubkey: client,
          // bigint, not Number(): avoids silent precision loss above 2^53 lamports.
          lamports: BigInt(opts.lamports),
        })
      )
    );
    console.log("Deposited. Tx:", tx);
  });

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
    "slots after join before refund_timeout is allowed " +
      "(must clear ORAO's callback_deadline + MIN_SETTLE_MARGIN_SLOTS margin; see check-orao)",
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
    "Fetches ORAO's NetworkState and our Config, and warns if the refund " +
      "timeout doesn't clear ORAO's callback deadline by the required margin"
  )
  .action(async (_opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const vrf = new OraoCb(p);
    const networkState = await vrf.getNetworkState();
    const program = coinflipProgram(p);
    const config = await program.account.config.fetch(CONFIG_PDA);

    const requestFee = networkState.config.requestFee;
    const callbackDeadline = networkState.config.callbackDeadline;
    console.log("ORAO request_fee:", requestFee.toString(), "lamports");
    console.log("ORAO callback_deadline:", callbackDeadline.toString(), "slots");
    console.log("Our refund_timeout_slots:", config.refundTimeoutSlots.toString());

    // Mirrors join_game's own dynamic check.
    const minTimeout = callbackDeadline.add(MIN_SETTLE_MARGIN_SLOTS);
    if (minTimeout.gt(config.refundTimeoutSlots)) {
      console.warn(
        `WARNING: callback_deadline + MIN_SETTLE_MARGIN_SLOTS (${minTimeout.toString()}) ` +
          `exceeds refund_timeout_slots (${config.refundTimeoutSlots.toString()}) — join_game ` +
          "will reject every join until refund_timeout_slots is raised via update_config."
      );
      // Non-zero so this can gate a monitor/runbook, not just a human reading stdout.
      process.exitCode = 1;
    } else {
      console.log(
        "OK: refund_timeout_slots clears ORAO's callback deadline by the required margin."
      );
    }
  });

cli
  .command("settle-fallback")
  .description(
    "Runs the permissionless settle_fallback crank on a stuck game (the liveness " +
      "backstop for when ORAO fulfills but the callback never runs)"
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
      .settleFallback()
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
    console.log("settle_fallback tx:", tx);
  });

await cli.parseAsync().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
