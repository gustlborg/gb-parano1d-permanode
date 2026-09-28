//! The permanode mirrors a few consensus rules in `permanode_core` (which
//! does not link the node). These tests hold the mirror against the node's
//! own v2.0.0 code at every boundary that matters, and check the contract
//! detection on real contract pages built with the node's templates.

use noid_chain::consensus::{development_allocation, emission, forks::ACTIVE_SCHEDULE, params};
use noid_poseidon2b::primitives::Address;
use noid_tx::experimental_object::applications;
use noid_tx::{output_bitmap_bit, Transaction, TxBody, TxInput, TxOutput, TX_INPUTS, TX_OUTPUTS};
use parano1d_permanode::decode;
use permanode_core::emission as mirror;

#[test]
fn constants_match_the_node() {
    assert_eq!(mirror::V2_ACTIVATION_HEIGHT, params::MAINNET_V2_ACTIVATION_HEIGHT);
    assert_eq!(params::V2_ACTIVATION_HEIGHT, Some(mirror::V2_ACTIVATION_HEIGHT));
    assert_eq!(mirror::LEGACY_BLOCK_TIME, params::BLOCK_TIME);
    assert_eq!(mirror::V2_BLOCK_TIME, params::V2_BLOCK_TIME);
    assert_eq!(mirror::V2_REWARD_INTERVAL_BLOCKS, emission::V2_REWARD_INTERVAL_BLOCKS);
    assert_eq!(mirror::V2_REWARDS_MICRONOID, emission::V2_REWARDS_MICRONOID);
    assert_eq!(
        mirror::DEVELOPMENT_ALLOCATION_END_HEIGHT,
        development_allocation::development_allocation_end_height_with_schedule(ACTIVE_SCHEDULE)
    );
    assert_eq!(mirror::LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT, development_allocation::DEVELOPMENT_ALLOCATION_END_HEIGHT);
}

fn interesting_heights() -> Vec<u64> {
    let h = mirror::V2_ACTIVATION_HEIGHT;
    let end = mirror::DEVELOPMENT_ALLOCATION_END_HEIGHT;
    let mut heights = vec![0, 1, 2, 4319, 4320, 4321, 95_124, 95_125, 207_359, 207_360, 207_361];
    heights.extend([h - 2, h - 1, h, h + 1, h + 2878, h + 2879, h + 2880, h + 5759]);
    heights.extend([end - 2880, end - 1, end, end + 1]);
    for tier in 1..mirror::V2_REWARDS_MICRONOID.len() {
        let t = mirror::v2_tier_first_height(tier);
        heights.extend([t - 2880, t - 1, t, t + 1, t + 2879]);
    }
    // a coarse sweep in between
    heights.extend((0..=10_000_000u64).step_by(9_973));
    heights.push(u64::MAX / 2);
    heights
}

#[test]
fn rewards_and_allocation_match_the_node() {
    for height in interesting_heights() {
        for log_slots in 24..=32u32 {
            assert_eq!(
                mirror::block_reward_at(height, log_slots),
                emission::block_reward_at_height(height, log_slots),
                "reward {height} {log_slots}"
            );
            let node = development_allocation::development_allocation_at_height(height, log_slots).unwrap();
            let ours = mirror::development_allocation_at(height, log_slots);
            assert_eq!(ours.active, node.active, "active {height}");
            assert_eq!(ours.payout_due, node.payout_due, "payout_due {height}");
            assert_eq!(ours.share_each, node.share_each, "share {height}");
            assert_eq!(ours.payout_each, node.payout_each, "payout {height}");
            assert_eq!(ours.miner_subsidy, node.miner_subsidy, "miner {height}");
        }
    }
}

#[test]
fn target_time_matches_the_node() {
    for to in interesting_heights().into_iter().filter(|h| *h < 20_000_000) {
        for from in [0u64, 1, 207_360, mirror::V2_ACTIVATION_HEIGHT - 1, mirror::V2_ACTIVATION_HEIGHT + 17] {
            if from <= to {
                assert_eq!(
                    mirror::target_seconds_between(from, to) as u128,
                    ACTIVE_SCHEDULE.ideal_elapsed(from, to),
                    "{from}..{to}"
                );
            }
        }
    }
}

fn coinbase() -> Transaction {
    let mut outputs = [TxOutput::dummy(); TX_OUTPUTS];
    outputs[0] = TxOutput { slot_index: 1, amount: 14_400_000, owner: Address([9; 32]) };
    Transaction::new(TxBody {
        epoch_anchor: [1; 32],
        fee: 0,
        input_owner: Address([0; 32]),
        inputs: [TxInput::dummy(); TX_INPUTS],
        outputs,
        validity_bitmap: output_bitmap_bit(0),
        is_coinbase: true,
    })
}

fn payment(slot: u32) -> Transaction {
    let mut inputs = [TxInput::dummy(); TX_INPUTS];
    inputs[0] = TxInput { slot_index: slot, amount: 1_000_000, creation_id: u64::from(slot) };
    let mut outputs = [TxOutput::dummy(); TX_OUTPUTS];
    outputs[0] = TxOutput { slot_index: slot + 100, amount: 990_000, owner: Address([5; 32]) };
    Transaction::new(TxBody {
        epoch_anchor: [3; 32],
        fee: 10_000,
        input_owner: Address([4; 32]),
        inputs,
        outputs,
        validity_bitmap: 1 | output_bitmap_bit(0) | noid_tx::PAGED_SPEND_START_BIT | noid_tx::PAGED_SPEND_END_BIT,
        is_coinbase: false,
    })
}

#[test]
fn contract_calls_are_detected_by_position() {
    let height = mirror::V2_ACTIVATION_HEIGHT + 5;
    let input = |slot: u32| TxInput { slot_index: slot, amount: 50_000, creation_id: u64::from(slot) };
    // closes: the payee collects before expiry
    let refund = applications::refundable_payment(Address([1; 32]), Address([2; 32]), height + 100, 1_000);
    let close = refund.build_call(input(10), 20, 500, [3; 32], height, true).unwrap();
    // continues: an allowance wallet pays out and keeps its successor
    let allowance = applications::allowance_wallet(Address([6; 32]), Address([7; 32]), None, height + 100, 1_000, 20_000, 0);
    let payout = TxOutput { slot_index: 22, amount: 10_000, owner: Address([8; 32]) };
    let call = allowance.build_payment(input(11), 21, 500, [3; 32], height, payout).unwrap();

    // v2 puts the contract calls first, then ordinary payments
    let txs = vec![coinbase(), Transaction::new(close.body), Transaction::new(call.body), payment(12)];
    let flags = decode::contract_flags_of(&txs).unwrap();
    assert_eq!(flags, vec![(1, decode::CONTRACT_CALL | decode::CONTRACT_CLOSE), (2, decode::CONTRACT_CALL)]);

    // a block without calls has none
    assert!(decode::contract_flags_of(&[coinbase(), payment(12)]).unwrap().is_empty());
}
