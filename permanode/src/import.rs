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
//! of transactions is complete and genuine. The decoded contents of those
//! transactions (amounts, owners) are taken as the other permanode
//! recorded them once their sums add up: fill only from permanodes you run
//! or trust.

use crate::indexer::insert_transactions;
use crate::rpc::{BlockTransactionInfo, BlockTransactionInputInfo, BlockTransactionOutputInfo, RetainedBlockInfo};
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

pub fn import_bodies(conn: &Connection, source_path: &Path) -> Result<ImportReport> {
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
    fill_gaps(conn, usize::MAX, "import", &format!("recovered via import from {source_name}"), |height, hash| {
        body_from_db(&source, height, hash).map_err(Rejected::from)
    })
}

/// Asks each peer in turn for the bodies of up to `max` open gaps.
pub fn fill_from_peers(conn: &Connection, peers: &[String], max: usize) -> Result<ImportReport> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut unreachable: Vec<String> = Vec::new();
    let mut report = fill_gaps(conn, max, "peer", "recovered from a peer permanode", |height, hash| {
        for peer in peers {
            if unreachable.contains(peer) {
                continue;
            }
            match fetch_peer_body(&agent, peer, height, hash) {
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
    mut find: impl FnMut(u64, &str) -> Result<Option<RetainedBlockInfo>, Rejected>,
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
        let body = match find(height, &hash) {
            Ok(Some(body)) => body,
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
