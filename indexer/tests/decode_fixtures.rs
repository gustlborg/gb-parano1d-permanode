//! Fixture-based tests for the getBlock fallback decoder
//! (`indexer/src/decode.rs`). Fixtures are real RPC responses captured
//! live against a running mainnet node - see
//! `docs/ANLEITUNG-getblock-decoder.md` section 2 for how they were made.

use parano1d_permanode_indexer::decode::decode_retained_block;
use parano1d_permanode_indexer::rpc::BlockDetailsInfo;
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read_json(name: &str) -> Value {
    let raw = std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn read_block_bytes(name: &str) -> Vec<u8> {
    let hex_str = read_json(name).as_str().expect("getBlock fixture is a hex string").to_string();
    hex::decode(hex_str).expect("valid hex")
}

/// Fixtures whose block has full `retained` data via getBlockDetails - the
/// decoder's output must match that field for field, after removing the
/// display-only fields RetainedBlockInfo doesn't have.
fn check_against_details(base: &str) {
    let bytes = read_block_bytes(&format!("{base}.getBlock.json"));
    let details_json = read_json(&format!("{base}.getBlockDetails.json"));
    let details: BlockDetailsInfo = serde_json::from_value(details_json.clone())
        .unwrap_or_else(|e| panic!("{base}: parse getBlockDetails: {e}"));
    let height = details.header.height;
    let hash = details.header.hash.clone();

    let decoded = decode_retained_block(&bytes, height, &hash)
        .unwrap_or_else(|e| panic!("{base}: decode failed: {e:#}"));
    let mine = serde_json::to_value(&decoded).unwrap();

    let mut expected = details_json["retained"].clone();
    for extra in ["reward_noid", "history_step_bytes", "bundle_bytes"] {
        expected.as_object_mut().unwrap().remove(extra);
    }

    assert_eq!(mine, expected, "{base}: decoded output does not match getBlockDetails.retained");
}

#[test]
fn block_108552_three_single_page_txs() {
    check_against_details("block_108552");
}

#[test]
fn block_108569_multi_page_tx() {
    check_against_details("block_108569");
}

#[test]
fn block_108574_coinbase_only() {
    check_against_details("block_108574");
}

#[test]
fn block_108537_marker_matches_precomputed_expectation() {
    let bytes = read_block_bytes("block_108537_marker.getBlock.json");
    let header = read_json("block_108537_marker.getBlockHeader.json");
    let height = header["height"].as_u64().unwrap();
    let hash = header["hash"].as_str().unwrap();

    // Sanity: this fixture really is a marker block (retained: null) -
    // otherwise the test would not exercise the fallback path at all.
    let details = read_json("block_108537_marker.getBlockDetails.json");
    assert!(details["retained"].is_null(), "fixture is not actually a marker block");

    let decoded = decode_retained_block(&bytes, height, hash).expect("decode marker block");
    let mine = serde_json::to_value(&decoded).unwrap();
    let expected = read_json("block_108537_marker.decoded.expected.json");
    assert_eq!(mine, expected);
}

#[test]
fn block_108537_marker_rejects_wrong_hash() {
    let bytes = read_block_bytes("block_108537_marker.getBlock.json");
    let header = read_json("block_108537_marker.getBlockHeader.json");
    let height = header["height"].as_u64().unwrap();

    let err = decode_retained_block(&bytes, height, "0000000000000000000000000000000000000000000000000000000000000000000000000000")
        .expect_err("wrong hash must be rejected");
    assert!(err.to_string().contains("hash"), "unexpected error: {err}");
}

#[test]
fn block_108537_marker_rejects_wrong_height() {
    let bytes = read_block_bytes("block_108537_marker.getBlock.json");
    let header = read_json("block_108537_marker.getBlockHeader.json");
    let hash = header["hash"].as_str().unwrap();

    let err = decode_retained_block(&bytes, 1, hash).expect_err("wrong height must be rejected");
    assert!(err.to_string().contains("height"), "unexpected error: {err}");
}
