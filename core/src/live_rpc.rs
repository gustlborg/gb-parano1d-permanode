//! Minimal read-only client for node RPC methods whose answers are never
//! persisted: mempool contents (transient by nature) and other live-only
//! figures like the network's true current UTXO count, which this
//! permanode's own database cannot reconstruct from history it hasn't
//! recorded. The API proxies these straight from the node on request.

use anyhow::{bail, Context, Result};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Mainnet launch value, docs/protocol/parameters.md: "Target block
/// interval | 20 seconds". Only used to turn a PoW target into an
/// estimated network hashrate; if this ever changes on-chain the estimate
/// would need the same adjustment the network's own difficulty retarget
/// already accounts for.
const TARGET_BLOCK_INTERVAL_SECONDS: f64 = 20.0;

#[derive(Clone)]
pub struct RpcClient {
    url: String,
    agent: ureq::Agent,
}

impl RpcClient {
    pub fn new(url: String) -> Self {
        // ureq has no timeouts by default; without one a hung node would
        // pin every request that touches it. Loopback calls take
        // milliseconds, so anything past this is a stuck node.
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(5)))
            .timeout_global(Some(std::time::Duration::from_secs(20)))
            .build();
        Self {
            url,
            agent: config.new_agent(),
        }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut resp = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("RPC call {method} failed to send"))?;
        let v: serde_json::Value = resp
            .body_mut()
            .read_json()
            .with_context(|| format!("RPC call {method}: invalid JSON response"))?;
        if let Some(err) = v.get("error") {
            bail!("RPC call {method} returned error: {err}");
        }
        v.get("result")
            .cloned()
            .with_context(|| format!("RPC call {method}: response has no result field"))
    }

    pub fn get_mempool_info(&self) -> Result<MempoolInfo> {
        let v = self.call("paranoid_getMempoolInfo", json!([]))?;
        Ok(serde_json::from_value(v)?)
    }

    /// One pending transaction in full (`None` once it left the mempool).
    /// Only this call tells a v2 contract call from a payment: it carries
    /// the call's public opening.
    pub fn get_mempool_entry(&self, tx_hash: &str) -> Result<Option<MempoolEntryInfo>> {
        let v = self.call("paranoid_getMempoolEntry", json!([tx_hash]))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(v)?)
    }

    /// The node's own count of currently-live (unspent) slots across the
    /// entire chain, tracked natively since genesis. There is no RPC to
    /// list them all - only this aggregate count, or per-address/per-index
    /// lookups - so this permanode's own "how many UTXOs have I recorded
    /// as unspent" figure (queries::ChainStats::live_utxos) will be far
    /// smaller than this for as long as it has only been recording a
    /// fraction of the chain's lifetime.
    pub fn get_active_slot_count(&self) -> Result<u64> {
        let v = self.call("paranoid_getActiveSlotCount", json!([]))?;
        v.as_u64().context("getActiveSlotCount: result is not u64")
    }

    pub fn get_chain_info(&self) -> Result<ChainInfo> {
        let v = self.call("paranoid_getChainInfo", json!([]))?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn get_mining_info(&self) -> Result<MiningInfo> {
        let v = self.call("paranoid_getMiningInfo", json!([]))?;
        Ok(serde_json::from_value(v)?)
    }

    /// Permanent block header by height (`None` above the tip).
    pub fn get_block_header(&self, height: u64) -> Result<Option<BlockHeaderInfo>> {
        let v = self.call("paranoid_getBlockHeader", json!([height]))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(v)?)
    }

    /// Live State dimensions: capacity, fill and how many more live slots
    /// until the state expands - which is also when the block reward halves.
    pub fn get_state_info(&self) -> Result<StateInfo> {
        let v = self.call("paranoid_getStateInfo", json!([]))?;
        Ok(serde_json::from_value(v)?)
    }

    /// Every currently-live (unspent) slot owned by `address`, read
    /// straight from the node's Live State - not reconstructed from
    /// transaction history at all, so it's correct regardless of whether
    /// this permanode has recorded any of the address's activity. This is
    /// how third-party explorers can show a correct balance for an address
    /// whose transactions they never archived either: current state and
    /// historical transaction log are two different things on this chain,
    /// and only the latter is short-lived.
    pub fn get_slots_by_owner(&self, address: &str) -> Result<Vec<SlotInfo>> {
        let v = self.call("paranoid_getSlotsByOwner", json!([address]))?;
        Ok(serde_json::from_value(v)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slot_index: u64,
    pub value: u64,
    pub creation_id: u64,
    pub owner: String,
    pub empty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    pub height: u64,
    pub best_hash: String,
    pub difficulty_target: String,
    pub active_slot_count: u64,
    pub log_slots: u32,
    pub circulating_supply_micronoid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeaderInfo {
    pub height: u64,
    pub hash: String,
    pub timestamp: u64,
    pub log_slots: u32,
    pub active_slot_count: u64,
    pub alloc_counter: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateInfo {
    pub log_slots: u32,
    pub capacity: u64,
    pub active_slots: u64,
    pub slots_until_expand: u64,
    pub expand_trigger_pct: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningInfo {
    pub height: u64,
    pub difficulty_bits: u32,
    pub difficulty_target: String,
    pub block_reward_micronoid: u64,
    pub active_slot_count: u64,
}

/// Estimated network hashrate from a PoW target: expected hashes needed to
/// find one below `target` is `2^256 / target`, divided by the protocol's
/// target block interval. `target_hex` is the target's canonical
/// little-endian byte encoding (as the RPC returns it), so it's parsed with
/// `from_bytes_le` directly rather than needing to reverse it first.
pub fn estimate_hashrate(target_hex: &str) -> Option<f64> {
    let bytes = hex_decode(target_hex)?;
    let target = BigUint::from_bytes_le(&bytes);
    if target == BigUint::ZERO {
        return None;
    }
    let max = BigUint::from(1u8) << 256u32;
    let expected_hashes = &max / &target;
    // f64 conversion is intentionally approximate - this is a rough
    // network-wide estimate, not an accounting figure.
    let expected_hashes_f64: f64 = expected_hashes.to_string().parse().ok()?;
    Some(expected_hashes_f64 / TARGET_BLOCK_INTERVAL_SECONDS)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolInfo {
    pub size: u64,
    pub fee_floor: u64,
    pub txs: Vec<MempoolTxInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolTxInfo {
    pub tx_hash: String,
    pub fee_micronoid: u64,
    pub fee_rate: u64,
    pub n_inputs: u32,
    pub n_outputs: u32,
    pub page_count: u32,
    pub admitted_height: u64,
    /// Smallest block class that can carry it (from node v2.0 on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_proof_class: Option<String>,
    #[serde(default)]
    pub requires_b255_miner: bool,
    /// A v2 contract call. Not part of `getMempoolInfo`: the API fills it
    /// in from `getMempoolEntry`, once per transaction.
    #[serde(default)]
    pub contract: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MempoolEntryInfo {
    /// The call's 699-byte public opening; `None` for a payment.
    #[serde(default)]
    pub contract_opening_hex: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mempool_entries_from_old_and_new_nodes() {
        // v1.1: no class fields; v2: class and B255 flag; neither carries `contract`
        let old: MempoolTxInfo = serde_json::from_value(json!({
            "tx_hash": "ab", "fee_micronoid": 9000, "fee_rate": 4500, "n_inputs": 1, "n_outputs": 1, "page_count": 1, "admitted_height": 7
        }))
        .unwrap();
        assert_eq!((old.minimum_proof_class.as_deref(), old.requires_b255_miner, old.contract), (None, false, false));
        let new: MempoolTxInfo = serde_json::from_value(json!({
            "tx_hash": "ab", "fee_micronoid": 9000, "fee_rate": 4500, "n_inputs": 1, "n_outputs": 1, "page_count": 1, "admitted_height": 7,
            "minimum_proof_class": "B25", "requires_b255_miner": true, "has_authorization": true
        }))
        .unwrap();
        assert_eq!((new.minimum_proof_class.as_deref(), new.requires_b255_miner), (Some("B25"), true));
        let call: MempoolEntryInfo = serde_json::from_value(json!({ "tx_hash": "ab", "contract_opening_hex": "4e4f" })).unwrap();
        let payment: MempoolEntryInfo = serde_json::from_value(json!({ "tx_hash": "ab", "contract_opening_hex": null })).unwrap();
        assert!(call.contract_opening_hex.is_some() && payment.contract_opening_hex.is_none());
    }
}
