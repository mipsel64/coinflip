use anchor_lang::prelude::*;

#[constant]
pub const CONFIG_SEED: &[u8] = b"config";

#[constant]
pub const ESCROW_SEED: &[u8] = b"escrow";

/// Hard cap on the protocol fee: 10%.
#[constant]
pub const MAX_FEE_BPS: u16 = 1_000;

#[constant]
pub const BPS_DENOMINATOR: u16 = 10_000;

/// Must exceed ORAO's callback-retry deadline (crate default 9_000 slots ≈ 1h):
/// otherwise a player could sabotage their recorded payout account, block the
/// callback, and force a refund before ORAO falls back to fulfilling without
/// it. ~2 hours at 400ms slots.
#[constant]
pub const MIN_REFUND_TIMEOUT_SLOTS: u64 = 18_000;

/// Upper bound keeps the refund deadline reachable (~46 days).
#[constant]
pub const MAX_REFUND_TIMEOUT_SLOTS: u64 = 10_000_000;
