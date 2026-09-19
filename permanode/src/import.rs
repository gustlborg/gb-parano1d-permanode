//! Fills this permanode's gaps from another permanode's database.
//!
//! A gap is a block whose header we have (from our own node) but whose
//! body the node had already pruned by the time we asked - typically
//! after this machine was offline for more than the node's serving
//! window. Another permanode that stayed online has the body; its
//! database (or a backup of it, see contrib/backup) has the same schema,
//! so the rows can be copied straight in through the indexer's own
//! insert path. The imported body is only accepted for a block whose
//! hash matches the header our node gave us.

use crate::indexer::insert_transactions;
use crate::rpc::{BlockTransactionInfo, BlockTransactionInputInfo, BlockTransactionOutputInfo, RetainedBlockInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use log::{info, warn};
use permanode_core::db;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;

#[derive(Debug, Default)]
pub struct ImportReport {
    pub gaps: usize,
    pub imported: usize,
    pub not_in_source: usize,
    pub rejected: usize,
    pub flags_cleared: usize,
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

    let mut report = ImportReport::default();
    let heights = db::open_gap_heights(conn, 0)?;
    report.gaps = heights.len();
    for height in heights {
        let Some((_, hash)) = db::canonical_hash_at(conn, height)? else {
            continue;
        };
        let Some(block_id) = db::uncaptured_block_id(conn, height, &hash)? else {
            continue; // recovered meanwhile
        };
        let body = match load_body(&source, height, &hash) {
            Ok(Some(body)) => body,
            Ok(None) => {
                report.not_in_source += 1;
                continue;
            }
            Err(e) => {
                warn!("height {height}: source body rejected: {e:#}");
                report.rejected += 1;
                continue;
            }
        };
        let n = body.transactions.len();
        let now = Utc::now().to_rfc3339();
        let tx = db::write_tx(conn)?;
        insert_transactions(&tx, block_id, &body)?;
        db::mark_body_recovered(&tx, block_id, "import")?;
        db::resolve_gap(&tx, height, &now, &format!("recovered via import from {source_name}"))?;
        tx.commit()?;
        info!("height {height}: gap recovered via import ({n} tx)");
        report.imported += 1;
    }

    // Outputs the sweep had flagged as "spent in a block without body"
    // whose spending transaction is now on record are ordinary spent
    // outputs again.
    report.flags_cleared = db::clear_spent_in_gap_with_recorded_spend(conn)?;
    Ok(report)
}

/// The body of `(height, hash)` as the source recorded it, or `None` if
/// the source doesn't have it either. Errors mean the source has rows for
/// this block but they don't add up, and the block is left as a gap.
fn load_body(source: &Connection, height: u64, hash: &str) -> Result<Option<RetainedBlockInfo>> {
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

        if page_hashes.len() != page_count as usize {
            bail!("tx {txid}: {} page hashes for {page_count} pages", page_hashes.len());
        }
        let output_total: u64 = outputs.iter().map(|o| o.amount_micronoid).sum();
        if output_total.to_string() != output_sum {
            bail!("tx {txid}: outputs sum to {output_total}, recorded {output_sum}");
        }
        let input_total: u64 = inputs.iter().map(|i| i.amount_micronoid).sum();
        if input_total.to_string() != input_sum {
            bail!("tx {txid}: inputs sum to {input_total}, recorded {input_sum}");
        }
        if !coinbase && !development_payout && input_total != output_total + fee as u64 {
            bail!("tx {txid}: inputs {input_total} != outputs {output_total} + fee {fee}");
        }
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
    if transactions.is_empty() {
        bail!("block row is marked captured but has no transactions");
    }
    if !transactions[0].coinbase {
        bail!("first transaction is not the coinbase");
    }
    let fee_total: u64 = transactions.iter().filter(|t| !t.coinbase).map(|t| t.fee_micronoid).sum();
    if fee_total.to_string() != total_fees {
        bail!("fees sum to {fee_total}, block records {total_fees}");
    }
    let coinbase_total: u64 = transactions[0].outputs.iter().map(|o| o.amount_micronoid).sum();
    if coinbase_total != reward as u64 {
        bail!("coinbase outputs sum to {coinbase_total}, block records {reward}");
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
