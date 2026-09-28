//! The protocol's emission schedule, mirrored from the node source
//! (`noid_chain::consensus::{emission, development_allocation, forks}`,
//! v2.0.0). Two eras, selected by height alone:
//!
//! - **legacy** (heights below 210,537, 20 s target): block subsidy 50 NOID
//!   at log_slots 24, halved with every state expansion (`log_slots += 1`),
//!   floored at 1 NOID; every 4,320th block pays each development fund 5%
//!   of a day's subsidy;
//! - **v2** (from height 210,537, 30 s target): the subsidy follows the
//!   height alone - 16, 11.30, 8, 5.65, 4, 2.83, 2, 1.41 NOID for one
//!   1,051,200-block interval each, then 1 NOID for ever. State expansion
//!   no longer changes it.
//!
//! The development allocation (90% miner, 5% O(1) Network Fund, 5%
//! Parano1d Lab) lasts three target-time years from genesis. The fork
//! converts the remaining time into 30 s blocks, so it ends at height
//! 3,223,778; v2 pays every 2,880 blocks from 213,416, the last payout
//! covers the final partial interval, and the incomplete legacy day before
//! the fork is never paid. Genesis mints nothing.
//!
//! Everything the node ever issued minus what is in circulation is what
//! fees have burned, so `emitted_up_to` lets the API report the total
//! burn since genesis without needing bodies it never had.

pub const MICRONOID_PER_NOID: u64 = 1_000_000;
pub const BASE_REWARD_MICRONOID: u64 = 50 * MICRONOID_PER_NOID;
pub const FLOOR_REWARD_MICRONOID: u64 = MICRONOID_PER_NOID;
pub const LOG_SLOTS_GENESIS: u32 = 24;

/// Target block interval before the fork, in seconds.
pub const LEGACY_BLOCK_TIME: u64 = 20;
/// Target block interval from the fork on, in seconds.
pub const V2_BLOCK_TIME: u64 = 30;
/// First block governed by the v2 rules.
pub const V2_ACTIVATION_HEIGHT: u64 = 210_537;

/// Target blocks per day before the fork (also the legacy payout interval).
pub const BLOCKS_PER_DAY: u64 = 24 * 60 * 60 / LEGACY_BLOCK_TIME;
/// Target blocks per day from the fork on (also the v2 payout interval).
pub const V2_BLOCKS_PER_DAY: u64 = 24 * 60 * 60 / V2_BLOCK_TIME;

/// One nominal 365-day year at the v2 target; the reward steps down once
/// per interval, counted from the activation block.
pub const V2_REWARD_INTERVAL_BLOCKS: u64 = 365 * V2_BLOCKS_PER_DAY;
/// Exact gross subsidy per v2 interval; the last value repeats for ever.
pub const V2_REWARDS_MICRONOID: [u64; 9] = [
    16_000_000, 11_300_000, 8_000_000, 5_650_000, 4_000_000, 2_830_000, 2_000_000, 1_410_000, 1_000_000,
];

/// Three 365-day years, the development allocation's duration.
pub const DEVELOPMENT_ALLOCATION_DURATION_SECONDS: u64 = 86_400 * 365 * 3;
/// Where the allocation would have ended without the fork (never reached).
pub const LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT: u64 = BLOCKS_PER_DAY * 365 * 3;
/// Last block of the development allocation under the network's schedule:
/// the legacy blocks count 20 s each, the rest of the three years is
/// converted into 30 s blocks (an incomplete final interval is dropped).
pub const DEVELOPMENT_ALLOCATION_END_HEIGHT: u64 = (V2_ACTIVATION_HEIGHT - 1)
    + (DEVELOPMENT_ALLOCATION_DURATION_SECONDS - (V2_ACTIVATION_HEIGHT - 1) * LEGACY_BLOCK_TIME) / V2_BLOCK_TIME;
const DEVELOPMENT_SHARE_DENOMINATOR: u64 = 20;

/// Whether `height` is governed by the v2 rules.
pub const fn v2_active(height: u64) -> bool {
    height >= V2_ACTIVATION_HEIGHT
}

/// Target interval of the block at `height`, in seconds.
pub const fn block_time_at(height: u64) -> u64 {
    if v2_active(height) {
        V2_BLOCK_TIME
    } else {
        LEGACY_BLOCK_TIME
    }
}

