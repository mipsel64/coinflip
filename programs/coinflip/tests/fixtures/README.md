# orao_vrf_cb.so

The deployed ORAO VRF Callback program binary, dumped so LiteSVM can execute
the real `request`/`settle_fallback`/`settle_callback` CPI paths instead of
relying on the `orao-solana-vrf-cb` crate's interface-only stubs.

- Program id: `VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU`
- Dumped from **mainnet** on 2026-08-18 via:

  ```bash
  solana program dump VRFCBePmGTpZ234BhbzNNzmyg39Rgdd6VgdfhHwKypU \
    programs/coinflip/tests/fixtures/orao_vrf_cb.so -u m
  ```

- sha256: `6d180b1e91be7d5c541f75e2413146beb047d9beeb76ac8eb1d7c883200c6b8b`

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
