//! Read-only queries for the API server. All of these only ever return the
//! currently-canonical version of a block: a block's latest
//! `block_status_log` entry must be `canonical`. Orphaned history is kept
//! in the database but is not what a normal API response should show.
//!
//! Header-only blocks (see `db::HEADER_ONLY_SOURCE`) are canonical blocks
//! too, so block lookups, block lists and mining statistics include them.
//! Everything that describes the recorded archive - its coverage, counts,
//! bodies, fees and burn - excludes them explicitly (`db::ARCHIVED`).
//!
//! Transactions imported from payment receipts into header-only blocks
//! (`db::RECEIPT_SOURCE`) appear on block, transaction and address pages,
//! marked `source: "receipt"`. Balances, counts and the live UTXO figures
//! describe the recorded history and leave them out (`db::RECORDED`).

use crate::db::{recorded_filter_on, ARCHIVED, HEADER_ONLY_SOURCE, RECEIPT_SOURCE};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

const CANONICAL_BLOCK_FILTER: &str = "
    (SELECT s.status FROM block_status_log s
     WHERE s.block_id = blocks.id
     ORDER BY s.id DESC LIMIT 1) = 'canonical'
";

/// `CANONICAL_BLOCK_FILTER` without the status lookup for header-only rows,
/// for scans over the whole chain: the backfill writes them canonical, only
/// for final heights, and the reorg re-check never touches them, so they
/// can never be orphaned (`header_only_rows_leave_the_archive_alone` checks
/// the result against the full filter).
const CANONICAL_OR_HEADER_ONLY: &str = "(blocks.body_source IS 'header' OR
    (SELECT s.status FROM block_status_log s
     WHERE s.block_id = blocks.id
     ORDER BY s.id DESC LIMIT 1) = 'canonical')";

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
    /// False for a header-only block below the archive: only its header is
    /// on record, and `tx_count` counts just the transactions imported from
    /// payment receipts (usually none).
    pub archived: bool,
    /// Transactions in the block, coinbase included, as a payment receipt's
    /// Merkle proof states it; only on header-only blocks with imported
    /// receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_count_total: Option<i64>,
    /// v2 contract calls in this block (0 before the fork).
    pub contract_calls: i64,
    /// When this permanode first saw the block as the node's tip (unix ms):
    /// the closest observable moment to when its hash was found, while
    /// `timestamp` is when the pool built the block's template. `None` for
    /// blocks it did not see arrive (older ones, gap fills, catch-up).
    pub seen_at_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct BlockDetail {
    pub height: i64,
    pub hash: String,
    pub prev_hash: String,
    pub state_root: String,
    pub tx_root: String,
    pub timestamp: i64,
    /// When this permanode first saw the block as the node's tip (unix ms);
    /// see `BlockSummary::seen_at_ms`.
    pub seen_at_ms: Option<i64>,
    pub miner: String,
    pub nonce_hex: String,
    pub difficulty_target: String,
    pub proof_class: Option<String>,
    /// The coinbase value: subsidy plus the fees the miner claimed. `None`
    /// where the body is not on record.
    pub reward_micronoid: Option<i64>,
    pub total_fees_micronoid: Option<String>,
    /// What the consensus rules let this block's coinbase mint before
    /// fees (development shares deducted), from its height and state size;
    /// 0 for genesis, which mints nothing. Known for every block, body or not.
    pub miner_subsidy_micronoid: u64,
    /// Slot-space depth from the header (`log₂` of the State capacity);
    /// `None` on rows recorded before it was kept.
    pub log_slots: Option<u32>,
    /// Live slots after this block, and every live output ever created up
    /// to it (the allocation counter) - header fields the database does not
    /// keep; the API reads them from the node, which keeps every header, or
    /// from the block's kept raw bytes. `None` where neither has them.
    pub active_slot_count: Option<u64>,
    pub alloc_counter: Option<u64>,
    pub body_captured: bool,
    /// False for a header-only block below the archive: the node keeps its
    /// header for ever, but its transactions were never recorded here
    /// (reward and fees are unknown; `transactions` holds only those
    /// imported from payment receipts, marked `source: "receipt"`).
    pub archived: bool,
    /// Where the recorded archive begins; only on header-only blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_from_height: Option<i64>,
    /// Transactions in the block, coinbase included, as a payment receipt's
    /// Merkle proof states it - how many `transactions` would hold if the
    /// block were known in full. Only on header-only blocks with imported
    /// receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_count_total: Option<i64>,
    /// Blocks on top of this one including itself, from the indexer's own
    /// tip; `None` if that isn't known yet. 18 and up is final on this
    /// chain (the protocol's maximum reorg depth is 17).
    pub confirmations: Option<i64>,
    /// False for a block a reorg replaced. Orphaned blocks stay in the
    /// database and can be opened by hash; the explorer's normal views
    /// only show canonical ones.
    pub canonical: bool,
    /// Other blocks this permanode recorded at the same height - the
    /// versions a reorg replaced, or the one that replaced this block.
    pub other_versions: Vec<BlockVersion>,
    pub transactions: Vec<TxSummary>,
}

