//! Read-only queries for the API server. All of these only ever return the
//! currently-canonical version of a block: a block's latest
//! `block_status_log` entry must be `canonical`. Orphaned history is kept
//! in the database but is not what a normal explorer view should show.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

const CANONICAL_BLOCK_FILTER: &str = "
    (SELECT s.status FROM block_status_log s
     WHERE s.block_id = blocks.id
     ORDER BY s.id DESC LIMIT 1) = 'canonical'
";

#[derive(Debug, Serialize)]
pub struct BlockSummary {
    pub height: i64,
    pub hash: String,
    pub timestamp: i64,
    pub miner: String,
    pub proof_class: Option<String>,
    pub reward_micronoid: Option<i64>,
    pub total_fees_micronoid: Option<String>,
    pub tx_count: i64,
    pub body_captured: bool,
}

#[derive(Debug, Serialize)]
pub struct BlockDetail {
    pub height: i64,
    pub hash: String,
    pub prev_hash: String,
    pub state_root: String,
    pub tx_root: String,
    pub timestamp: i64,
    pub miner: String,
    pub nonce_hex: String,
    pub difficulty_target: String,
    pub proof_class: Option<String>,
    pub reward_micronoid: Option<i64>,
    pub total_fees_micronoid: Option<String>,
    pub body_captured: bool,
    /// Blocks on top of this one including itself, from the indexer's own
    /// tip; `None` if that isn't known yet. 18 and up is final on this
    /// chain (the protocol's maximum reorg depth is 17).
    pub confirmations: Option<i64>,
    pub transactions: Vec<TxSummary>,
}

