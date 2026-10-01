//! Fills this permanode's gaps from another permanode: from a copy of its
//! database (`import-bodies`), or, while both run, over the network from
//! its peer endpoint (`backfill_peers`, `fill-from-peer`).
//!
//! A gap is a block whose header we have (from our own node) but whose
//! body the node had already pruned by the time we asked - typically
//! after this machine was offline for more than the node's serving
//! window. Another permanode that stayed online has the body.
//!
//! Every body taken over must fit this permanode's own record of the
//! block, which came from its own node: the same hash, and its
//! transactions in order must rebuild the header's `tx_root` - so the list
//! of transactions is complete and genuine. Where the other permanode kept
//! the block's raw bytes (`raw.rs`), those are taken and decoded here, so
//! every field is checked against the header. Otherwise the decoded
//! contents of those transactions (amounts, owners) are taken as the other
//! permanode recorded them once their sums add up: fill only from
//! permanodes you run or trust.
//!
//! The raw bytes themselves are filled the same way (`fill_raw_from_peers`,
//! and `import-bodies`): a permanode that was offline has the bodies from
//! its node's serving window on, but not the bytes of what it missed.

use crate::indexer::{insert_transactions, record_contract_flags};
use crate::rpc::{BlockTransactionInfo, BlockTransactionInputInfo, BlockTransactionOutputInfo, RetainedBlockInfo};
use crate::{decode, raw};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use log::{info, warn};
use permanode_core::db;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// Path of the peer endpoint's block bodies: `<peer>/peer/v1/body/<height>/<hash>`.
pub const PEER_BODY_PATH: &str = "/peer/v1/body";
/// Path of the peer endpoint's raw block bytes: `<peer>/peer/v1/raw/<height>/<hash>`.
pub const PEER_RAW_PATH: &str = "/peer/v1/raw";

/// What another permanode hands over for a block.
pub enum Offer {
    /// The block's raw bytes: decoded here, every field checked.
    Raw(Vec<u8>),
    /// The body as the other permanode recorded it (`check_body`).
    Body(RetainedBlockInfo),
}

#[derive(Debug, Default, Serialize)]
pub struct RawReport {
    /// Blocks asked for.
    pub missing: usize,
    pub kept: usize,
    pub not_in_source: usize,
    pub rejected: usize,
    pub unreachable: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub gaps: usize,
    pub imported: usize,
    pub not_in_source: usize,
    pub rejected: usize,
    pub flags_cleared: usize,
    /// Peers that could not be asked (network, HTTP error).
    pub unreachable: Vec<String>,
}

/// Fills the gaps from a copy of another permanode's database, and with
/// `keep_raw` the raw bytes this one has none of (see `import_raw`).
pub fn import_bodies(conn: &Connection, source_path: &Path, keep_raw: bool) -> Result<ImportReport> {
    let source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open source database {}", source_path.display()))?;
    for table in ["blocks", "transactions", "tx_inputs", "tx_outputs", "tx_page_hashes", "block_status_log"] {
        let present: bool = source.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |r| r.get(0),
        )?;
        if !present {
            bail!("{} is not a permanode database (no table {table})", source_path.display());
        }
    }
    let source_name = source_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let source_tip: Option<i64> = source.query_row("SELECT MAX(height) FROM blocks WHERE body_captured = 1", [], |r| r.get(0))?;
    info!("importing from {} (bodies up to height {})", source_path.display(), source_tip.unwrap_or(-1));
    let source_has_raw = has_table(&source, "raw_blocks")?;
    fill_gaps(conn, usize::MAX, "import", &format!("recovered via import from {source_name}"), keep_raw, |height, hash| {
        if source_has_raw {
            if let Some(bytes) = raw::load(&source, height, hash).map_err(Rejected::from)? {
                return Ok(Some(Offer::Raw(bytes)));
            }
        }
        Ok(body_from_db(&source, height, hash).map_err(Rejected::from)?.map(Offer::Body))
    })
}

/// Takes over the raw bytes the source database kept for blocks whose
/// body this permanode has but whose bytes it does not.
pub fn import_raw(conn: &Connection, source_path: &Path) -> Result<RawReport> {
    let source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open source database {}", source_path.display()))?;
    if !has_table(&source, "raw_blocks")? {
        return Ok(RawReport::default());
    }
    let candidates = db::blocks_missing_raw(conn, 0, u64::MAX, usize::MAX)?;
    fill_raw(conn, candidates, "import", |height, hash| raw::load(&source, height, hash).map_err(Rejected::from))
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1", params![table], |r| r.get(0))?)
}

