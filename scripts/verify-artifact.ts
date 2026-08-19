// Refuses to let a test build reach mainnet.
//
// The treasury is a compile-time constant, so a `--features local` binary is
// indistinguishable from a real one by name, size, or program id — it just
// silently pays every fee to a keypair that is committed to this repository.
// This script proves which constant a binary actually carries, before deploy
// (against target/deploy/coinflip.so) and after it (against the on-chain
// ProgramData, with --url).
//
// Needs no keypair and no wallet: it only reads.
//
//   npx tsx verify-artifact.ts                       # local artifact
//   npx tsx verify-artifact.ts --url https://api.mainnet-beta.solana.com
import { web3 } from "@coral-xyz/anchor";
import { Command } from "commander";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { IDL_ADDRESS, IDL_PATH, idlConstantPubkey } from "./idl.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

const SO_PATH = join(__dirname, "../target/deploy/coinflip.so");

// `treasury::ID`'s `local` variant — programs/coinflip/tests/fixtures/treasury-local.json.
// Hardcoded on purpose: this is the value we are checking is ABSENT, so it must
// not come from the same IDL the binary is being checked against.
const LOCAL_TREASURY = new web3.PublicKey("9wR75bCR1bo68BygzHkgJ3N735u5TmGsVhzjRrFzNUtJ");

const BPF_LOADER_UPGRADEABLE_ID = new web3.PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

// UpgradeableLoaderState::ProgramData, bincode-encoded: 4-byte enum tag,
// 8-byte deployed slot, 1-byte Option tag, 32-byte upgrade authority — then
// the ELF.
const PROGRAMDATA_HEADER_LEN = 4 + 8 + 1 + 32;

/**
 * Whether `elf` embeds `key` as a compile-time constant.
 *
 * Mirrors the Rust harness's probe (tests/common/mod.rs): the treasury constant
 * is only ever compared against, and the release SBF build turns that into
 * immediate loads rather than a contiguous 32-byte blob, splitting each 64-bit
 * immediate into two 4-byte halves in separate instruction words. So look for
 * all eight 4-byte words — eight independent hits cannot line up by chance.
 */
function embedsKey(elf: Buffer, key: web3.PublicKey): boolean {
  const bytes = key.toBuffer();
  for (let i = 0; i < 32; i += 4) {
    if (elf.indexOf(bytes.subarray(i, i + 4)) === -1) return false;
  }
  return true;
}

function programDataAddress(programId: web3.PublicKey): web3.PublicKey {
  return web3.PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE_ID
  )[0];
}

async function fetchDeployedElf(url: string): Promise<Buffer> {
  const connection = new web3.Connection(url, "confirmed");
  const address = programDataAddress(IDL_ADDRESS);
  const account = await connection.getAccountInfo(address, "confirmed");
  if (!account) {
    throw new Error(
      `ProgramData ${address.toBase58()} not found on ${url} — is ${IDL_ADDRESS.toBase58()} ` +
        "deployed on this cluster, and deployed with the upgradeable loader?"
    );
  }
  if (!account.owner.equals(BPF_LOADER_UPGRADEABLE_ID)) {
    throw new Error(
      `${address.toBase58()} is not owned by the upgradeable loader (owner ${account.owner.toBase58()})`
    );
  }
  return Buffer.from(account.data).subarray(PROGRAMDATA_HEADER_LEN);
}

/** Runs every probe, printing each result; returns false if any failed. */
function verify(elf: Buffer, source: string): boolean {
  const idlTreasury = idlConstantPubkey("TREASURY");
  const failures: string[] = [];

  console.log(`Artifact:      ${source} (${elf.length} bytes)`);
  console.log(`IDL:           ${IDL_PATH}`);
  console.log(`IDL TREASURY:  ${idlTreasury.toBase58()}`);
  console.log(`Program id:    ${IDL_ADDRESS.toBase58()}`);
  console.log();

  // (a) The one that must never ship.
  if (embedsKey(elf, LOCAL_TREASURY)) {
    failures.push(
      `built with --features local (embeds the test treasury ${LOCAL_TREASURY.toBase58()}) — DO NOT DEPLOY`
    );
  } else {
    console.log("OK  binary does not embed the local/test treasury");
  }

  // (b) Positive half: if a toolchain change ever stops embedding the constant
  // the way the probe expects, (a) would pass vacuously. This fails instead.
  if (embedsKey(elf, idlTreasury)) {
    console.log("OK  binary embeds the IDL's TREASURY constant");
  } else {
    failures.push(
      `binary does not embed the IDL's TREASURY constant (${idlTreasury.toBase58()}) — ` +
        "the IDL and the binary come from different builds, or the compiler stopped " +
        "embedding the constant this probe looks for (check tests/common/mod.rs's elf_embeds_key)"
    );
  }

  // (c) A local IDL beside a plain binary: the binary would be fine, but every
  // client reading TREASURY out of that IDL would send fees to the test key.
  if (idlTreasury.equals(LOCAL_TREASURY)) {
    failures.push(
      "the IDL was built with --features local (its TREASURY constant is the test key) — " +
        "rebuild it with `anchor idl build -o target/idl/coinflip.json -t target/types/coinflip.ts`"
    );
  } else {
    console.log("OK  IDL's TREASURY constant is not the local/test treasury");
  }

  console.log();
  if (failures.length === 0) {
    console.log("PASS: this artifact is a deployable build.");
    return true;
  }
  for (const failure of failures) console.error(`FAIL: ${failure}`);
  return false;
}

const cli = new Command();
cli
  .description(
    "Verifies that a coinflip build is a DEPLOYABLE one: that it carries the real " +
      "treasury constant and not the committed test key. Reads only — no keypair needed."
  )
  .option(
    "--url <rpc>",
    "verify the DEPLOYED program at this RPC endpoint instead of target/deploy/coinflip.so"
  )
  .action(async (opts) => {
    const [elf, source] = opts.url
      ? [await fetchDeployedElf(opts.url), `on-chain ProgramData via ${opts.url}`]
      : [readFileSync(SO_PATH), SO_PATH];
    if (!verify(elf, source)) process.exitCode = 1;
  });

await cli.parseAsync().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
