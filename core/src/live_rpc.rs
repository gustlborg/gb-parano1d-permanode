//! Minimal read-only client for node RPC methods whose answers are never
//! persisted: mempool contents (transient by nature) and other live-only
//! figures like the network's true current UTXO count, which this
//! permanode's own database cannot reconstruct from history it hasn't
//! recorded. The API proxies these straight from the node on request.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone)]
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
}