/// A competing block at the same height, recorded before or after a reorg.
#[derive(Debug, Serialize)]
pub struct BlockVersion {
    pub hash: String,
    pub canonical: bool,
    pub miner: String,
    pub timestamp: i64,
    pub tx_count: i64,
    pub body_captured: bool,
    /// When this permanode last logged a status for it.
    pub observed_at: Option<String>,
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
    /// v2 contract call: `"call"` (the contract continues under its
    /// successor address, the first output) or `"close"`; absent for
    /// ordinary transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<&'static str>,
    /// `"receipt"` for a transaction imported from a payment receipt into a
    /// block below the archive; absent for recorded transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

/// `transactions.source` as the API names it.
fn source_kind(source: Option<String>) -> Option<&'static str> {
    (source.as_deref() == Some(RECEIPT_SOURCE)).then_some(RECEIPT_SOURCE)
}

/// `transactions.contract_flags` as the API names it.
pub fn contract_kind(flags: i64) -> Option<&'static str> {
    match flags {
        0 => None,
        f if f & 2 != 0 => Some("close"),
        _ => Some("call"),
    }
}

#[derive(Debug, Serialize)]
pub struct TxDetail {
    pub txid: String,
    /// `"receipt"`: imported from a payment receipt into a header-only block
    /// below the archive, which this permanode never recorded. Every field
    /// comes from the receipt's pages, which its Merkle proof binds to the
    /// block header; only the creation ids of the outputs are unknown
    /// (null). Absent for recorded transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    pub position: i64,
    /// See `TxSummary::contract`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<&'static str>,
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
    /// The same txid recorded in other blocks - a transaction that
    /// survived a reorg appears once per block it was in. Empty for the
    /// vast majority of transactions.
    pub other_occurrences: Vec<TxOccurrence>,
}

