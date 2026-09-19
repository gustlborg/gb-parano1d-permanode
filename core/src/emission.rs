//! The protocol's emission schedule, mirrored from the node source
//! (`noid_chain::consensus::{emission, development_allocation}`, v1.1.0):
//!
//! - block subsidy 50 NOID at log_slots 24, halved with every state
//!   expansion (`log_slots += 1`), floored at 1 NOID;
//! - during the first three target-time years (heights 1..=4,730,400) the
//!   miner receives 90% of the subsidy, and every 4,320th block additionally
//!   pays each of the two development funds 5% of a day's subsidy;
//! - genesis mints nothing.
//!
//! Everything the node ever issued minus what is in circulation is what
//! fees have burned, so `emitted_up_to` lets the explorer report the total
//! burn since genesis without needing bodies it never had.

pub const MICRONOID_PER_NOID: u64 = 1_000_000;
pub const BASE_REWARD_MICRONOID: u64 = 50 * MICRONOID_PER_NOID;
pub const FLOOR_REWARD_MICRONOID: u64 = MICRONOID_PER_NOID;
pub const LOG_SLOTS_GENESIS: u32 = 24;
pub const BLOCKS_PER_DAY: u64 = 24 * 60 * 60 / 20;
pub const DEVELOPMENT_ALLOCATION_END_HEIGHT: u64 = BLOCKS_PER_DAY * 365 * 3;
const DEVELOPMENT_SHARE_DENOMINATOR: u64 = 20;

/// Subsidy per block at a given state size.
pub fn block_reward(log_slots: u32) -> u64 {
    let halvings = log_slots.saturating_sub(LOG_SLOTS_GENESIS);
    (BASE_REWARD_MICRONOID >> halvings.min(63)).max(FLOOR_REWARD_MICRONOID)
}

pub fn development_allocation_active(height: u64) -> bool {
    height > 0 && height <= DEVELOPMENT_ALLOCATION_END_HEIGHT
}

/// What the miner's coinbase mints at `height`, fees aside.
pub fn miner_subsidy(height: u64, log_slots: u32) -> u64 {
    let subsidy = block_reward(log_slots);
    if development_allocation_active(height) {
        subsidy - 2 * (subsidy / DEVELOPMENT_SHARE_DENOMINATOR)
    } else {
        subsidy
    }
}

/// Development payout minted at `height` on top of the coinbase (both
/// funds together), zero on all but every 4,320th block.
pub fn development_payout(height: u64, log_slots: u32) -> u64 {
    if development_allocation_active(height) && height.is_multiple_of(BLOCKS_PER_DAY) {
        2 * (block_reward(log_slots) / DEVELOPMENT_SHARE_DENOMINATOR) * BLOCKS_PER_DAY
    } else {
        0
    }
}

/// Total µNOID minted for heights 1..=height. `expansions` lists the
/// first height at which log_slots reached 25, 26, ... (ascending); empty
/// while the state has never expanded.
pub fn emitted_up_to(height: u64, expansions: &[u64]) -> u128 {
    let split = emitted_split_up_to(height, expansions);
    split.miner + split.development
}

/// What has been minted for heights 1..=height, by recipient: the miners'
/// coinbases and the development payouts (both funds together, half
/// each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmissionSplit {
    pub miner: u128,
    pub development: u128,
}

pub fn emitted_split_up_to(height: u64, expansions: &[u64]) -> EmissionSplit {
    let mut total = EmissionSplit::default();
    let mut start = 1u64;
    let mut log_slots = LOG_SLOTS_GENESIS;
    let mut bounds: Vec<u64> = expansions.iter().copied().filter(|h| *h <= height).collect();
    bounds.push(height + 1);
    for next in bounds {
        // heights start..next-1 all have this log_slots
        if next > start {
            let range = emitted_range(start, next - 1, log_slots);
            total.miner += range.miner;
            total.development += range.development;
        }
        start = next;
        log_slots += 1;
    }
    total
}

