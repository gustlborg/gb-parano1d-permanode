//! The consensus fee model, mirrored so the explorer can split a paid fee
//! into what the miner claimed and what consensus burned.
//!
//! Required fee = base + per-input + per-output + state-growth, where the
//! state-growth component is charged on `max(0, outputs - inputs)` net-new
//! live UTXO slots, scaled by the occupancy of the state domain, and burned:
//! the coinbase may claim only `fee - state_growth`. Occupancy is taken
//! from the header of the block's *parent*, which is what the node checks
//! the coinbase against.

/// Fixed anti-DoS component per non-coinbase transaction (µNOID).
pub const MIN_FEE_BASE: u64 = 5_000;
/// Per live input (µNOID).
pub const FEE_PER_INPUT: u64 = 100;
/// Per live output (µNOID).
pub const FEE_PER_OUTPUT: u64 = 700;
/// Per net-new live slot at low occupancy (µNOID); multiplied under
/// state pressure.
pub const STATE_GROWTH_FEE_BASE: u64 = 2_500;

/// Occupancy thresholds of the pressure multiplier, in basis points.
pub const PRESSURE_LOW_BPS: u64 = 5_000;
pub const PRESSURE_HIGH_BPS: u64 = 7_500;
pub const PRESSURE_EXTREME_BPS: u64 = 9_000;

/// Occupancy of the state domain in basis points, integer arithmetic as
/// in the node.
pub fn occupancy_bps(active_slot_count: u64, log_slots: u32) -> u64 {
    let capacity = 1u128.checked_shl(log_slots).unwrap_or(u128::MAX).max(1);
    ((active_slot_count as u128).saturating_mul(10_000) / capacity) as u64
}

/// 1x below 50 % occupancy, 2x from 50 %, 4x from 75 %, 8x from 90 %.
pub fn pressure_multiplier(active_slot_count: u64, log_slots: u32) -> u64 {
    match occupancy_bps(active_slot_count, log_slots) {
        bps if bps >= PRESSURE_EXTREME_BPS => 8,
        bps if bps >= PRESSURE_HIGH_BPS => 4,
        bps if bps >= PRESSURE_LOW_BPS => 2,
        _ => 1,
    }
}

/// Burn per net-new live slot at the given occupancy (µNOID).
pub fn state_growth_fee_per_slot(active_slot_count: u64, log_slots: u32) -> u64 {
    STATE_GROWTH_FEE_BASE.saturating_mul(pressure_multiplier(active_slot_count, log_slots))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FeeBreakdown {
    pub base: u64,
    pub input: u64,
    pub output: u64,
    pub net_new_slots: u64,
    pub multiplier: u64,
    /// Occupancy the multiplier was derived from, in basis points.
    pub occupancy_bps: u64,
    /// The state-growth component: burned by consensus.
    pub burned: u64,
    /// Minimum fee the transaction had to pay.
    pub required_total: u64,
    /// What it actually paid above the minimum; the miner keeps it.
    pub tip: u64,
    /// `fee - burned`: everything the coinbase may claim.
    pub to_miner: u64,
}

/// Split `fee` (as paid) for a transaction with `n_inputs` live inputs and
/// `n_outputs` live outputs, at the parent's occupancy.
pub fn fee_breakdown(fee: u64, n_inputs: u64, n_outputs: u64, active_slot_count: u64, log_slots: u32) -> FeeBreakdown {
    let net_new_slots = n_outputs.saturating_sub(n_inputs);
    let base = MIN_FEE_BASE;
    let input = FEE_PER_INPUT.saturating_mul(n_inputs);
    let output = FEE_PER_OUTPUT.saturating_mul(n_outputs);
    let multiplier = pressure_multiplier(active_slot_count, log_slots);
    let burned = STATE_GROWTH_FEE_BASE.saturating_mul(multiplier).saturating_mul(net_new_slots);
    let required_total = base.saturating_add(input).saturating_add(output).saturating_add(burned);
    FeeBreakdown {
        base,
        input,
        output,
        net_new_slots,
        multiplier,
        occupancy_bps: occupancy_bps(active_slot_count, log_slots),
        burned,
        required_total,
        tip: fee.saturating_sub(required_total),
        to_miner: fee.saturating_sub(burned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP_24: u64 = 1 << 24;

    #[test]
    fn one_in_two_out_by_pressure_tier() {
        // Fixed part 5 000 + 100 + 2 x 700 = 6 500, plus one net-new slot.
        for (active, total, burn) in [
            (0, 9_000, 2_500),
            (CAP_24 / 2 - 1, 9_000, 2_500),
            (CAP_24 / 2, 11_500, 5_000),
            (CAP_24 / 4 * 3, 16_500, 10_000),
            ((CAP_24 * 9).div_ceil(10), 26_500, 20_000),
        ] {
            let b = fee_breakdown(total, 1, 2, active, 24);
            assert_eq!(b.required_total, total, "active {active}");
            assert_eq!(b.burned, burn);
            assert_eq!(b.tip, 0);
            assert_eq!(b.to_miner, total - burn);
        }
    }

    #[test]
    fn no_growth_no_burn() {
        let b = fee_breakdown(5_800, 1, 1, 0, 24);
        assert_eq!(b.required_total, 5_800);
        assert_eq!(b.burned, 0);
        // consolidation, overpaid: 5 000 + 300 + 1 400 = 6 700, tip 1 400
        let b = fee_breakdown(8_100, 3, 2, 0, 24);
        assert_eq!((b.required_total, b.burned, b.tip, b.to_miner), (6_700, 0, 1_400, 8_100));
    }

    #[test]
    fn overpaid_send_keeps_burn_fixed() {
        // block 117410 on chain: 1 -> 2 paying 9 900
        let b = fee_breakdown(9_900, 1, 2, 49_000, 24);
        assert_eq!((b.burned, b.tip, b.to_miner, b.multiplier), (2_500, 900, 7_400, 1));
    }

    #[test]
    fn multiplier_thresholds_are_inclusive() {
        assert_eq!(pressure_multiplier(CAP_24 / 2 - 1, 24), 1);
        assert_eq!(pressure_multiplier(CAP_24 / 2, 24), 2);
        assert_eq!(pressure_multiplier(CAP_24 / 4 * 3, 24), 4);
        // 90 % of 2^24 is not an integer: 15 099 494 is still 8 999 bps
        assert_eq!(pressure_multiplier(CAP_24 * 9 / 10, 24), 4);
        assert_eq!(pressure_multiplier((CAP_24 * 9).div_ceil(10), 24), 8);
        // after an expansion the same set is half as crowded
        assert_eq!(pressure_multiplier(CAP_24 / 4 * 3, 25), 1);
    }
}