/// Target seconds from block `from` to block `to` (the blocks after `from`
/// up to and including `to`), each at its own era's interval.
pub fn target_seconds_between(from: u64, to: u64) -> u64 {
    if to <= from {
        return 0;
    }
    let last_legacy = V2_ACTIVATION_HEIGHT - 1;
    let legacy = to.min(last_legacy).saturating_sub(from);
    let v2 = to - from - legacy;
    legacy * LEGACY_BLOCK_TIME + v2 * V2_BLOCK_TIME
}

/// Legacy subsidy per block at a given state size (heights before the fork).
pub fn block_reward(log_slots: u32) -> u64 {
    let halvings = log_slots.saturating_sub(LOG_SLOTS_GENESIS);
    (BASE_REWARD_MICRONOID >> halvings.min(63)).max(FLOOR_REWARD_MICRONOID)
}

/// The v2 interval (0..=8) of `height`, `None` before the fork.
pub fn v2_tier(height: u64) -> Option<usize> {
    v2_active(height).then(|| {
        ((height - V2_ACTIVATION_HEIGHT) / V2_REWARD_INTERVAL_BLOCKS).min(V2_REWARDS_MICRONOID.len() as u64 - 1) as usize
    })
}

/// First block of v2 interval `tier`.
pub const fn v2_tier_first_height(tier: usize) -> u64 {
    V2_ACTIVATION_HEIGHT + tier as u64 * V2_REWARD_INTERVAL_BLOCKS
}

/// Gross subsidy of the block at `height` (miner plus development share).
/// `log_slots` only matters before the fork.
pub fn block_reward_at(height: u64, log_slots: u32) -> u64 {
    match v2_tier(height) {
        Some(tier) => V2_REWARDS_MICRONOID[tier],
        None => block_reward(log_slots),
    }
}

/// The node's `DevelopmentAllocation` for one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevelopmentAllocation {
    /// The block is inside the three-year allocation period.
    pub active: bool,
    /// The block carries the mandatory two-output payout.
    pub payout_due: bool,
    /// 5% of this block's subsidy, per fund (0 when inactive).
    pub share_each: u64,
    /// Amount of each of the two payout outputs, if due.
    pub payout_each: Option<u64>,
    /// What the primary coinbase may mint, fees aside.
    pub miner_subsidy: u64,
}

/// Mirrors `development_allocation_with_schedule` for the mainnet schedule.
pub fn development_allocation_at(height: u64, log_slots: u32) -> DevelopmentAllocation {
    let subsidy = block_reward_at(height, log_slots);
    let inactive = DevelopmentAllocation {
        active: false,
        payout_due: false,
        share_each: 0,
        payout_each: None,
        miner_subsidy: subsidy,
    };
    let share_each = subsidy / DEVELOPMENT_SHARE_DENOMINATOR;
    if !v2_active(height) {
        // legacy rule: payout on every multiple of 4,320
        if height == 0 || height > LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT {
            return inactive;
        }
        let payout_due = height.is_multiple_of(BLOCKS_PER_DAY);
        return DevelopmentAllocation {
            active: true,
            payout_due,
            share_each,
            payout_each: payout_due.then_some(share_each * BLOCKS_PER_DAY),
            miner_subsidy: subsidy - 2 * share_each,
        };
    }
    if height > DEVELOPMENT_ALLOCATION_END_HEIGHT {
        return inactive;
    }
    let elapsed = height - (V2_ACTIVATION_HEIGHT - 1);
    let count = if height == V2_ACTIVATION_HEIGHT {
        None
    } else if elapsed.is_multiple_of(V2_BLOCKS_PER_DAY) {
        Some(V2_BLOCKS_PER_DAY)
    } else if height == DEVELOPMENT_ALLOCATION_END_HEIGHT {
        Some(elapsed % V2_BLOCKS_PER_DAY)
    } else {
        None
    };
    DevelopmentAllocation {
        active: true,
        payout_due: count.is_some(),
        share_each,
        payout_each: count.map(|blocks| share_each * blocks),
        miner_subsidy: subsidy - 2 * share_each,
    }
}

pub fn development_allocation_active(height: u64) -> bool {
    height > 0 && height <= DEVELOPMENT_ALLOCATION_END_HEIGHT
}

/// What the miner's coinbase mints at `height`, fees aside.
pub fn miner_subsidy(height: u64, log_slots: u32) -> u64 {
    development_allocation_at(height, log_slots).miner_subsidy
}

