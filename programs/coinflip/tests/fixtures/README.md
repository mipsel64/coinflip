# orao_vrf.so

The deployed ORAO VRF program binary, dumped so LiteSVM can execute the real
`request_v2` CPI path instead of relying on the `orao-solana-vrf` crate's
interface-only stubs.

- Program id: `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`
- Dumped from **mainnet** on 2026-08-19 via:

  ```bash
  solana program dump VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y \
    programs/coinflip/tests/fixtures/orao_vrf.so -u m
  ```

- sha256: `f7603b1a993e7853a209a4d04e93e7b05cb41264580bb62030be0bd36889fafe`

Replaces `orao_vrf_cb.so` (the Callback VRF, program
`VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU`), deleted when the program moved
to plain VRF + crank settlement.

The harness crafts ORAO's `NetworkState` itself (`tests/common/mod.rs`'s
`setup_orao`) — the mainnet one lives at
`5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK`, is 472 bytes (`8 + 464`, what
the fixture allocates too), and charged a 500_000-lamport request fee when this
was written. `fulfill_v2` cannot run locally (it needs the oracle quorum's
ed25519 signatures), so fulfilled requests are written directly into the SVM.

If ORAO upgrades this program, re-dump it with the command above and update
the sha256 in this file so the fixture's provenance stays verifiable.

# treasury-local.json

The treasury the program uses when built with `--features local`, i.e. the fee
destination the e2e suite derives every treasury ATA against. Pubkey:
`9wR75bCR1bo68BygzHkgJ3N735u5TmGsVhzjRrFzNUtJ`.

**This secret key is committed on purpose.** It is a throwaway generated with
`solana-keygen new`, never funded, and never the deployed treasury — the real
one (`BUs86uMPdNMJ9SiFijb4TABpFduhaEqqESs96pTGadsN`) is a compile-time constant
in non-`local` builds and its keypair lives outside the repo. Committing this
one keeps the fee destination stable across machines and CI; the harness reads
the pubkey out of it (`tests/common/mod.rs`'s `treasury()`) rather than
hardcoding it a second time.

If it is ever regenerated, update `treasury::ID`'s `local` variant in
`src/lib.rs` to match — the harness fails the run with instructions if the
loaded `.so` doesn't embed this key.
