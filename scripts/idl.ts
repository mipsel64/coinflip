// Shared access to the built IDL: its address and its `#[constant]` values.
//
// The program's fee destination is a compile-time constant, not account state,
// so every script reads it from here rather than fetching Config or hardcoding
// a pubkey. An IDL built with `--features local` carries the TEST treasury —
// see scripts/verify-artifact.ts, which refuses to let such a build ship.
import * as anchor from "@coral-xyz/anchor";
import { web3 } from "@coral-xyz/anchor";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));

/** Raw (snake_case) IDL, as anchor writes it — `anchor.Program` camelCases it
 * internally when building method/account namespaces, but `address` and
 * `constants` are read here directly, before any of that conversion. */
export interface RawIdlConstant {
  name: string;
  type: string;
  value: string;
}
export interface RawIdl {
  address: string;
  constants?: RawIdlConstant[];
}

export const IDL_PATH = join(__dirname, "../target/idl/coinflip.json");
export const IDL_RAW = require("../target/idl/coinflip.json") as RawIdl;

function rawConstant(name: string): RawIdlConstant {
  const found = IDL_RAW.constants?.find((c) => c.name === name);
  if (!found) {
    throw new Error(
      `IDL constant "${name}" not found in target/idl/coinflip.json — rebuild the IDL ` +
        "(`anchor idl build`), or this #[constant] was renamed/removed in the program source"
    );
  }
  return found;
}

/** Reads a numeric `#[constant]` straight out of the IDL instead of hardcoding it. */
export function idlConstant(name: string): anchor.BN {
  return new anchor.BN(rawConstant(name).value);
}

/** Same, for a `Pubkey` constant (the IDL stores it as a base58 string). */
export function idlConstantPubkey(name: string): web3.PublicKey {
  return new web3.PublicKey(rawConstant(name).value);
}

/** The program id the IDL was built for. */
export const IDL_ADDRESS = new web3.PublicKey(IDL_RAW.address);