/// Development payout minted at `height` on top of the coinbase (both
/// funds together), zero on all but the payout blocks.
pub fn development_payout(height: u64, log_slots: u32) -> u64 {
    development_allocation_at(height, log_slots).payout_each.map_or(0, |each| 2 * each)
}

/// Whether the block at `height` carries a development payout (independent
/// of the state size).
pub fn is_development_payout_height(height: u64) -> bool {
    development_allocation_at(height, LOG_SLOTS_GENESIS).payout_due
}

/// The last v2 payout covers a partial interval unless the allocation ends
/// exactly on an interval boundary.
fn final_partial_payout_blocks() -> u64 {
    (DEVELOPMENT_ALLOCATION_END_HEIGHT - (V2_ACTIVATION_HEIGHT - 1)) % V2_BLOCKS_PER_DAY
}

/// Regular (full-interval) v2 payout heights in `lo..=hi`: those
/// `V2_ACTIVATION_HEIGHT - 1 + k * 2,880` with `k >= 1`.
fn v2_regular_payouts_in(lo: u64, hi: u64) -> u64 {
    let base = V2_ACTIVATION_HEIGHT - 1;
    let hi = hi.min(DEVELOPMENT_ALLOCATION_END_HEIGHT);
    let lo = lo.max(V2_ACTIVATION_HEIGHT);
    if hi < lo {
        return 0;
    }
    let k_max = (hi - base) / V2_BLOCKS_PER_DAY;
    let k_min = (lo - base).div_ceil(V2_BLOCKS_PER_DAY);
    (k_max + 1).saturating_sub(k_min)
}

/// Number of blocks in `1..=height` that carry a development payout. Each
/// of them mints two outputs on top of the coinbase.
pub fn development_payout_blocks_up_to(height: u64) -> u64 {
    let legacy = height.min(V2_ACTIVATION_HEIGHT - 1) / BLOCKS_PER_DAY;
    let mut v2 = v2_regular_payouts_in(V2_ACTIVATION_HEIGHT, height);
    if final_partial_payout_blocks() != 0 && height >= DEVELOPMENT_ALLOCATION_END_HEIGHT {
        v2 += 1;
    }
    legacy + v2
}

/// Total µNOID minted for heights 1..=height. `expansions` lists the
/// first height at which log_slots reached 25, 26, ... (ascending); empty
/// while the state has never expanded. Expansions only matter before the
/// fork.
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
    let mut total = emitted_legacy_up_to(height.min(V2_ACTIVATION_HEIGHT - 1), expansions);
    if v2_active(height) {
        let v2 = emitted_v2_between(V2_ACTIVATION_HEIGHT, height);
        total.miner += v2.miner;
        total.development += v2.development;
    }
    total
}

fn emitted_legacy_up_to(height: u64, expansions: &[u64]) -> EmissionSplit {
    let mut total = EmissionSplit::default();
    let mut start = 1u64;
    let mut log_slots = LOG_SLOTS_GENESIS;
    let mut bounds: Vec<u64> = expansions.iter().copied().filter(|h| *h <= height).collect();
    bounds.push(height + 1);
    for next in bounds {
        // heights start..next-1 all have this log_slots
        if next > start {
            let range = emitted_legacy_range(start, next - 1, log_slots);
            total.miner += range.miner;
            total.development += range.development;
        }
        start = next;
        log_slots += 1;
    }
    total
}

/// Legacy heights `from..=to` (all inside the allocation period, which the
/// fork ends long before its legacy end).
fn emitted_legacy_range(from: u64, to: u64, log_slots: u32) -> EmissionSplit {
    if to < from {
        return EmissionSplit::default();
    }
    let subsidy = block_reward(log_slots) as u128;
    let share = (block_reward(log_slots) / DEVELOPMENT_SHARE_DENOMINATOR) as u128;
    let n = (to - from + 1) as u128;
    let payout_blocks = ((to / BLOCKS_PER_DAY) - ((from - 1) / BLOCKS_PER_DAY)) as u128;
    EmissionSplit {
        miner: n * (subsidy - 2 * share),
        development: payout_blocks * 2 * share * BLOCKS_PER_DAY as u128,
    }
}