#[derive(Debug, Serialize)]
pub struct TxSummary {
    pub position: i64,
    pub txid: String,
    pub page_count: i64,
    pub fee_micronoid: i64,
    pub coinbase: bool,
    pub development_payout: bool,
    pub input_owner: Option<String>,
    /// Owner of the first output (by idx). A transaction can have more
    /// than one output; `n_outputs` tells the caller whether there are
    /// others besides this one.
    pub receiver: Option<String>,
    pub input_sum_micronoid: String,
    pub output_sum_micronoid: String,
    pub height: i64,
    pub timestamp: i64,
    pub n_inputs: i64,
    pub n_outputs: i64,
    /// Only on address pages: what this transaction did to the viewed
    /// address's balance - outputs it received minus inputs it spent
    /// (change back to itself therefore cancels out, the fee shows as a
    /// small negative). `output_sum_micronoid` is the transaction's total
    /// and says nothing about one participant's share.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_delta_micronoid: Option<String>,
    /// Only on address pages: the first output owner that is not the
    /// viewed address (`receiver` may be its own change output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TxDetail {
    pub txid: String,
    pub position: i64,
    pub page_count: i64,
    pub fee_micronoid: i64,
    pub coinbase: bool,
    pub development_payout: bool,
    pub epoch_anchor: String,
    pub input_owner: Option<String>,
    pub input_sum_micronoid: String,
    pub output_sum_micronoid: String,
    pub page_hashes: Vec<String>,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    pub block: TxBlockRef,
    /// See `BlockDetail::confirmations`; `Some(0)` if the block was
    /// orphaned.
    pub confirmations: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TxInput {
    pub slot_index: i64,
    pub amount_micronoid: i64,
    pub creation_id: String,
}

#[derive(Debug, Serialize)]
pub struct TxOutput {
    pub slot_index: i64,
    pub amount_micronoid: i64,
    pub owner: String,
    pub creation_id: String,
}

#[derive(Debug, Serialize)]
pub struct TxBlockRef {
    pub height: i64,
    pub hash: String,
    pub timestamp: i64,
    pub canonical: bool,
}

#[derive(Debug, Serialize)]
pub struct ChainStats {
    pub last_processed_height: Option<i64>,
    pub indexed_blocks: i64,
    pub indexed_transactions: i64,
    /// Still-unresolved gaps only - a gap the getBlock fallback decoder
    /// later recovered is no longer missing data, so it's not counted
    /// here anymore (see `gaps_resolved`).
    pub gaps: i64,
    /// Gaps that were recovered via the getBlock fallback decoder after
    /// initially being recorded - kept visible for transparency even
    /// though the data is no longer actually missing.
    pub gaps_resolved: i64,
    /// Times the getBlock fallback decoder's output has disagreed with
    /// getBlockDetails for a block both could decode - see
    /// indexer's `decoder_selfcheck` config option. Should stay 0.
    pub decoder_mismatches: i64,
    pub oldest_retained_timestamp: Option<i64>,
    /// Outputs recorded on a canonical block whose creation_id has not
    /// (yet) been consumed by any recorded input - the live UTXO set as
    /// far as this permanode's own indexed history can tell. Same caveat
    /// as an address's confirmed balance: outputs already unspent before
    /// this permanode started recording are invisible to it.
    pub live_utxos: i64,
}

#[derive(Debug, Serialize)]
pub struct GapEntry {
    pub height: i64,
    pub hash: Option<String>,
    pub detected_at: String,
    pub note: String,
    pub resolved_at: Option<String>,
    pub resolution: Option<String>,
}

pub fn recent_blocks(conn: &Connection, limit: i64) -> Result<Vec<BlockSummary>> {
    let sql = format!(
        "SELECT blocks.height, blocks.hash, blocks.timestamp, blocks.miner,
                blocks.proof_class, blocks.reward_micronoid, blocks.total_fees_micronoid,
                blocks.body_captured,
                (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id) AS tx_count
         FROM blocks
         WHERE {CANONICAL_BLOCK_FILTER}
         ORDER BY blocks.height DESC
         LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(BlockSummary {
            height: row.get(0)?,
            hash: row.get(1)?,
            timestamp: row.get(2)?,
            miner: row.get(3)?,
            proof_class: row.get(4)?,
            reward_micronoid: row.get(5)?,
            total_fees_micronoid: row.get(6)?,
            body_captured: row.get::<_, i64>(7)? != 0,
            tx_count: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn block_id_and_row(
    conn: &Connection,
    where_clause: &str,
    param: &str,
) -> Result<Option<(i64, BlockDetail)>> {
    let sql = format!(
        "SELECT blocks.id, blocks.height, blocks.hash, blocks.prev_hash, blocks.state_root,
                blocks.tx_root, blocks.timestamp, blocks.miner, blocks.nonce_hex,
                blocks.difficulty_target, blocks.proof_class, blocks.reward_micronoid,
                blocks.total_fees_micronoid, blocks.body_captured
         FROM blocks
         WHERE {where_clause} AND {CANONICAL_BLOCK_FILTER}
         LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![param], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                BlockDetail {
                    height: row.get(1)?,
                    hash: row.get(2)?,
                    prev_hash: row.get(3)?,
                    state_root: row.get(4)?,
                    tx_root: row.get(5)?,
                    timestamp: row.get(6)?,
                    miner: row.get(7)?,
                    nonce_hex: row.get(8)?,
                    difficulty_target: row.get(9)?,
                    proof_class: row.get(10)?,
                    reward_micronoid: row.get(11)?,
                    total_fees_micronoid: row.get(12)?,
                    body_captured: row.get::<_, i64>(13)? != 0,
                    confirmations: None,
                    transactions: vec![],
                },
            ))
        })
        .optional()?;
    Ok(row)
}

// Column names rather than positions on purpose: a positional mismatch
// after adding `receiver` bit us twice already (n_outputs silently reading
// n_inputs' column). Named lookups can't drift out of sync with the SELECT
// list like that.
const TX_SUMMARY_COLUMNS: &str = "
    t.position, t.txid, t.page_count, t.fee_micronoid, t.coinbase, t.development_payout,
    t.input_owner, t.input_sum_micronoid, t.output_sum_micronoid,
    b.height, b.timestamp,
    (SELECT COUNT(*) FROM tx_inputs i WHERE i.tx_id = t.id) AS n_inputs,
    (SELECT COUNT(*) FROM tx_outputs o WHERE o.tx_id = t.id) AS n_outputs,
    (SELECT o.owner FROM tx_outputs o WHERE o.tx_id = t.id ORDER BY o.idx ASC LIMIT 1) AS receiver
";

fn tx_summary_from_row(row: &rusqlite::Row) -> rusqlite::Result<TxSummary> {
    Ok(TxSummary {
        position: row.get("position")?,
        txid: row.get("txid")?,
        page_count: row.get("page_count")?,
        fee_micronoid: row.get("fee_micronoid")?,
        coinbase: row.get::<_, i64>("coinbase")? != 0,
        development_payout: row.get::<_, i64>("development_payout")? != 0,
        input_owner: row.get("input_owner")?,
        receiver: row.get("receiver")?,
        input_sum_micronoid: row.get("input_sum_micronoid")?,
        output_sum_micronoid: row.get("output_sum_micronoid")?,
        height: row.get("height")?,
        timestamp: row.get("timestamp")?,
        n_inputs: row.get("n_inputs")?,
        n_outputs: row.get("n_outputs")?,
        address_delta_micronoid: row.get::<_, Option<i64>>("address_delta").ok().flatten().map(|d| d.to_string()),
        counterparty: row.get::<_, Option<String>>("counterparty").ok().flatten(),
    })
}

fn tx_summaries_for_block(conn: &Connection, block_id: i64) -> Result<Vec<TxSummary>> {
    let sql = format!(
        "SELECT {TX_SUMMARY_COLUMNS}
         FROM transactions t
         JOIN blocks b ON b.id = t.block_id
         WHERE t.block_id = ?1
         ORDER BY t.position ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![block_id], tx_summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The indexer's own tip, if it has recorded one.
pub fn indexed_tip(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT value FROM indexer_state WHERE key = 'last_processed_height'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|s| s.parse().ok()))
}

fn confirmations_for(tip: Option<i64>, height: i64) -> Option<i64> {
    tip.map(|t| (t - height + 1).max(0))
}

fn finish_block(conn: &Connection, block_id: i64, mut detail: BlockDetail) -> Result<BlockDetail> {
    detail.transactions = tx_summaries_for_block(conn, block_id)?;
    detail.confirmations = confirmations_for(indexed_tip(conn)?, detail.height);
    Ok(detail)
}

pub fn block_by_height(conn: &Connection, height: i64) -> Result<Option<BlockDetail>> {
    let Some((block_id, detail)) = block_id_and_row(conn, "blocks.height = ?1", &height.to_string())? else {
        return Ok(None);
    };
    Ok(Some(finish_block(conn, block_id, detail)?))
}

pub fn block_by_hash(conn: &Connection, hash: &str) -> Result<Option<BlockDetail>> {
    let Some((block_id, detail)) = block_id_and_row(conn, "blocks.hash = ?1", hash)? else {
        return Ok(None);
    };
    Ok(Some(finish_block(conn, block_id, detail)?))
}

pub fn tx_by_txid(conn: &Connection, txid: &str) -> Result<Option<TxDetail>> {
    let canonical_on_b = canonical_filter_on("b");
    let sql = format!(
        "SELECT t.id, t.position, t.page_count, t.fee_micronoid, t.coinbase,
                t.development_payout, t.epoch_anchor, t.input_owner, t.input_sum_micronoid,
                t.output_sum_micronoid, b.height, b.hash, b.timestamp,
                ({canonical_on_b}) AS is_canonical
         FROM transactions t
         JOIN blocks b ON b.id = t.block_id
         WHERE t.txid = ?1
         ORDER BY is_canonical DESC
         LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![txid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                TxDetail {
                    txid: txid.to_string(),
                    position: row.get(1)?,
                    page_count: row.get(2)?,
                    fee_micronoid: row.get(3)?,
                    coinbase: row.get::<_, i64>(4)? != 0,
                    development_payout: row.get::<_, i64>(5)? != 0,
                    epoch_anchor: row.get(6)?,
                    input_owner: row.get(7)?,
                    input_sum_micronoid: row.get(8)?,
                    output_sum_micronoid: row.get(9)?,
                    page_hashes: vec![],
                    inputs: vec![],
                    outputs: vec![],
                    block: TxBlockRef {
                        height: row.get(10)?,
                        hash: row.get(11)?,
                        timestamp: row.get(12)?,
                        canonical: row.get::<_, i64>(13)? != 0,
                    },
                    confirmations: None,
                },
            ))
        })
        .optional()?;
    let Some((tx_id, mut detail)) = row else {
        return Ok(None);
    };
    detail.confirmations = if detail.block.canonical {
        confirmations_for(indexed_tip(conn)?, detail.block.height)
    } else {
        Some(0)
    };