fn peer_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// Asks each peer in turn for the bodies of up to `max` open gaps - its
/// raw bytes where it kept them, else its recorded body.
pub fn fill_from_peers(conn: &Connection, peers: &[String], max: usize, keep_raw: bool) -> Result<ImportReport> {
    let agent = peer_agent();
    let mut unreachable: Vec<String> = Vec::new();
    let mut report = fill_gaps(conn, max, "peer", "recovered from a peer permanode", keep_raw, |height, hash| {
        for peer in peers {
            if unreachable.contains(peer) {
                continue;
            }
            match fetch_peer_offer(&agent, peer, height, hash) {
                Ok(Some(body)) => return Ok(Some(body)),
                Ok(None) => {}
                Err(PeerError::Unreachable(e)) => {
                    warn!("peer {peer} unreachable: {e:#}");
                    unreachable.push(peer.clone());
                }
                Err(PeerError::Bad(e)) => return Err(Rejected(e.context(format!("from {peer}")))),
            }
        }
        Ok(None)
    })?;
    report.unreachable = unreachable;
    Ok(report)
}

/// Asks the peers for the raw bytes of blocks this permanode has a body
/// but no bytes for, up to `max` of them, from the lowest height any peer
/// kept bytes for (`/peer/v1/status`) up to `below`: above it the node
/// still serves the body and the indexer fetches the bytes itself.
pub fn fill_raw_from_peers(conn: &Connection, peers: &[String], below: u64, max: usize) -> Result<RawReport> {
    let agent = peer_agent();
    let mut unreachable: Vec<String> = Vec::new();
    let mut from: Option<u64> = None;
    for peer in peers {
        match peer_raw_from(&agent, peer) {
            Ok(Some(h)) => from = Some(from.map_or(h, |f| f.min(h))),
            Ok(None) => {}
            Err(e) => {
                warn!("peer {peer} unreachable: {e:#}");
                unreachable.push(peer.clone());
            }
        }
    }
    let Some(from) = from.filter(|f| *f < below) else {
        return Ok(RawReport { unreachable, ..RawReport::default() });
    };
    let candidates = db::blocks_missing_raw(conn, from, below - 1, max)?;
    let mut report = fill_raw(conn, candidates, "peer", |height, hash| {
        for peer in peers {
            if unreachable.contains(peer) {
                continue;
            }
            match fetch_peer_raw(&agent, peer, height, hash) {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(PeerError::Unreachable(e)) => {
                    warn!("peer {peer} unreachable: {e:#}");
                    unreachable.push(peer.clone());
                }
                Err(PeerError::Bad(e)) => return Err(Rejected(e.context(format!("from {peer}")))),
            }
        }
        Ok(None)
    })?;
    report.unreachable = unreachable;
    Ok(report)
}