/// v2 heights `from..=to` (both at or after the fork).
fn emitted_v2_between(from: u64, to: u64) -> EmissionSplit {
    let mut total = EmissionSplit::default();
    if to < from {
        return total;
    }
    for tier in 0..V2_REWARDS_MICRONOID.len() {
        let first = v2_tier_first_height(tier).max(from);
        let last = if tier + 1 == V2_REWARDS_MICRONOID.len() {
            to
        } else {
            (v2_tier_first_height(tier + 1) - 1).min(to)
        };
        if last < first {
            continue;
        }
        let reward = V2_REWARDS_MICRONOID[tier] as u128;
        let share = (V2_REWARDS_MICRONOID[tier] / DEVELOPMENT_SHARE_DENOMINATOR) as u128;
        let dev_last = last.min(DEVELOPMENT_ALLOCATION_END_HEIGHT);
        let dev_blocks = if dev_last >= first { (dev_last - first + 1) as u128 } else { 0 };
        let plain_blocks = (last - first + 1) as u128 - dev_blocks;
        total.miner += dev_blocks * (reward - 2 * share) + plain_blocks * reward;
        // Tier boundaries fall on payout boundaries (1,051,200 = 365 x
        // 2,880), so every regular payout of this range uses this tier.
        total.development += v2_regular_payouts_in(first, last) as u128 * 2 * share * V2_BLOCKS_PER_DAY as u128;
        let partial = final_partial_payout_blocks();
        if partial != 0 && (first..=last).contains(&DEVELOPMENT_ALLOCATION_END_HEIGHT) {
            total.development += 2 * share * partial as u128;
        }
    }
    total
}

