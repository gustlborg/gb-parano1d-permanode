//! Minimal JSON-RPC client for a Parano1d node, covering only the read-only
//! chain methods the indexer needs. Field names/types are taken from live
//! responses of a running `parano1d 1.1.0` node (`docs/reference/rpc.md` in
//! the node source matches), not just the docs, since retained-window
//! behavior in practice diverged from what the docs promise.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct RpcClient {
    url: String,
    agent: ureq::Agent,
}

impl RpcClient {
    pub fn new(url: String) -> Self {
        Self {
            url,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut resp = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("RPC call {method} failed to send"))?;
        let v: Value = resp
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

    pub fn block_count(&self) -> Result<u64> {
        let v = self.call("paranoid_blockCount", json!([]))?;
        v.as_u64().context("blockCount: result is not u64")
    }

    pub fn get_block_header(&self, height: u64) -> Result<Option<BlockHeaderInfo>> {
        let v = self.call("paranoid_getBlockHeader", json!([height]))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v)?))
    }

    pub fn get_block_details(&self, height: u64) -> Result<Option<BlockDetailsInfo>> {
        let v = self.call("paranoid_getBlockDetails", json!([height]))?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(v)?))
    }

    /// Raw canonical block bytes for `height` (`paranoid_getBlock`), decoded
    /// from hex. Unlike getBlockDetails this is *not* filtered by the
    /// node's "recursive suffix marker" bundle accessor, so it still serves
    /// marker blocks - see decode.rs and project docs for why that matters.
    /// Still only valid within the retention/serving window; `None` after.
    pub fn get_block_raw(&self, height: u64) -> Result<Option<Vec<u8>>> {
        let v = self.call("paranoid_getBlock", json!([height]))?;
        let Some(hex_str) = v.as_str() else {
            return Ok(None);
        };
        Ok(Some(hex::decode(hex_str).context("getBlock: invalid hex")?))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(dead_code)] // full RPC schema kept for fidelity even where we don't persist every field yet
pub struct BlockHeaderInfo {
    pub height: u64,
    pub hash: String,
    pub prev_hash: String,
    pub state_root: String,
    pub tx_root: String,
    pub timestamp: u64,
    pub miner: String,
    pub nonce_hex: String,
    pub difficulty_target: String,
    pub log_slots: u32,
    pub active_slot_count: u64,
    pub alloc_counter: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockDetailsInfo {
    pub header: BlockHeaderInfo,
    pub retained: Option<RetainedBlockInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct RetainedBlockInfo {
    pub proof_class: String,
    pub logical_transactions: u32,
    pub user_pages: u32,
    pub live_inputs: u32,
    pub live_outputs: u32,
    pub reward_micronoid: u64,
    pub total_fees_micronoid: String,
    pub block_bytes: u64,
    pub transactions: Vec<BlockTransactionInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct BlockTransactionInfo {
    pub position: u32,
    pub txid: String,
    pub page_count: u32,
    pub live_inputs: u32,
    pub live_outputs: u32,
    pub fee_micronoid: u64,
    pub coinbase: bool,
    pub development_payout: bool,
    pub epoch_anchor: String,
    pub input_owner: Option<String>,
    pub input_sum_micronoid: String,
    pub output_sum_micronoid: String,
    pub page_hashes: Vec<String>,
    pub inputs: Vec<BlockTransactionInputInfo>,
    pub outputs: Vec<BlockTransactionOutputInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockTransactionInputInfo {
    pub page: u32,
    pub lane: u32,
    pub slot_index: u64,
    pub amount_micronoid: u64,
    pub creation_id: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BlockTransactionOutputInfo {
    pub page: u32,
    pub lane: u32,
    pub slot_index: u64,
    pub amount_micronoid: u64,
    pub owner: String,
    pub creation_id: u64,
}