/// Offers each block to `find` and keeps the bytes it returns once they
/// prove to be that block (`raw::store`).
fn fill_raw(
    conn: &Connection,
    candidates: Vec<(u64, String)>,
    source: &str,
    mut find: impl FnMut(u64, &str) -> Result<Option<Vec<u8>>, Rejected>,
) -> Result<RawReport> {
    let mut report = RawReport { missing: candidates.len(), ..RawReport::default() };
    for (height, hash) in candidates {
        let bytes = match find(height, &hash) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                report.not_in_source += 1;
                continue;
            }
            Err(Rejected(e)) => {
                warn!("height {height}: raw bytes rejected: {e:#}");
                report.rejected += 1;
                continue;
            }
        };
        if let Err(e) = raw::verify(&bytes, height, &hash) {
            warn!("height {height}: raw bytes rejected: {e:#}");
            report.rejected += 1;
            continue;
        }
        let tx = db::write_tx(conn)?;
        let kept = raw::store(&tx, height, &hash, &bytes, source)?;
        tx.commit()?;
        if kept {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// A body that was offered but does not add up, as opposed to a failure of
/// our own database (which aborts the run).
pub struct Rejected(anyhow::Error);

impl From<anyhow::Error> for Rejected {
    fn from(e: anyhow::Error) -> Self {
        Rejected(e)
    }
}

enum PeerError {
    Unreachable(anyhow::Error),
    Bad(anyhow::Error),
}

/// The peer's raw bytes of the block if it kept them, else its body.
fn fetch_peer_offer(agent: &ureq::Agent, peer: &str, height: u64, hash: &str) -> Result<Option<Offer>, PeerError> {
    if let Some(bytes) = fetch_peer_raw(agent, peer, height, hash)? {
        return Ok(Some(Offer::Raw(bytes)));
    }
    Ok(fetch_peer_body(agent, peer, height, hash)?.map(Offer::Body))
}

/// The raw bytes the peer kept for the block, `None` on 404 (also from a
/// peer too old to have the path).
fn fetch_peer_raw(agent: &ureq::Agent, peer: &str, height: u64, hash: &str) -> Result<Option<Vec<u8>>, PeerError> {
    let url = format!("{}{PEER_RAW_PATH}/{height}/{hash}", peer.trim_end_matches('/'));
    let mut resp = agent.get(&url).call().map_err(|e| PeerError::Unreachable(e.into()))?;
    match resp.status().as_u16() {
        200 => resp
            .body_mut()
            .with_config()
            .limit(raw::max_block_bytes() as u64)
            .read_to_vec()
            .map(Some)
            .map_err(|e| PeerError::Bad(anyhow::Error::from(e).context("unreadable raw bytes"))),
        404 => Ok(None),
        s => Err(PeerError::Unreachable(anyhow::anyhow!("HTTP {s} for {url}"))),
    }
}

/// The lowest height the peer kept raw bytes for, `None` if it kept none
/// (or is too old to say).
fn peer_raw_from(agent: &ureq::Agent, peer: &str) -> Result<Option<u64>> {
    let url = format!("{}/peer/v1/status", peer.trim_end_matches('/'));
    let mut resp = agent.get(&url).call()?;
    if resp.status().as_u16() != 200 {
        bail!("HTTP {} for {url}", resp.status().as_u16());
    }
    let status: serde_json::Value = resp.body_mut().read_json()?;
    Ok(status.get("raw_from").and_then(|v| v.as_u64()))
}

fn fetch_peer_body(agent: &ureq::Agent, peer: &str, height: u64, hash: &str) -> Result<Option<RetainedBlockInfo>, PeerError> {
    let url = format!("{}{PEER_BODY_PATH}/{height}/{hash}", peer.trim_end_matches('/'));
    let mut resp = agent.get(&url).call().map_err(|e| PeerError::Unreachable(e.into()))?;
    match resp.status().as_u16() {
        200 => resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_json::<RetainedBlockInfo>()
            .map(Some)
            .map_err(|e| PeerError::Bad(anyhow::Error::from(e).context("unreadable body"))),
        404 => Ok(None),
        s => Err(PeerError::Unreachable(anyhow::anyhow!("HTTP {s} for {url}"))),
    }
}

/// Offers every open gap (lowest first, at most `max`) to `find`, checks
/// what it returns against our own header and records it.
fn fill_gaps(
    conn: &Connection,
    max: usize,
    body_source: &str,
    resolution: &str,
    keep_raw: bool,
    mut find: impl FnMut(u64, &str) -> Result<Option<Offer>, Rejected>,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let heights = db::open_gap_heights(conn, 0)?;
    report.gaps = heights.len();
    for height in heights.into_iter().take(max) {
        let Some((_, hash)) = db::canonical_hash_at(conn, height)? else {
            continue;
        };
        let Some(block_id) = db::uncaptured_block_id(conn, height, &hash)? else {
            continue; // recovered meanwhile
        };
        let tx_root: String = conn.query_row("SELECT tx_root FROM blocks WHERE id = ?1", params![block_id], |r| r.get(0))?;
        let (body, bytes) = match find(height, &hash) {
            Ok(Some(Offer::Body(body))) => (body, None),
            Ok(Some(Offer::Raw(bytes))) => match decode::decode_retained_block(&bytes, height, &hash) {
                Ok(body) => (body, Some(bytes)),
                Err(e) => {
                    warn!("height {height}: raw bytes rejected: {e:#}");
                    report.rejected += 1;
                    continue;
                }
            },
            Ok(None) => {
                report.not_in_source += 1;
                continue;
            }
            Err(Rejected(e)) => {
                warn!("height {height}: body rejected: {e:#}");
                report.rejected += 1;
                continue;
            }
        };
        if let Err(e) = check_body(&body, &tx_root) {
            warn!("height {height}: body rejected: {e:#}");
            report.rejected += 1;
            continue;
        }
        let n = body.transactions.len();
        let now = Utc::now().to_rfc3339();
        let tx = db::write_tx(conn)?;
        insert_transactions(&tx, block_id, &body)?;
        db::mark_body_recovered(&tx, block_id, body_source)?;
        if let Some(bytes) = &bytes {
            record_contract_flags(&tx, height, &hash, Some(bytes))?;
            if keep_raw {
                raw::store(&tx, height, &hash, bytes, body_source)?;
            }
        }
        db::resolve_gap(&tx, height, &now, resolution)?;
        tx.commit()?;
        info!("height {height}: gap {resolution} ({n} tx)");
        report.imported += 1;
    }

    // Outputs the sweep had flagged as "spent in a block without body"
    // whose spending transaction is now on record are ordinary spent
    // outputs again.
    report.flags_cleared = db::clear_spent_in_gap_with_recorded_spend(conn)?;
    let closed = db::resolve_gaps_with_bodies(conn, &Utc::now().to_rfc3339())?;
    if closed > 0 {
        info!("{closed} gap entr{} closed: the block has a body on record", if closed == 1 { "y" } else { "ies" });
    }
    Ok(report)
}

/// Checks a body against the header this permanode recorded for the
/// block: its transactions, in order, must rebuild `tx_root`, and every
/// sum must add up.
pub fn check_body(body: &RetainedBlockInfo, tx_root: &str) -> Result<()> {
    let txs = &body.transactions;
    if txs.is_empty() {
        bail!("no transactions");
    }
    if txs.len() > 256 {
        bail!("{} transactions, a block holds at most 256", txs.len());
    }
    if !txs[0].coinbase {
        bail!("first transaction is not the coinbase");
    }
    let mut txids = Vec::with_capacity(txs.len());
    for (i, t) in txs.iter().enumerate() {
        if t.position as usize != i {
            bail!("transaction {} at position {i}", t.position);
        }
        let id: [u8; 32] = hex::decode(&t.txid).ok().and_then(|b| b.try_into().ok()).with_context(|| format!("txid {} is not 32 bytes of hex", t.txid))?;
        txids.push(id);
        if t.page_hashes.len() != t.page_count as usize {
            bail!("tx {}: {} page hashes for {} pages", t.txid, t.page_hashes.len(), t.page_count);
        }
        if t.live_inputs as usize != t.inputs.len() || t.live_outputs as usize != t.outputs.len() {
            bail!("tx {}: live input/output counts do not match its lists", t.txid);
        }
        let output_total: u64 = t.outputs.iter().map(|o| o.amount_micronoid).sum();
        if output_total.to_string() != t.output_sum_micronoid {
            bail!("tx {}: outputs sum to {output_total}, recorded {}", t.txid, t.output_sum_micronoid);
        }
        let input_total: u64 = t.inputs.iter().map(|i| i.amount_micronoid).sum();
        if input_total.to_string() != t.input_sum_micronoid {
            bail!("tx {}: inputs sum to {input_total}, recorded {}", t.txid, t.input_sum_micronoid);
        }
        if !t.coinbase && !t.development_payout && input_total != output_total + t.fee_micronoid {
            bail!("tx {}: inputs {input_total} != outputs {output_total} + fee {}", t.txid, t.fee_micronoid);
        }
    }
    let root = hex::encode(noid_chain::tx_tree::root_from_hashes(&txids));
    if root != tx_root {
        bail!("the transactions rebuild tx_root {root}, the header says {tx_root}");
    }
    let fee_total: u64 = txs.iter().filter(|t| !t.coinbase).map(|t| t.fee_micronoid).sum();
    if fee_total.to_string() != body.total_fees_micronoid {
        bail!("fees sum to {fee_total}, block records {}", body.total_fees_micronoid);
    }
    let coinbase_total: u64 = txs[0].outputs.iter().map(|o| o.amount_micronoid).sum();
    if coinbase_total != body.reward_micronoid {
        bail!("coinbase outputs sum to {coinbase_total}, block records {}", body.reward_micronoid);
    }
    if body.logical_transactions as usize != txs.len()
        || body.live_inputs != txs.iter().map(|t| t.live_inputs).sum::<u32>()
        || body.live_outputs != txs.iter().map(|t| t.live_outputs).sum::<u32>()
        || body.user_pages != txs.iter().filter(|t| !t.coinbase && !t.development_payout).map(|t| t.page_count).sum::<u32>()
    {
        bail!("block totals do not match its transactions");
    }
    Ok(())
}

/// The body of `(height, hash)` as a permanode database recorded it, or
/// `None` if it does not have it. Checked by the caller (`check_body`).
pub fn body_from_db(source: &Connection, height: u64, hash: &str) -> Result<Option<RetainedBlockInfo>> {
    let Some((block_id, proof_class, reward, total_fees)) = source
        .query_row(
            "SELECT id, proof_class, reward_micronoid, total_fees_micronoid
             FROM blocks WHERE height = ?1 AND hash = ?2 AND body_captured = 1",
            params![height as i64, hash],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let (Some(proof_class), Some(reward), Some(total_fees)) = (proof_class, reward, total_fees) else {
        bail!("block row is marked captured but has no body fields");
    };

    let mut transactions = Vec::new();
    let mut stmt = source.prepare(
        "SELECT id, position, txid, page_count, fee_micronoid, coinbase, development_payout,
                epoch_anchor, input_owner, input_sum_micronoid, output_sum_micronoid
         FROM transactions WHERE block_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![block_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)? != 0,
            r.get::<_, i64>(6)? != 0,
            r.get::<_, String>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
        ))
    })?;
    for row in rows {
        let (tx_id, position, txid, page_count, fee, coinbase, development_payout, epoch_anchor, input_owner, input_sum, output_sum) = row?;
        let page_hashes: Vec<String> = source
            .prepare("SELECT page_hash FROM tx_page_hashes WHERE tx_id = ?1 ORDER BY idx")?
            .query_map(params![tx_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let inputs: Vec<BlockTransactionInputInfo> = source
            .prepare("SELECT page, lane, slot_index, amount_micronoid, creation_id FROM tx_inputs WHERE tx_id = ?1 ORDER BY idx")?
            .query_map(params![tx_id], |r| {
                Ok(BlockTransactionInputInfo {
                    page: r.get::<_, i64>(0)? as u32,
                    lane: r.get::<_, i64>(1)? as u32,
                    slot_index: r.get::<_, i64>(2)? as u64,
                    amount_micronoid: r.get::<_, i64>(3)? as u64,
                    creation_id: r.get::<_, String>(4)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let outputs: Vec<BlockTransactionOutputInfo> = source
            .prepare("SELECT page, lane, slot_index, amount_micronoid, owner, creation_id FROM tx_outputs WHERE tx_id = ?1 ORDER BY idx")?
            .query_map(params![tx_id], |r| {
                Ok(BlockTransactionOutputInfo {
                    page: r.get::<_, i64>(0)? as u32,
                    lane: r.get::<_, i64>(1)? as u32,
                    slot_index: r.get::<_, i64>(2)? as u64,
                    amount_micronoid: r.get::<_, i64>(3)? as u64,
                    owner: r.get::<_, String>(4)?,
                    creation_id: r.get::<_, String>(5)?.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        transactions.push(BlockTransactionInfo {
            position: position as u32,
            txid,
            page_count: page_count as u32,
            live_inputs: inputs.len() as u32,
            live_outputs: outputs.len() as u32,
            fee_micronoid: fee as u64,
            coinbase,
            development_payout,
            epoch_anchor,
            input_owner,
            input_sum_micronoid: input_sum,
            output_sum_micronoid: output_sum,
            page_hashes,
            inputs,
            outputs,
        });
    }

    Ok(Some(RetainedBlockInfo {
        proof_class,
        logical_transactions: transactions.len() as u32,
        user_pages: transactions.iter().filter(|t| !t.coinbase && !t.development_payout).map(|t| t.page_count).sum(),
        live_inputs: transactions.iter().map(|t| t.live_inputs).sum(),
        live_outputs: transactions.iter().map(|t| t.live_outputs).sum(),
        reward_micronoid: reward as u64,
        total_fees_micronoid: total_fees,
        block_bytes: 0,
        transactions,
    }))
}
