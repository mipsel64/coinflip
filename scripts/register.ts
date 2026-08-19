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

function provider(cluster: string, keyPath: string): anchor.AnchorProvider {
  const url =
    cluster === "devnet" ? web3.clusterApiUrl("devnet") : web3.clusterApiUrl("mainnet-beta");
  const kp = loadKeypair(keyPath);
  return new anchor.AnchorProvider(
    new web3.Connection(url, "confirmed"),
    new anchor.Wallet(kp),
    {}
  );
}

function coinflipProgram(p: anchor.AnchorProvider): anchor.Program<Coinflip> {
  const idl = require("../target/idl/coinflip.json") as Coinflip;
  return new anchor.Program<Coinflip>(idl, p);
}

const cli = new Command();
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
  .requiredOption("--lamports <n>", "amount to deposit into the client balance")
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const [client] = clientAddress(PROGRAM_ID, CONFIG_PDA);
    const tx = await p.sendAndConfirm(
      new web3.Transaction().add(
        web3.SystemProgram.transfer({
          fromPubkey: p.publicKey,
          toPubkey: client,
          lamports: Number(opts.lamports),
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
  .option("--treasury <pubkey>", "fee-destination authority (defaults to the wallet)")
  .option("--fee-bps <n>", "protocol fee in basis points", "100")
  .option(
    "--refund-timeout-slots <n>",
    "slots after join before refund_timeout is allowed " +
      "(must clear ORAO's callback_deadline + 1800-slot margin; see check-orao)",
    "18000"
  )
  .action(async (opts, cmd) => {
    const p = provider(cmd.parent.opts().cluster, cmd.parent.opts().key);
    const program = coinflipProgram(p);
    const admin = opts.admin ? new web3.PublicKey(opts.admin) : p.wallet.publicKey;
    const treasury = opts.treasury ? new web3.PublicKey(opts.treasury) : p.wallet.publicKey;

    const tx = await program.methods
      .initializeConfig(
        admin,
        treasury,
        Number(opts.feeBps),
        new anchor.BN(opts.refundTimeoutSlots)
      )
      .accounts({
        payer: p.wallet.publicKey,
        programData: programDataAddress(PROGRAM_ID),
      })
      .rpc();
    console.log("Config initialized:", CONFIG_PDA.toBase58());
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

    // Mirrors join_game's own dynamic check (MIN_SETTLE_MARGIN_SLOTS = 1_800).
    const minTimeout = callbackDeadline.addn(1_800);
    if (minTimeout.gt(config.refundTimeoutSlots)) {
      console.warn(
        `WARNING: callback_deadline + 1800 (${minTimeout.toString()}) exceeds ` +
          `refund_timeout_slots (${config.refundTimeoutSlots.toString()}) — join_game ` +
          "will reject every join until refund_timeout_slots is raised via update_config."
      );
    } else {
      console.log(
        "OK: refund_timeout_slots clears ORAO's callback deadline by the required margin."
      );
    }
  });

cli.parseAsync();