/// The next height at which a development payout is minted after
/// `height`, or `None` once the allocation has ended.
pub fn next_development_payout_height(height: u64) -> Option<u64> {
    if height < V2_ACTIVATION_HEIGHT - 1 {
        let next = (height / BLOCKS_PER_DAY + 1) * BLOCKS_PER_DAY;
        if next < V2_ACTIVATION_HEIGHT {
            return Some(next);
        }
    }
    let base = V2_ACTIVATION_HEIGHT - 1;
    let from = height.max(base);
    let next = base + ((from - base) / V2_BLOCKS_PER_DAY + 1) * V2_BLOCKS_PER_DAY;
    if next <= DEVELOPMENT_ALLOCATION_END_HEIGHT {
        Some(next)
    } else if height < DEVELOPMENT_ALLOCATION_END_HEIGHT && final_partial_payout_blocks() != 0 {
        Some(DEVELOPMENT_ALLOCATION_END_HEIGHT)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_published_schedule() {
        assert_eq!(V2_REWARD_INTERVAL_BLOCKS, 1_051_200);
        assert_eq!(DEVELOPMENT_ALLOCATION_END_HEIGHT, 3_223_778);
        assert_eq!(LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT, 4_730_400);
        assert_eq!(final_partial_payout_blocks(), 762);
        assert_eq!(v2_tier_first_height(8), 8_620_137);
        let eight: u64 = V2_REWARDS_MICRONOID[..8].iter().map(|r| r * V2_REWARD_INTERVAL_BLOCKS).sum();
        assert_eq!(eight, 53_810_928 * MICRONOID_PER_NOID);
        for reward in V2_REWARDS_MICRONOID {
            assert!(reward.is_multiple_of(DEVELOPMENT_SHARE_DENOMINATOR));
        }
    }

    #[test]
    fn legacy_schedule_matches_the_node() {
        assert_eq!(block_reward(24), 50_000_000);
        assert_eq!(block_reward(25), 25_000_000);
        assert_eq!(block_reward(30), 1_000_000);
        assert_eq!(block_reward(40), 1_000_000);
        assert_eq!(miner_subsidy(1, 24), 45_000_000);
        assert_eq!(development_payout(4320, 24), 21_600_000_000);
        assert_eq!(development_payout(4321, 24), 0);
        assert_eq!(development_payout(207_360, 24), 21_600_000_000);
        assert_eq!(block_reward_at(V2_ACTIVATION_HEIGHT - 1, 25), 25_000_000);
    }

    #[test]
    fn v2_edges() {
        let h = V2_ACTIVATION_HEIGHT;
        assert_eq!(block_reward_at(h, 24), 16_000_000);
        assert_eq!(block_reward_at(h, 30), 16_000_000);
        assert_eq!(miner_subsidy(h, 24), 14_400_000);
        // the incomplete legacy day is never paid, no payout on H itself
        assert_eq!(development_payout(h, 24), 0);
        assert_eq!(development_payout(h + 2878, 24), 0);
        assert_eq!(development_payout(h + 2879, 24), 2 * 2_304_000_000);
        assert_eq!(development_payout(h + 2880, 24), 0);
        assert_eq!(block_reward_at(v2_tier_first_height(1) - 1, 24), 16_000_000);
        assert_eq!(block_reward_at(v2_tier_first_height(1), 24), 11_300_000);
        assert_eq!(development_payout(v2_tier_first_height(1) - 1, 24), 2 * 2_304_000_000);
        assert_eq!(development_payout(v2_tier_first_height(1) + 2879, 24), 2 * 565_000 * 2880);
        let end = DEVELOPMENT_ALLOCATION_END_HEIGHT;
        assert_eq!(development_payout(end, 24), 2 * 762 * 400_000);
        assert_eq!(miner_subsidy(end, 24), 7_200_000);
        assert_eq!(miner_subsidy(end + 1, 24), 8_000_000);
        assert!(!development_allocation_at(end + 1, 24).active);
        assert_eq!(block_reward_at(u64::MAX, 24), 1_000_000);
    }

    #[test]
    fn next_payout_crosses_the_fork() {
        assert_eq!(next_development_payout_height(0), Some(4320));
        assert_eq!(next_development_payout_height(4319), Some(4320));
        assert_eq!(next_development_payout_height(4320), Some(8640));
        assert_eq!(next_development_payout_height(207_359), Some(207_360));
        // 211,680 would have been the next legacy payout; v2 pays at 213,416
        assert_eq!(next_development_payout_height(207_360), Some(213_416));
        assert_eq!(next_development_payout_height(213_415), Some(213_416));
        assert_eq!(next_development_payout_height(213_416), Some(216_296));
        let end = DEVELOPMENT_ALLOCATION_END_HEIGHT;
        assert_eq!(next_development_payout_height(end - 1), Some(end));
        assert_eq!(next_development_payout_height(end), None);
    }

    #[test]
    fn target_time_is_height_aware() {
        let h = V2_ACTIVATION_HEIGHT;
        assert_eq!(target_seconds_between(0, 10), 200);
        assert_eq!(target_seconds_between(h - 1, h), 30);
        assert_eq!(target_seconds_between(h - 2, h), 50);
        assert_eq!(target_seconds_between(0, DEVELOPMENT_ALLOCATION_END_HEIGHT), 94_607_980);
        assert_eq!(target_seconds_between(5, 5), 0);
    }

    /// One block-by-block pass across the fork, every tier boundary up to
    /// the third and the end of the allocation, against the closed forms.
    #[test]
    fn closed_forms_match_block_by_block() {
        let expansions = [150_000u64];
        let limit = v2_tier_first_height(3) + 5_000;
        let mut checkpoints: Vec<u64> = vec![1, 4319, 4320, 4321, 149_999, 150_000, 150_001, 207_360, 210_535];
        let h = V2_ACTIVATION_HEIGHT;
        let end = DEVELOPMENT_ALLOCATION_END_HEIGHT;
        checkpoints.extend([h - 1, h, h + 1, h + 2878, h + 2879, h + 2880, end - 1, end, end + 1, limit]);
        for tier in 1..=3 {
            let t = v2_tier_first_height(tier);
            checkpoints.extend([t - 2881, t - 1, t, t + 1, t + 2879]);
        }
        checkpoints.sort_unstable();
        let mut next = 0;
        let mut miner: u128 = 0;
        let mut dev: u128 = 0;
        let mut payout_blocks = 0u64;
        let mut log_slots = LOG_SLOTS_GENESIS;
        for height in 1..=limit {
            if expansions.contains(&height) {
                log_slots += 1;
            }
            let a = development_allocation_at(height, log_slots);
            miner += a.miner_subsidy as u128;
            dev += a.payout_each.map_or(0, |e| 2 * e as u128);
            payout_blocks += a.payout_due as u64;
            while next < checkpoints.len() && checkpoints[next] == height {
                assert_eq!(
                    emitted_split_up_to(height, &expansions),
                    EmissionSplit { miner, development: dev },
                    "height {height}"
                );
                assert_eq!(development_payout_blocks_up_to(height), payout_blocks, "payout blocks {height}");
                next += 1;
            }
        }
        assert_eq!(next, checkpoints.len());
        // v2 development total: 1,799,224.8 NOID per fund
        let v2_dev = emitted_split_up_to(limit, &expansions).development - emitted_split_up_to(h - 1, &expansions).development;
        assert_eq!(v2_dev, 2 * 1_799_224_800_000);
    }
}