#[derive(Debug, Serialize)]
pub struct TxOccurrence {
    pub height: i64,
    pub block_hash: String,
    pub canonical: bool,
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
    /// Null for the outputs of a transaction imported from a receipt: the
    /// block assigns creation ids when it is applied.
    pub creation_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TxBlockRef {
    pub height: i64,
    pub hash: String,
    pub timestamp: i64,
    pub canonical: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainStats {
    pub last_processed_height: Option<i64>,
    /// Canonical blocks of the recorded archive; header-only blocks below
    /// it are counted separately in `header_only_blocks`.
    pub indexed_blocks: i64,
    /// Transactions recorded from block bodies (the archive), orphaned
    /// blocks included; imported receipts are counted separately.
    pub indexed_transactions: i64,
    /// Transactions imported from payment receipts into header-only blocks
    /// below the archive (`import-receipts`).
    pub receipt_transactions: i64,
    /// First height of the recorded archive (the lowest block recorded as
    /// it arrived, with a body or as a gap).
    pub archive_from_height: Option<i64>,
    /// Blocks below the archive whose header the backfill copied from the
    /// node: canonical, but without any recorded transaction.
    pub header_only_blocks: i64,
    /// Lowest height with a block header on record, header-only or
    /// archived; 0 once the backfill has reached genesis.
    pub headers_from_height: Option<i64>,
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
    /// Oldest block of the archive with a recorded body - where the
    /// transaction history begins ("History since").
    pub oldest_retained_timestamp: Option<i64>,
    /// Transactions in canonical blocks of the last 24 hours (recorded
    /// bodies only).
    pub transactions_24h: i64,
    /// Fees burned by consensus in the last 24 hours, from recorded
    /// blocks: total fees minus what the coinbase claimed on top of the
    /// subsidy. `None` if no block with a body falls into the window.
    pub burned_fees_24h_micronoid: Option<String>,
    /// Addresses holding at least one live UTXO per the last sweep.
    pub addresses_with_balance: i64,
    /// Blocks a reorg replaced. Kept on record instead of overwritten -
    /// that history is one of the reasons this permanode exists.
    pub orphaned_blocks: i64,
    /// Outputs recorded on a canonical block whose creation_id has not
    /// (yet) been consumed by any recorded input - the live UTXO set as
    /// far as this permanode's own indexed history can tell. Same caveat
    /// as an address's confirmed balance: outputs already unspent before
    /// this permanode started recording are invisible to it.
    pub live_utxos: i64,
}

#[derive(Debug, Serialize)]
pub struct OrphanedBlock {
    pub height: i64,
    pub hash: String,
    pub miner: String,
    pub timestamp: i64,
    pub tx_count: i64,
    pub body_captured: bool,
    pub observed_at: Option<String>,
    /// The block that took this height, if this permanode recorded it.
    pub replaced_by: Option<String>,
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

/// Canonical blocks below `before` (all if `None`), newest first. Pages
/// run on from the archive into the header-only blocks below it.
pub fn recent_blocks(conn: &Connection, limit: i64, before: Option<i64>) -> Result<Vec<BlockSummary>> {
    let sql = format!(
        "SELECT blocks.height, blocks.hash, blocks.timestamp, blocks.miner,
                blocks.proof_class, blocks.reward_micronoid, blocks.total_fees_micronoid,
                blocks.body_captured,
                (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id) AS tx_count,
                (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id AND t.contract_flags != 0) AS contract_calls,
                blocks.body_source, blocks.tx_count_total, blocks.tip_seen_at_ms
         FROM blocks
         WHERE blocks.height < ?2 AND {CANONICAL_BLOCK_FILTER}
         ORDER BY blocks.height DESC
         LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, before.unwrap_or(i64::MAX)], |row| {
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
            contract_calls: row.get(9)?,
            archived: row.get::<_, Option<String>>(10)?.as_deref() != Some(HEADER_ONLY_SOURCE),
            tx_count_total: row.get(11)?,
            seen_at_ms: row.get(12)?,
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
                blocks.total_fees_micronoid, blocks.body_captured,
                ({CANONICAL_BLOCK_FILTER}) AS is_canonical,
                blocks.body_source, blocks.log_slots, blocks.tx_count_total, blocks.tip_seen_at_ms
         FROM blocks
         WHERE {where_clause}
         ORDER BY is_canonical DESC
         LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![param], |row| {
            let height: i64 = row.get(1)?;
            let log_slots = row.get::<_, Option<i64>>(16)?.map_or(crate::emission::LOG_SLOTS_GENESIS, |l| l as u32);
            Ok((
                row.get::<_, i64>(0)?,
                BlockDetail {
                    height,
                    hash: row.get(2)?,
                    prev_hash: row.get(3)?,
                    state_root: row.get(4)?,
                    tx_root: row.get(5)?,
                    timestamp: row.get(6)?,
                    seen_at_ms: row.get(18)?,
                    miner: row.get(7)?,
                    nonce_hex: row.get(8)?,
                    difficulty_target: row.get(9)?,
                    proof_class: row.get(10)?,
                    reward_micronoid: row.get(11)?,
                    total_fees_micronoid: row.get(12)?,
                    miner_subsidy_micronoid: miner_subsidy_of(height, log_slots),
                    log_slots: row.get::<_, Option<i64>>(16)?.map(|l| l as u32),
                    active_slot_count: None,
                    alloc_counter: None,
                    body_captured: row.get::<_, i64>(13)? != 0,
                    archived: row.get::<_, Option<String>>(15)?.as_deref() != Some(HEADER_ONLY_SOURCE),
                    archive_from_height: None,
                    tx_count_total: row.get(17)?,
                    confirmations: None,
                    canonical: row.get::<_, i64>(14)? != 0,
                    other_versions: vec![],
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
    t.input_owner, t.input_sum_micronoid, t.output_sum_micronoid, t.contract_flags, t.source,
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
        contract: contract_kind(row.get("contract_flags")?),
        source: source_kind(row.get("source")?),
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

/// Subsidy of the primary coinbase at `height` (see
/// `BlockDetail::miner_subsidy_micronoid`).
fn miner_subsidy_of(height: i64, log_slots: u32) -> u64 {
    match u64::try_from(height) {
        Ok(h) if h > 0 => crate::emission::miner_subsidy(h, log_slots),
        _ => 0,
    }
}

fn archive_from_height(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row(&format!("SELECT MIN(height) FROM blocks WHERE {ARCHIVED}"), [], |r| r.get(0))?)
}

fn finish_block(conn: &Connection, block_id: i64, mut detail: BlockDetail) -> Result<BlockDetail> {
    if !detail.archived {
        detail.archive_from_height = archive_from_height(conn)?;
    }
    detail.transactions = tx_summaries_for_block(conn, block_id)?;
    // A replaced block has no confirmations: it is not on the chain any
    // more, however deep its former height lies.
    detail.confirmations = if detail.canonical { confirmations_for(indexed_tip(conn)?, detail.height) } else { Some(0) };
    detail.other_versions = block_versions_at(conn, detail.height, block_id)?;
    Ok(detail)
}

/// Every other block recorded at `height`, newest status first. Empty for
/// the vast majority of heights; non-empty exactly where a reorg happened.
fn block_versions_at(conn: &Connection, height: i64, except_block_id: i64) -> Result<Vec<BlockVersion>> {
    let sql = format!(
        "SELECT blocks.hash, blocks.miner, blocks.timestamp, blocks.body_captured,
                ({CANONICAL_BLOCK_FILTER}) AS is_canonical,
                (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id) AS tx_count,
                (SELECT s.observed_at FROM block_status_log s WHERE s.block_id = blocks.id ORDER BY s.id DESC LIMIT 1) AS observed_at
         FROM blocks
         WHERE blocks.height = ?1 AND blocks.id <> ?2
         ORDER BY is_canonical DESC, observed_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![height, except_block_id], |row| {
        Ok(BlockVersion {
            hash: row.get("hash")?,
            canonical: row.get::<_, i64>("is_canonical")? != 0,
            miner: row.get("miner")?,
            timestamp: row.get("timestamp")?,
            tx_count: row.get("tx_count")?,
            body_captured: row.get::<_, i64>("body_captured")? != 0,
            observed_at: row.get("observed_at")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Blocks a reorg replaced, newest first - the reorg history this
/// permanode keeps instead of overwriting it.
pub fn orphaned_blocks(conn: &Connection, limit: i64) -> Result<Vec<OrphanedBlock>> {
    let sql = format!(
        "SELECT blocks.height, blocks.hash, blocks.miner, blocks.timestamp, blocks.body_captured,
                (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id) AS tx_count,
                (SELECT s.observed_at FROM block_status_log s WHERE s.block_id = blocks.id ORDER BY s.id DESC LIMIT 1) AS observed_at,
                (SELECT b2.hash FROM blocks b2 WHERE b2.height = blocks.height AND b2.id <> blocks.id
                   AND ({canonical_on_b2}) LIMIT 1) AS replaced_by
         FROM blocks
         WHERE blocks.{ARCHIVED} AND NOT ({CANONICAL_BLOCK_FILTER})
         ORDER BY blocks.height DESC
         LIMIT ?1",
        canonical_on_b2 = canonical_filter_on("b2")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit.clamp(1, 500)], |row| {
        Ok(OrphanedBlock {
            height: row.get("height")?,
            hash: row.get("hash")?,
            miner: row.get("miner")?,
            timestamp: row.get("timestamp")?,
            tx_count: row.get("tx_count")?,
            body_captured: row.get::<_, i64>("body_captured")? != 0,
            observed_at: row.get("observed_at")?,
            replaced_by: row.get("replaced_by")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
                ({canonical_on_b}) AS is_canonical, t.contract_flags, t.source
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
                    source: source_kind(row.get(15)?),
                    position: row.get(1)?,
                    contract: contract_kind(row.get(14)?),
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
                    other_occurrences: vec![],
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

    // The same txid can sit in more than one block: a transaction that
    // survived a reorg was re-mined into the replacement block, and this
    // permanode keeps both.
    let occurrence_sql = format!(
        "SELECT b.height, b.hash, ({canonical_on_b}) AS is_canonical
         FROM transactions t JOIN blocks b ON b.id = t.block_id
         WHERE t.txid = ?1 AND t.id <> ?2
         ORDER BY is_canonical DESC, b.height DESC"
    );
    let mut occ_stmt = conn.prepare(&occurrence_sql)?;
    detail.other_occurrences = occ_stmt
        .query_map(params![txid, tx_id], |row| {
            Ok(TxOccurrence {
                height: row.get("height")?,
                block_hash: row.get("hash")?,
                canonical: row.get::<_, i64>("is_canonical")? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

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
/// the total matching count for pagination. Transactions imported from
/// payment receipts are part of an address's history too, marked
/// `source: "receipt"` (their `address_delta_micronoid` is exact: the
/// receipt holds the input amounts).
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

/// Of `txs_by_address`'s total, the transactions imported from payment
/// receipts (older than the archive, not part of the recorded figures).
pub fn receipt_txs_by_address(conn: &Connection, address: &str) -> Result<i64> {
    // Walks only the imported rows (partial index idx_tx_receipt).
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM transactions t
             WHERE t.source = '{RECEIPT_SOURCE}'
               AND (t.input_owner = ?1 OR EXISTS (SELECT 1 FROM tx_outputs o WHERE o.tx_id = t.id AND o.owner = ?1))"
        ),
        params![address],
        |r| r.get(0),
    )?)
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The canonical-status predicate for the `blocks` table, for callers
/// outside this module that build their own SQL (the CSV export).
pub fn canonical_block_filter() -> String {
    CANONICAL_BLOCK_FILTER.to_string()
}

/// Same predicate for a different table alias.
pub fn canonical_block_filter_on(block_alias: &str) -> String {
    canonical_filter_on(block_alias)
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
    /// Part of `total_sent_micronoid` that came from outputs this permanode
    /// never saw created - they predate its recording. Non-zero means the
    /// address was active before the permanode started, so received/sent
    /// totals cannot add up to the balance.
    pub sent_from_unrecorded_micronoid: String,
}

/// Confirmed balance and UTXO count for `address`, computed from indexed
/// history only. An output counts as unspent if no recorded input anywhere
/// (any address, any block) spends the same `creation_id` - slot_index
/// alone isn't a stable identifier since slots get recycled once spent, but
/// creation_id is unique per creation event. This can only see spends and
/// receipts that happened after this permanode started recording: a
/// balance that already existed before that is not reflected here.
/// Transactions imported from payment receipts are not part of the
/// recorded history and count for none of these figures.
pub fn address_balance(conn: &Connection, address: &str) -> Result<AddressBalance> {
    let canonical_on_b = canonical_filter_on("b");
    let canonical_on_b2 = canonical_filter_on("b2");
    let recorded_t = recorded_filter_on("t");
    let recorded_t2 = recorded_filter_on("t2");

    let total_received_micronoid: String = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(CAST(o.amount_micronoid AS INTEGER)), 0)
             FROM tx_outputs o
             JOIN transactions t ON t.id = o.tx_id
             JOIN blocks b ON b.id = t.block_id
             WHERE o.owner = ?1 AND {recorded_t} AND {canonical_on_b}"
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
         WHERE o.owner = ?1 AND {recorded_t} AND {canonical_on_b} AND o.spent_in_gap = ?2
           AND NOT EXISTS (
             SELECT 1 FROM tx_inputs i
             JOIN transactions t2 ON t2.id = i.tx_id
             JOIN blocks b2 ON b2.id = t2.block_id
             WHERE i.creation_id = o.creation_id AND {recorded_t2} AND {canonical_on_b2}
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
             WHERE t.input_owner = ?1 AND {recorded_t} AND {canonical_on_b}"
        ),
        params![address],
        |row| row.get::<_, i64>(0),
    )?
    .to_string();

    // An imported output never matches here: its creation id is NULL.
    let sent_from_unrecorded_micronoid: String = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(i.amount_micronoid), 0)
                 FROM tx_inputs i
                 JOIN transactions t ON t.id = i.tx_id
                 JOIN blocks b ON b.id = t.block_id
                 WHERE t.input_owner = ?1 AND {recorded_t} AND {canonical_on_b}
                   AND NOT EXISTS (SELECT 1 FROM tx_outputs o WHERE o.creation_id = i.creation_id)"
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
        sent_from_unrecorded_micronoid,
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

    let indexed_blocks: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM blocks WHERE blocks.{ARCHIVED} AND {CANONICAL_BLOCK_FILTER}"),
        [],
        |r| r.get(0),
    )?;
    // All rows minus the imported ones: both counts come from an index
    // alone, a filtered count would read the whole table.
    let receipt_transactions = crate::db::receipt_transaction_count(conn)?;
    let indexed_transactions: i64 =
        conn.query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get::<_, i64>(0))? - receipt_transactions;
    let gaps: i64 =
        conn.query_row("SELECT COUNT(*) FROM ingest_gaps WHERE resolved_at IS NULL", [], |r| r.get(0))?;
    let gaps_resolved: i64 =
        conn.query_row("SELECT COUNT(*) FROM ingest_gaps WHERE resolved_at IS NOT NULL", [], |r| r.get(0))?;
    let oldest_retained_timestamp: Option<i64> = conn.query_row(
        &format!("SELECT MIN(timestamp) FROM blocks WHERE body_captured = 1 AND {ARCHIVED}"),
        [],
        |r| r.get(0),
    )?;
    let archive_from_height = archive_from_height(conn)?;
    // Header-only rows are written canonical and never re-checked (they
    // are final long before the backfill reaches them), so no status
    // lookup is needed to count them.
    let header_only_blocks: i64 =
        conn.query_row("SELECT COUNT(*) FROM blocks WHERE body_source = 'header'", [], |r| r.get(0))?;
    let headers_from_height: Option<i64> = conn.query_row("SELECT MIN(height) FROM blocks", [], |r| r.get(0))?;

    let now_unix = chrono_now();
    let day_ago = now_unix - 86_400;
    let transactions_24h: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM transactions t JOIN blocks b ON b.id = t.block_id
             WHERE b.timestamp >= ?1 AND {} AND {}",
            recorded_filter_on("t"),
            canonical_filter_on("b")
        ),
        params![day_ago],
        |r| r.get(0),
    )?;
    let burned_fees_24h_micronoid = {
        let mut stmt = conn.prepare(&format!(
            "SELECT height, reward_micronoid, CAST(total_fees_micronoid AS INTEGER), log_slots
             FROM blocks WHERE timestamp >= ?1 AND body_captured = 1 AND reward_micronoid IS NOT NULL
               AND {ARCHIVED} AND {CANONICAL_BLOCK_FILTER}"
        ))?;
        let rows = stmt.query_map(params![day_ago], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut any = false;
        let mut burned: i128 = 0;
        for row in rows {
            let (height, reward, fees, log_slots) = row?;
            any = true;
            burned += burned_in_block(height, reward, fees, log_slots);
        }
        any.then(|| burned.to_string())
    };
    let addresses_with_balance: i64 =
        conn.query_row("SELECT COUNT(*) FROM address_balance_cache WHERE live_utxo_count > 0", [], |r| r.get(0))?;
    // Header-only rows are never orphaned (see `CANONICAL_OR_HEADER_ONLY`);
    // leaving them out spares a status lookup for each of them.
    let orphaned_blocks: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM blocks WHERE blocks.{ARCHIVED} AND NOT ({CANONICAL_BLOCK_FILTER})"),
        [],
        |r| r.get(0),
    )?;

    let canonical_on_b = canonical_filter_on("b");
    let canonical_on_b2 = canonical_filter_on("b2");
    let recorded_t = recorded_filter_on("t");
    let recorded_t2 = recorded_filter_on("t2");
    let live_utxos: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*)
             FROM tx_outputs o
             JOIN transactions t ON t.id = o.tx_id
             JOIN blocks b ON b.id = t.block_id
             WHERE {recorded_t} AND {canonical_on_b} AND o.spent_in_gap = 0
               AND NOT EXISTS (
                 SELECT 1 FROM tx_inputs i
                 JOIN transactions t2 ON t2.id = i.tx_id
                 JOIN blocks b2 ON b2.id = t2.block_id
                 WHERE i.creation_id = o.creation_id AND {recorded_t2} AND {canonical_on_b2}
               )"
        ),
        [],
        |r| r.get(0),
    )?;

    Ok(ChainStats {
        last_processed_height,
        indexed_blocks,
        indexed_transactions,
        receipt_transactions,
        archive_from_height,
        header_only_blocks,
        headers_from_height,
        gaps,
        gaps_resolved,
        decoder_mismatches,
        oldest_retained_timestamp,
        transactions_24h,
        burned_fees_24h_micronoid,
        addresses_with_balance,
        orphaned_blocks,
        live_utxos,
    })
}

/// Average interval between canonical blocks recorded in the last
/// `window_seconds`, i.e. (span between oldest and newest block in the
/// window) / (count - 1). `None` if fewer than 2 blocks fall in the
/// window - including, unavoidably, for a window that reaches further
/// back than this permanode has been recording. A chain figure, not one of
/// the archive: header-only blocks below the archive count (their
/// timestamps are the chain's own), so a young permanode with the header
/// backfill done has a full 24-hour figure.
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

/// Fees burned by consensus in one recorded block: what the transactions
/// paid minus what the coinbase claimed on top of the subsidy.
fn burned_in_block(height: i64, reward: i64, fees: i64, log_slots: Option<i64>) -> i128 {
    let subsidy = crate::emission::miner_subsidy(height as u64, log_slots.unwrap_or(24) as u32) as i128;
    (fees as i128 - (reward as i128 - subsidy)).max(0)
}

/// Burn per recorded canonical block, ascending by height. Blocks without
/// a body are absent (their burn is unknown).
pub fn burn_by_block(conn: &Connection) -> Result<Vec<(u64, u128)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT height, reward_micronoid, CAST(total_fees_micronoid AS INTEGER), log_slots
         FROM blocks WHERE body_captured = 1 AND reward_micronoid IS NOT NULL AND {ARCHIVED} AND {CANONICAL_BLOCK_FILTER}
         ORDER BY height"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (height, reward, fees, log_slots) = row?;
        out.push((height as u64, burned_in_block(height, reward, fees, log_slots) as u128));
    }
    Ok(out)
}

/// What the recorded blocks since `since_unix` did to the live state.
#[derive(Debug, Serialize, Default, Clone)]
pub struct StateActivity {
    /// Recorded canonical blocks with a body in the period.
    pub blocks: u64,
    pub from_height: Option<u64>,
    pub to_height: Option<u64>,
    /// Whether the records reach back to the start of the period; if
    /// not, the figures only cover the part since this permanode began
    /// recording.
    pub complete: bool,
    /// Live outputs created (coinbase and development payouts included -
    /// they occupy slots too).
    pub utxos_created: u64,
    /// Live inputs consumed.
    pub utxos_consumed: u64,
    /// Slots given back by transactions with more inputs than outputs.
    pub slots_freed: u64,
    pub burned_micronoid: String,
}

pub fn state_activity(conn: &Connection, since_unix: i64) -> Result<StateActivity> {
    let mut stmt = conn.prepare(&format!(
        "SELECT height, reward_micronoid, CAST(total_fees_micronoid AS INTEGER), log_slots,
                (SELECT COUNT(*) FROM tx_outputs o JOIN transactions t ON t.id = o.tx_id WHERE t.block_id = blocks.id),
                (SELECT COUNT(*) FROM tx_inputs i JOIN transactions t ON t.id = i.tx_id WHERE t.block_id = blocks.id),
                (SELECT COALESCE(SUM(MAX(0,
                     (SELECT COUNT(*) FROM tx_inputs i WHERE i.tx_id = t.id)
                   - (SELECT COUNT(*) FROM tx_outputs o WHERE o.tx_id = t.id))), 0)
                 FROM transactions t WHERE t.block_id = blocks.id)
         FROM blocks
         WHERE timestamp >= ?1 AND body_captured = 1 AND reward_micronoid IS NOT NULL AND {ARCHIVED} AND {CANONICAL_BLOCK_FILTER}
         ORDER BY height"
    ))?;
    let rows = stmt.query_map(params![since_unix], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut a = StateActivity::default();
    let mut burned: i128 = 0;
    for row in rows {
        let (height, reward, fees, log_slots, created, consumed, freed) = row?;
        a.blocks += 1;
        a.from_height.get_or_insert(height as u64);
        a.to_height = Some(height as u64);
        a.utxos_created += created as u64;
        a.utxos_consumed += consumed as u64;
        a.slots_freed += freed as u64;
        burned += burned_in_block(height, reward, fees, log_slots);
    }
    a.burned_micronoid = burned.to_string();
    let oldest: Option<i64> = conn.query_row(
        &format!("SELECT MIN(timestamp) FROM blocks WHERE body_captured = 1 AND {ARCHIVED} AND {CANONICAL_BLOCK_FILTER}"),
        [],
        |r| r.get(0),
    )?;
    a.complete = oldest.is_some_and(|t| t <= since_unix);
    Ok(a)
}

/// Blocks an address mined, over every canonical block on record - the
/// archive and the header-only blocks below it. Genesis is nobody's.
#[derive(Debug, Serialize)]
pub struct BlocksMined {
    pub count: i64,
    pub first_height: Option<i64>,
    pub last_height: Option<i64>,
    /// Lowest height the count covers: 1 once the header backfill has
    /// reached genesis, higher while it is still running (or disabled).
    pub counted_from_height: Option<i64>,
}

pub fn blocks_mined(conn: &Connection, address: &str) -> Result<BlocksMined> {
    let (count, first_height, last_height): (i64, Option<i64>, Option<i64>) = conn.query_row(
        &format!(
            "SELECT COUNT(*), MIN(height), MAX(height) FROM blocks
             WHERE miner = ?1 AND height >= 1 AND {CANONICAL_OR_HEADER_ONLY}"
        ),
        params![address],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(BlocksMined { count, first_height, last_height, counted_from_height: lowest_mined_height(conn)? })
}

/// Lowest height a mining count can cover: the lowest block on record,
/// but never genesis.
fn lowest_mined_height(conn: &Connection) -> Result<Option<i64>> {
    let lowest: Option<i64> = conn.query_row("SELECT MIN(height) FROM blocks", [], |r| r.get(0))?;
    Ok(lowest.map(|h| h.max(1)))
}

#[derive(Debug, Serialize, Clone)]
pub struct MinerEntry {
    pub address: String,
    pub blocks: i64,
    /// Fraction of the period's blocks (0..1).
    pub share: f64,
    pub first_height: i64,
    pub last_height: i64,
}

/// Miners by blocks found since `since_unix` (all of history if `None`).
#[derive(Debug, Serialize, Clone)]
pub struct MinersReport {
    pub since_timestamp: Option<i64>,
    /// Canonical blocks counted (genesis excluded).
    pub blocks: i64,
    pub from_height: Option<i64>,
    pub to_height: Option<i64>,
    /// Whether the blocks on record reach back to the start of the period
    /// (for all of history: to block 1). If not, the figures only cover
    /// the part from `counted_from_height` on.
    pub complete: bool,
    pub counted_from_height: Option<i64>,
    /// Distinct miners in the period; `miners` holds the top `limit`.
    pub miner_count: i64,
    pub miners: Vec<MinerEntry>,
}

pub fn miners(conn: &Connection, since_unix: Option<i64>, limit: usize) -> Result<MinersReport> {
    // The whole chain is counted from the miner index alone (it holds
    // body_source and height); a period walks the timestamp index and
    // groups in a temporary b-tree (`+miner`), which beats visiting the
    // miner index for a small part of it.
    let sql = match since_unix {
        None => format!(
            "SELECT miner, COUNT(*), MIN(height), MAX(height) FROM blocks
             WHERE height >= 1 AND {CANONICAL_OR_HEADER_ONLY}
             GROUP BY miner"
        ),
        Some(_) => format!(
            "SELECT miner, COUNT(*), MIN(height), MAX(height) FROM blocks
             WHERE timestamp >= ?1 AND height >= 1 AND {CANONICAL_OR_HEADER_ONLY}
             GROUP BY +miner"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let row = |r: &rusqlite::Row| {
        Ok(MinerEntry { address: r.get(0)?, blocks: r.get(1)?, share: 0.0, first_height: r.get(2)?, last_height: r.get(3)? })
    };
    let mut miners: Vec<MinerEntry> = match since_unix {
        None => stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?,
        Some(since) => stmt.query_map(params![since], row)?.collect::<rusqlite::Result<_>>()?,
    };
    let blocks: i64 = miners.iter().map(|m| m.blocks).sum();
    for m in &mut miners {
        m.share = if blocks > 0 { m.blocks as f64 / blocks as f64 } else { 0.0 };
    }
    miners.sort_by(|a, b| b.blocks.cmp(&a.blocks).then(b.last_height.cmp(&a.last_height)));
    let from_height = miners.iter().map(|m| m.first_height).min();
    let to_height = miners.iter().map(|m| m.last_height).max();
    let miner_count = miners.len() as i64;
    miners.truncate(limit);
    let counted_from_height = lowest_mined_height(conn)?;
    let complete = match since_unix {
        None => counted_from_height == Some(1),
        Some(since) => {
            let oldest: Option<i64> = conn.query_row("SELECT MIN(timestamp) FROM blocks", [], |r| r.get(0))?;
            oldest.is_some_and(|t| t <= since)
        }
    };
    Ok(MinersReport { since_timestamp: since_unix, blocks, from_height, to_height, complete, counted_from_height, miner_count, miners })
}