    let mut ph_stmt = conn.prepare(
        "SELECT page_hash FROM tx_page_hashes WHERE tx_id = ?1 ORDER BY idx ASC",
    )?;
    detail.page_hashes = ph_stmt
        .query_map(params![tx_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut in_stmt = conn.prepare(
        "SELECT slot_index, amount_micronoid, creation_id FROM tx_inputs WHERE tx_id = ?1 ORDER BY idx ASC",
    )?;
    detail.inputs = in_stmt
        .query_map(params![tx_id], |row| {
            Ok(TxInput {
                slot_index: row.get(0)?,
                amount_micronoid: row.get(1)?,
                creation_id: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut out_stmt = conn.prepare(
        "SELECT slot_index, amount_micronoid, owner, creation_id FROM tx_outputs WHERE tx_id = ?1 ORDER BY idx ASC",
    )?;
    detail.outputs = out_stmt
        .query_map(params![tx_id], |row| {
            Ok(TxOutput {
                slot_index: row.get(0)?,
                amount_micronoid: row.get(1)?,
                owner: row.get(2)?,
                creation_id: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(Some(detail))
}

/// Transactions where `address` appears as sender or as a recipient of at
/// least one output, newest block first. Returns the page of summaries plus
/// the total matching count for pagination.
pub fn txs_by_address(
    conn: &Connection,
    address: &str,
    page: i64,
    page_size: i64,
) -> Result<(Vec<TxSummary>, i64)> {
    let offset = (page.max(1) - 1).saturating_mul(page_size);
    let canonical_on_b = canonical_filter_on("b");

    let count_sql = format!(
        "SELECT COUNT(*)
         FROM transactions t
         JOIN blocks b ON b.id = t.block_id
         WHERE {canonical_on_b}
           AND (t.input_owner = ?1 OR EXISTS (
                 SELECT 1 FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner = ?1))"
    );
    let total: i64 = conn.query_row(&count_sql, params![address], |row| row.get(0))?;
    // A page past the end is answered without touching the table again:
    // SQLite would otherwise walk every matching row up to the offset,
    // which makes `?page=999999999` a cheap way to burn CPU.
    if offset >= total {
        return Ok((Vec::new(), total));
    }

    let sql = format!(
        "SELECT {TX_SUMMARY_COLUMNS},
                (SELECT COALESCE(SUM(o.amount_micronoid), 0) FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner = ?1)
                  - (CASE WHEN t.input_owner = ?1 THEN CAST(t.input_sum_micronoid AS INTEGER) ELSE 0 END) AS address_delta,
                (SELECT o.owner FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner != ?1 ORDER BY o.idx ASC LIMIT 1) AS counterparty
         FROM transactions t
         JOIN blocks b ON b.id = t.block_id
         WHERE {canonical_on_b}
           AND (t.input_owner = ?1 OR EXISTS (
                 SELECT 1 FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner = ?1))
         ORDER BY b.height DESC, t.position DESC
         LIMIT ?2 OFFSET ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![address, page_size, offset], tx_summary_from_row)?;
    let items: Vec<TxSummary> = rows.collect::<rusqlite::Result<_>>()?;

    Ok((items, total))
}

fn canonical_filter_on(block_alias: &str) -> String {
    format!(
        "(SELECT s.status FROM block_status_log s WHERE s.block_id = {block_alias}.id \
          ORDER BY s.id DESC LIMIT 1) = 'canonical'"
    )
}

#[derive(Debug, Serialize)]
pub struct AddressBalance {
    pub confirmed_balance_micronoid: String,
    pub confirmed_utxos: i64,
    pub total_received_micronoid: String,
    pub total_sent_micronoid: String,
    /// Recorded outputs of this address that the node no longer holds
    /// although no recorded transaction spent them - spent in blocks
    /// this permanode has no body for. Excluded from the confirmed
    /// figures above; shown so the gap is visible instead of silent.
    pub spent_in_gap_micronoid: String,
    pub spent_in_gap_utxos: i64,
}

/// Confirmed balance and UTXO count for `address`, computed from indexed
/// history only. An output counts as unspent if no recorded input anywhere
/// (any address, any block) spends the same `creation_id` - slot_index
/// alone isn't a stable identifier since slots get recycled once spent, but
/// creation_id is unique per creation event. This can only see spends and
/// receipts that happened after this permanode started recording: a
/// balance that already existed before that is not reflected here.
pub fn address_balance(conn: &Connection, address: &str) -> Result<AddressBalance> {
    let canonical_on_b = canonical_filter_on("b");
    let canonical_on_b2 = canonical_filter_on("b2");

    let total_received_micronoid: String = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(CAST(o.amount_micronoid AS INTEGER)), 0)
             FROM tx_outputs o
             JOIN transactions t ON t.id = o.tx_id
             JOIN blocks b ON b.id = t.block_id
             WHERE o.owner = ?1 AND {canonical_on_b}"
        ),
        params![address],
        |row| row.get::<_, i64>(0),
    )?
    .to_string();

    // Unspent = no recorded input consumed it AND the sweep hasn't found it
    // gone from the node's state (spent_in_gap, see db::migrate).
    let unspent_sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(CAST(o.amount_micronoid AS INTEGER)), 0)
         FROM tx_outputs o
         JOIN transactions t ON t.id = o.tx_id
         JOIN blocks b ON b.id = t.block_id
         WHERE o.owner = ?1 AND {canonical_on_b} AND o.spent_in_gap = ?2
           AND NOT EXISTS (
             SELECT 1 FROM tx_inputs i
             JOIN transactions t2 ON t2.id = i.tx_id
             JOIN blocks b2 ON b2.id = t2.block_id
             WHERE i.creation_id = o.creation_id AND {canonical_on_b2}
           )"
    );
    let (confirmed_utxos, confirmed_balance_micronoid): (i64, String) =
        conn.query_row(&unspent_sql, params![address, 0], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.to_string())))?;
    let (spent_in_gap_utxos, spent_in_gap_micronoid): (i64, String) =
        conn.query_row(&unspent_sql, params![address, 1], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.to_string())))?;

    let total_sent_micronoid: String = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(CAST(t.input_sum_micronoid AS INTEGER)), 0)
             FROM transactions t
             JOIN blocks b ON b.id = t.block_id
             WHERE t.input_owner = ?1 AND {canonical_on_b}"
        ),
        params![address],
        |row| row.get::<_, i64>(0),
    )?
    .to_string();

    Ok(AddressBalance {
        confirmed_balance_micronoid,
        confirmed_utxos,
        total_received_micronoid,
        total_sent_micronoid,
        spent_in_gap_micronoid,
        spent_in_gap_utxos,
    })
}

