use crate::config::Config;
use crate::rpc::{BlockDetailsInfo, RpcClient};
use permanode_core::db;
use anyhow::Result;
use chrono::Utc;
use log::{info, warn};
use rusqlite::{params, Connection};
use std::thread;
use std::time::Duration;

pub fn run(conn: &Connection, rpc: &RpcClient, cfg: &Config) -> Result<()> {
    let mut cycles: u64 = 0;
    loop {
        if let Err(e) = poll_once(conn, rpc, cfg) {
            warn!("poll cycle failed, will retry: {e:#}");
        }

        cycles += 1;
        if cfg.retention_days > 0 && cycles % cfg.prune_every_cycles == 0 {
            let cutoff = Utc::now().timestamp() - (cfg.retention_days as i64 * 86_400);
            match db::prune_older_than(conn, cutoff) {
                Ok(n) if n > 0 => info!("pruned transaction detail for {n} block(s) older than {} day(s)", cfg.retention_days),
                Ok(_) => {}
                Err(e) => warn!("pruning pass failed: {e:#}"),
            }
        }

        thread::sleep(Duration::from_secs(cfg.poll_interval_seconds));
    }
}

fn poll_once(conn: &Connection, rpc: &RpcClient, cfg: &Config) -> Result<()> {
    let tip = rpc.block_count()?;

    let last_processed: u64 = db::get_state(conn, "last_processed_height")?
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| tip.saturating_sub(1));

    // Ingest any new heights.
    for height in (last_processed + 1)..=tip {
        ingest_height(conn, rpc, height)?;
        db::set_state(conn, "last_processed_height", &height.to_string())?;
    }

    // Re-check a trailing window for reorgs, independent of whether we
    // just ingested new heights this cycle.
    let recheck_from = tip.saturating_sub(cfg.reorg_check_depth);
    for height in recheck_from..=tip {
        recheck_height(conn, rpc, height)?;
    }

    Ok(())
}

fn ingest_height(conn: &Connection, rpc: &RpcClient, height: u64) -> Result<()> {
    let Some(details) = rpc.get_block_details(height)? else {
        // Header not available yet (node not synced this far) - nothing
        // to do, next poll cycle will retry.
        return Ok(());
    };
    store_block(conn, &details)?;
    Ok(())
}

fn recheck_height(conn: &Connection, rpc: &RpcClient, height: u64) -> Result<()> {
    let Some(header) = rpc.get_block_header(height)? else {
        return Ok(());
    };
    let now = Utc::now().to_rfc3339();

    match db::canonical_hash_at(conn, height)? {
        None => {
            // We have no canonical record for this height yet (e.g. it was
            // below our start height) - nothing to reconcile.
        }
        Some((_block_id, known_hash)) if known_hash == header.hash => {
            // Still canonical, nothing to do.
        }
        Some((old_block_id, old_hash)) => {
            warn!(
                "reorg detected at height {height}: {old_hash} is no longer canonical, \
                 new canonical hash is {}",
                header.hash
            );
            db::mark_orphaned(conn, old_block_id, &now)?;
            // Fetch and store the new canonical block body immediately -
            // it may already be gone if the node's retained window is as
            // tight as observed live on 17.09.2026.
            match rpc.get_block_details(height)? {
                Some(details) => store_block(conn, &details)?,
                None => {
                    db::record_gap(
                        conn,
                        height,
                        Some(&header.hash),
                        &now,
                        "reorg: new canonical body already unavailable at recheck time",
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn store_block(conn: &Connection, details: &BlockDetailsInfo) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let h = &details.header;

    // Already recorded with this exact hash? Nothing new to do (idempotent
    // re-poll of an unchanged height).
    if let Some((_, known_hash)) = db::canonical_hash_at(conn, h.height)? {
        if known_hash == h.hash {
            return Ok(());
        }
    }

    let body_captured = details.retained.is_some();
    let (reward, fees, proof_class) = match &details.retained {
        Some(r) => (
            Some(r.reward_micronoid as i64),
            Some(r.total_fees_micronoid.clone()),
            Some(r.proof_class.clone()),
        ),
        None => (None, None, None),
    };

    conn.execute(
        "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner,
            nonce_hex, difficulty_target, proof_class, reward_micronoid, total_fees_micronoid,
            body_captured, first_seen_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
         ON CONFLICT(height, hash) DO NOTHING",
        params![
            h.height as i64,
            h.hash,
            h.prev_hash,
            h.state_root,
            h.tx_root,
            h.timestamp as i64,
            h.miner,
            h.nonce_hex,
            h.difficulty_target,
            proof_class,
            reward,
            fees,
            body_captured as i64,
            now,
        ],
    )?;

    let block_id: i64 = conn.query_row(
        "SELECT id FROM blocks WHERE height = ?1 AND hash = ?2",
        params![h.height as i64, h.hash],
        |row| row.get(0),
    )?;

    conn.execute(
        "INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'canonical', ?2)",
        params![block_id, now],
    )?;

    if !body_captured {
        db::record_gap(
            conn,
            h.height,
            Some(&h.hash),
            &now,
            "body already pruned by node on first ingest attempt",
        )?;
        return Ok(());
    }

    let retained = details.retained.as_ref().unwrap();
    for tx in &retained.transactions {
        conn.execute(
            "INSERT INTO transactions (block_id, position, txid, page_count, fee_micronoid,
                coinbase, development_payout, epoch_anchor, input_owner, input_sum_micronoid,
                output_sum_micronoid)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(block_id, position) DO NOTHING",
            params![
                block_id,
                tx.position as i64,
                tx.txid,
                tx.page_count as i64,
                tx.fee_micronoid as i64,
                tx.coinbase as i64,
                tx.development_payout as i64,
                tx.epoch_anchor,
                tx.input_owner,
                tx.input_sum_micronoid,
                tx.output_sum_micronoid,
            ],
        )?;
        let tx_id: i64 = conn.query_row(
            "SELECT id FROM transactions WHERE block_id = ?1 AND position = ?2",
            params![block_id, tx.position as i64],
            |row| row.get(0),
        )?;

        for (idx, ph) in tx.page_hashes.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO tx_page_hashes (tx_id, idx, page_hash) VALUES (?1,?2,?3)",
                params![tx_id, idx as i64, ph],
            )?;
        }
        for (idx, i) in tx.inputs.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO tx_inputs (tx_id, idx, page, lane, slot_index, amount_micronoid, creation_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![tx_id, idx as i64, i.page as i64, i.lane as i64, i.slot_index as i64, i.amount_micronoid as i64, i.creation_id.to_string()],
            )?;
        }
        for (idx, o) in tx.outputs.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO tx_outputs (tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![tx_id, idx as i64, o.page as i64, o.lane as i64, o.slot_index as i64, o.amount_micronoid as i64, o.owner, o.creation_id.to_string()],
            )?;
        }
    }

    Ok(())
}