fn emitted_range(from: u64, to: u64, log_slots: u32) -> EmissionSplit {
    if to < from {
        return EmissionSplit::default();
    }
    let subsidy = block_reward(log_slots) as u128;
    let share = (block_reward(log_slots) / DEVELOPMENT_SHARE_DENOMINATOR) as u128;
    let n = (to - from + 1) as u128;
    // blocks inside the development period get 90% to the miner + daily payouts
    let dev_to = to.min(DEVELOPMENT_ALLOCATION_END_HEIGHT);
    let dev_n = if dev_to >= from { (dev_to - from + 1) as u128 } else { 0 };
    let plain_n = n - dev_n;
    let payout_blocks = if dev_to >= from {
        (dev_to / BLOCKS_PER_DAY) - ((from - 1) / BLOCKS_PER_DAY)
    } else {
        0
    } as u128;
    EmissionSplit {
        miner: dev_n * (subsidy - 2 * share) + plain_n * subsidy,
        development: payout_blocks * 2 * share * BLOCKS_PER_DAY as u128,
    }
}

/// The next height at which a development payout is minted after
/// `height`, or `None` once the allocation has ended.
pub fn next_development_payout_height(height: u64) -> Option<u64> {
    let next = (height / BLOCKS_PER_DAY + 1) * BLOCKS_PER_DAY;
    (next <= DEVELOPMENT_ALLOCATION_END_HEIGHT).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_matches_the_node() {
        assert_eq!(block_reward(24), 50_000_000);
        assert_eq!(block_reward(25), 25_000_000);
        assert_eq!(block_reward(30), 1_000_000);
        assert_eq!(block_reward(40), 1_000_000);
        assert_eq!(miner_subsidy(1, 24), 45_000_000);
        assert_eq!(miner_subsidy(DEVELOPMENT_ALLOCATION_END_HEIGHT + 1, 24), 50_000_000);
        assert_eq!(development_payout(4320, 24), 21_600_000_000);
        assert_eq!(development_payout(4321, 24), 0);
    }

    #[test]
    fn emission_sums_block_by_block() {
        let expansions = [10u64];
        for h in [0u64, 1, 5, 9, 10, 11, 4319, 4320, 4321, 8640, 12_345] {
            let mut expected: u128 = 0;
            let mut log_slots = 24;
            for height in 1..=h {
                if expansions.contains(&height) {
                    log_slots += 1;
                }
                expected += (miner_subsidy(height, log_slots) + development_payout(height, log_slots)) as u128;
            }
            assert_eq!(emitted_up_to(h, &expansions), expected, "height {h}");
            let mut expected_plain: u128 = 0;
            for height in 1..=h {
                expected_plain += (miner_subsidy(height, 24) + development_payout(height, 24)) as u128;
            }
            assert_eq!(emitted_up_to(h, &[]), expected_plain, "height {h} without expansions");
        }
    }

    #[test]
    fn split_matches_block_by_block() {
        let mut miner: u128 = 0;
        let mut dev: u128 = 0;
        for height in 1..=9000u64 {
            miner += miner_subsidy(height, 24) as u128;
            dev += development_payout(height, 24) as u128;
        }
        assert_eq!(emitted_split_up_to(9000, &[]), EmissionSplit { miner, development: dev });
        assert_eq!(dev, 2 * 21_600_000_000);
    }

    #[test]
    fn next_payout() {
        assert_eq!(next_development_payout_height(0), Some(4320));
        assert_eq!(next_development_payout_height(4319), Some(4320));
        assert_eq!(next_development_payout_height(4320), Some(8640));
        assert_eq!(next_development_payout_height(DEVELOPMENT_ALLOCATION_END_HEIGHT - 1), Some(DEVELOPMENT_ALLOCATION_END_HEIGHT));
        assert_eq!(next_development_payout_height(DEVELOPMENT_ALLOCATION_END_HEIGHT), None);
    }
}