pub fn chain_stats(conn: &Connection) -> Result<ChainStats> {
    let last_processed_height: Option<i64> = conn
        .query_row(
            "SELECT value FROM indexer_state WHERE key = 'last_processed_height'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| s.parse().ok());

    let decoder_mismatches: i64 = conn
        .query_row(
            "SELECT value FROM indexer_state WHERE key = 'decoder_mismatches'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let indexed_blocks: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM blocks WHERE {CANONICAL_BLOCK_FILTER}"), [], |r| r.get(0))?;
    let indexed_transactions: i64 =
        conn.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))?;
    let gaps: i64 =
        conn.query_row("SELECT COUNT(*) FROM ingest_gaps WHERE resolved_at IS NULL", [], |r| r.get(0))?;
    let gaps_resolved: i64 =
        conn.query_row("SELECT COUNT(*) FROM ingest_gaps WHERE resolved_at IS NOT NULL", [], |r| r.get(0))?;
    let oldest_retained_timestamp: Option<i64> = conn.query_row(
        "SELECT MIN(timestamp) FROM blocks WHERE body_captured = 1",
        [],
        |r| r.get(0),
    )?;

    let canonical_on_b = canonical_filter_on("b");
    let canonical_on_b2 = canonical_filter_on("b2");
    let live_utxos: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*)
             FROM tx_outputs o
             JOIN transactions t ON t.id = o.tx_id
             JOIN blocks b ON b.id = t.block_id
             WHERE {canonical_on_b} AND o.spent_in_gap = 0
               AND NOT EXISTS (
                 SELECT 1 FROM tx_inputs i
                 JOIN transactions t2 ON t2.id = i.tx_id
                 JOIN blocks b2 ON b2.id = t2.block_id
                 WHERE i.creation_id = o.creation_id AND {canonical_on_b2}
               )"
        ),
        [],
        |r| r.get(0),
    )?;

    Ok(ChainStats {
        last_processed_height,
        indexed_blocks,
        indexed_transactions,
        gaps,
        gaps_resolved,
        decoder_mismatches,
        oldest_retained_timestamp,
        live_utxos,
    })
}

