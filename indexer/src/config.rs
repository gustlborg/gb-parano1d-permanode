use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_rpc_url")]
    pub rpc_url: String,

    #[serde(default = "default_db_path")]
    pub db_path: String,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,

    /// How many recent heights to re-check on every poll for reorgs.
    /// Must stay >= the protocol's maximum reorg depth (17 blocks at
    /// launch, see docs/protocol/parameters.md in the node source) plus a
    /// safety margin - the live-observed body-retention window did not
    /// reliably match the documented 18 blocks, so err on the wide side.
    #[serde(default = "default_reorg_check_depth")]
    pub reorg_check_depth: u64,

    /// Days of transaction detail to keep before pruning. 0 = keep
    /// forever. Block headers are always kept regardless of this setting.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,

    /// How often to run the pruning pass, in poll cycles.
    #[serde(default = "default_prune_every_cycles")]
    pub prune_every_cycles: u64,

    /// When getBlockDetails reports `retained: null` inside the node's
    /// serving window, fall back to decoding the raw block from getBlock
    /// ourselves (works around a node RPC bug where "marker" blocks from
    /// multi-block commits never get their body exposed through the
    /// details/transactions RPCs even though the node still has it - see
    /// project memory). Off switch in case a future node version changes
    /// the wire format and the decoder starts erroring.
    #[serde(default = "default_true")]
    pub getblock_fallback: bool,

    /// For every block where getBlockDetails already returned a body,
    /// additionally decode it via getBlock and compare, to continuously
    /// verify the fallback decoder against the RPC's own output (cheap,
    /// same-height loopback call). Logs and counts mismatches instead of
    /// trusting the decoder blindly. Meant to be turned off after it's
    /// been quiet for a while.
    #[serde(default = "default_true")]
    pub decoder_selfcheck: bool,
}

fn default_rpc_url() -> String {
    "http://127.0.0.1:9601".to_string()
}
fn default_db_path() -> String {
    "permanode.sqlite3".to_string()
}
fn default_poll_interval() -> u64 {
    5
}
fn default_reorg_check_depth() -> u64 {
    30
}
fn default_retention_days() -> u64 {
    0
}
fn default_prune_every_cycles() -> u64 {
    720 // ~1h at the default 5s poll interval
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: default_rpc_url(),
            db_path: default_db_path(),
            poll_interval_seconds: default_poll_interval(),
            reorg_check_depth: default_reorg_check_depth(),
            retention_days: default_retention_days(),
            prune_every_cycles: default_prune_every_cycles(),
            getblock_fallback: default_true(),
            decoder_selfcheck: default_true(),
        }
    }
}

impl Config {
    /// Load from `path`, creating it with commented defaults if missing —
    /// mirrors the node binary's own `-c/--config` behaviour.
    pub fn load_or_create(path: &Path) -> Result<Config> {
        if !path.exists() {
            let cfg = Config::default();
            let toml_str = format!(
                "# parano1d-permanode indexer config\n\
                 # Missing keys fall back to these defaults.\n\n\
                 rpc_url = {:?}\n\
                 db_path = {:?}\n\
                 poll_interval_seconds = {}\n\
                 reorg_check_depth = {}\n\
                 # 0 = keep transaction detail forever\n\
                 retention_days = {}\n\
                 prune_every_cycles = {}\n\
                 getblock_fallback = {}\n\
                 decoder_selfcheck = {}\n",
                cfg.rpc_url,
                cfg.db_path,
                cfg.poll_interval_seconds,
                cfg.reorg_check_depth,
                cfg.retention_days,
                cfg.prune_every_cycles,
                cfg.getblock_fallback,
                cfg.decoder_selfcheck,
            );
            std::fs::write(path, toml_str)?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }
}
