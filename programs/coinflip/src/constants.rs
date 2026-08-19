use anchor_lang::prelude::*;

#[constant]
pub const CONFIG_SEED: &[u8] = b"config";

#[constant]
pub const ESCROW_SEED: &[u8] = b"escrow";

/// Fee destination authority, re-exported from `crate::treasury` so clients and
/// indexers read it out of the IDL instead of hardcoding it. Compile-time: an
/// IDL built with the `local` feature carries the test key, not the real one.
#[constant]
pub const TREASURY: Pubkey = crate::treasury::ID;

/// Hard cap on the protocol fee: 10%.
#[constant]
pub const MAX_FEE_BPS: u16 = 1_000;

#[constant]
pub const BPS_DENOMINATOR: u16 = 10_000;

/// Floor on the refund window: ~10 minutes at 400ms slots. ORAO's oracle
/// quorum answers a request in seconds, and the crank settles as soon as it
/// does, so this covers a fulfillment outage with a wide margin while keeping
/// the escape hatch reachable. There is no oracle-side deadline to clear
/// anymore — plain VRF has no callback and no retry window.
#[constant]
pub const MIN_REFUND_TIMEOUT_SLOTS: u64 = 1_500;

/// Upper bound keeps the refund deadline reachable (~46 days).
#[constant]
pub const MAX_REFUND_TIMEOUT_SLOTS: u64 = 10_000_000;
