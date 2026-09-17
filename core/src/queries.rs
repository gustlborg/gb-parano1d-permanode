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
    pub n_inputs: i64,
    pub n_outputs: i64,
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
    pub gaps: i64,
    pub oldest_retained_timestamp: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GapEntry {
    pub height: i64,
    pub hash: Option<String>,
    pub detected_at: String,
    pub note: String,
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
        n_inputs: row.get("n_inputs")?,
        n_outputs: row.get("n_outputs")?,
    })
}

fn tx_summaries_for_block(conn: &Connection, block_id: i64) -> Result<Vec<TxSummary>> {
    let sql = format!(
        "SELECT {TX_SUMMARY_COLUMNS}
         FROM transactions t
         WHERE t.block_id = ?1
         ORDER BY t.position ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![block_id], tx_summary_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn block_by_height(conn: &Connection, height: i64) -> Result<Option<BlockDetail>> {
    let Some((block_id, mut detail)) = block_id_and_row(conn, "blocks.height = ?1", &height.to_string())? else {
        return Ok(None);
    };
    detail.transactions = tx_summaries_for_block(conn, block_id)?;
    Ok(Some(detail))
}

pub fn block_by_hash(conn: &Connection, hash: &str) -> Result<Option<BlockDetail>> {
    let Some((block_id, mut detail)) = block_id_and_row(conn, "blocks.hash = ?1", hash)? else {
        return Ok(None);
    };
    detail.transactions = tx_summaries_for_block(conn, block_id)?;
    Ok(Some(detail))
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
                },
            ))
        })
        .optional()?;
    let Some((tx_id, mut detail)) = row else {
        return Ok(None);
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
    let offset = (page.max(1) - 1) * page_size;
    let canonical_on_b = canonical_filter_on("b");
    let sql = format!(
        "SELECT {TX_SUMMARY_COLUMNS}, b.height
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

    let count_sql = format!(
        "SELECT COUNT(*)
         FROM transactions t
         JOIN blocks b ON b.id = t.block_id
         WHERE {}
           AND (t.input_owner = ?1 OR EXISTS (
                 SELECT 1 FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner = ?1))",
        canonical_filter_on("b")
    );
    let total: i64 = conn.query_row(&count_sql, params![address], |row| row.get(0))?;

    Ok((items, total))
}

fn canonical_filter_on(block_alias: &str) -> String {
    format!(
        "(SELECT s.status FROM block_status_log s WHERE s.block_id = {block_alias}.id \
          ORDER BY s.id DESC LIMIT 1) = 'canonical'"
    )
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

    let indexed_blocks: i64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM blocks WHERE {CANONICAL_BLOCK_FILTER}"), [], |r| r.get(0))?;
    let indexed_transactions: i64 =
        conn.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))?;
    let gaps: i64 = conn.query_row("SELECT COUNT(*) FROM ingest_gaps", [], |r| r.get(0))?;
    let oldest_retained_timestamp: Option<i64> = conn.query_row(
        "SELECT MIN(timestamp) FROM blocks WHERE body_captured = 1",
        [],
        |r| r.get(0),
    )?;

    Ok(ChainStats {
        last_processed_height,
        indexed_blocks,
        indexed_transactions,
        gaps,
        oldest_retained_timestamp,
    })
}

pub fn recent_gaps(conn: &Connection, limit: i64) -> Result<Vec<GapEntry>> {
    let mut stmt = conn.prepare(
        "SELECT height, hash, detected_at, note FROM ingest_gaps ORDER BY height DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(GapEntry {
            height: row.get(0)?,
            hash: row.get(1)?,
            detected_at: row.get(2)?,
            note: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
