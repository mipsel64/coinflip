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