/// Average interval between canonical blocks recorded in the last
/// `window_seconds`, i.e. (span between oldest and newest block in the
/// window) / (count - 1). `None` if fewer than 2 blocks fall in the
/// window - including, unavoidably, for a window that reaches further
/// back than this permanode has been recording.
pub fn avg_block_time_seconds(conn: &Connection, window_seconds: i64, now_unix: i64) -> Result<Option<f64>> {
    let cutoff = now_unix - window_seconds;
    let sql = format!(
        "SELECT COUNT(*), MIN(timestamp), MAX(timestamp)
         FROM blocks
         WHERE timestamp >= ?1 AND {CANONICAL_BLOCK_FILTER}"
    );
    let (count, min_ts, max_ts): (i64, Option<i64>, Option<i64>) =
        conn.query_row(&sql, params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    match (count, min_ts, max_ts) {
        (c, Some(min), Some(max)) if c >= 2 && max > min => {
            Ok(Some((max - min) as f64 / (c - 1) as f64))
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Serialize)]
pub struct RichListEntry {
    pub address: String,
    pub live_balance_micronoid: String,
    pub live_utxo_count: i64,
    pub fetched_at: String,
}

/// The address balance cache (see core::db::address_balance_cache),
/// largest live balance first. Figures are only as fresh as the indexer's
/// last refresh pass - `fetched_at` says when that was for each row, since
/// different addresses can lag by different amounts if the refresh sweep
/// is still catching up on a growing address list.
pub fn richlist(conn: &Connection, limit: i64) -> Result<Vec<RichListEntry>> {
    let mut stmt = conn.prepare(
        "SELECT address, live_balance_micronoid, live_utxo_count, fetched_at
         FROM address_balance_cache
         ORDER BY CAST(live_balance_micronoid AS INTEGER) DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(RichListEntry {
            address: row.get(0)?,
            live_balance_micronoid: row.get(1)?,
            live_utxo_count: row.get(2)?,
            fetched_at: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn recent_gaps(conn: &Connection, limit: i64) -> Result<Vec<GapEntry>> {
    let mut stmt = conn.prepare(
        "SELECT height, hash, detected_at, note, resolved_at, resolution
         FROM ingest_gaps ORDER BY height DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(GapEntry {
            height: row.get(0)?,
            hash: row.get(1)?,
            detected_at: row.get(2)?,
            note: row.get(3)?,
            resolved_at: row.get(4)?,
            resolution: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
