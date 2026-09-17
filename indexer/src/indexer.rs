use crate::config::Config;
use crate::decode;
use crate::rpc::{BlockDetailsInfo, BlockHeaderInfo, RetainedBlockInfo, RpcClient};
use anyhow::Result;
use chrono::Utc;
use log::{error, info, warn};
use permanode_core::db;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Observed width of the node's getBlock serving window (see
/// docs/ANLEITUNG-getblock-decoder.md / node-issue-17-09 REPORT.md
/// section 6: `repro_retained_null.py 42` against a live node). Only used
/// to bound how far back the gap-backfill sweep still bothers looking -
/// anything older than this is permanently gone even via getBlock.
const GETBLOCK_SERVING_WINDOW: u64 = 42;

pub fn run(conn: &Connection, rpc: &RpcClient, cfg: &Config) -> Result<()> {
    let mut cycles: u64 = 0;
    // Guards against two live-state sweeps overlapping if one is still
    // running (tens of thousands of RPC calls per populated segment) when
    // its next trigger comes around - the flag lives for the whole run().
    let slot_scan_running = Arc::new(AtomicBool::new(false));
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

        if cycles % cfg.refresh_addresses_every_cycles == 0 {
            match refresh_known_address_balances(conn, rpc) {
                Ok(n) => info!("refreshed live balance cache for {n} known address(es)"),
                Err(e) => warn!("address balance refresh pass failed: {e:#}"),
            }
        }

        if cfg.scan_slots_every_cycles > 0 && cycles % cfg.scan_slots_every_cycles == 0 {
            if slot_scan_running.swap(true, Ordering::SeqCst) {
                warn!("live state sweep trigger fired but a previous sweep is still running, skipping");
            } else {
                let db_path = cfg.db_path.clone();
                let rpc = rpc.clone();
                let running_flag = Arc::clone(&slot_scan_running);
                // Its own connection (WAL mode allows concurrent readers/
                // writers) so the getSlot flood never holds up the main
                // ingest loop's connection.
                thread::spawn(move || {
                    let result = db::open(&db_path).and_then(|scan_conn| scan_live_state(&scan_conn, &rpc));
                    match result {
                        Ok(n) => info!("live state sweep finished: {n} distinct address(es) with a balance"),
                        Err(e) => warn!("live state sweep failed: {e:#}"),
                    }
                    running_flag.store(false, Ordering::SeqCst);
                });
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
        ingest_height(conn, rpc, cfg, height)?;
        db::set_state(conn, "last_processed_height", &height.to_string())?;
    }

    // Re-check a trailing window for reorgs, independent of whether we
    // just ingested new heights this cycle.
    let recheck_from = tip.saturating_sub(cfg.reorg_check_depth);
    for height in recheck_from..=tip {
        recheck_height(conn, rpc, cfg, height)?;
    }

    // Retry still-open gaps that are still inside the getBlock serving
    // window - a gap recorded a few cycles ago (e.g. getblock_fallback was
    // briefly toggled off, or the node hadn't finished writing the body
    // yet) may be recoverable now even though it wasn't at first ingest.
    if cfg.getblock_fallback {
        let min_height = tip.saturating_sub(GETBLOCK_SERVING_WINDOW);
        for height in db::open_gap_heights(conn, min_height)? {
            backfill_gap(conn, rpc, height)?;
        }
    }

    Ok(())
}

/// What happened when we tried the getBlock fallback for a height whose
/// getBlockDetails came back with `retained: null`.
enum FallbackOutcome {
    Recovered(RetainedBlockInfo),
    /// getBlock also has nothing (outside its serving window, or the node
    /// genuinely never had this body) - not the decoder's fault.
    NoBody,
    /// getBlock had bytes but decode_retained_block rejected them (hash
    /// mismatch from a reorg race between the two RPC calls, or a real
    /// wire-format problem). Already logged by the caller of decode.
    DecodeFailed(String),
}

fn try_getblock_fallback(rpc: &RpcClient, height: u64, expected_hash: &str) -> FallbackOutcome {
    let raw = match rpc.get_block_raw(height) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return FallbackOutcome::NoBody,
        Err(e) => {
            warn!("height {height}: getBlock RPC call failed: {e:#}");
            return FallbackOutcome::NoBody;
        }
    };
    match decode::decode_retained_block(&raw, height, expected_hash) {
        Ok(retained) => FallbackOutcome::Recovered(retained),
        Err(e) => {
            error!("height {height}: getBlock body could not be decoded: {e:#}");
            FallbackOutcome::DecodeFailed(e.to_string())
        }
    }
}

fn ingest_height(conn: &Connection, rpc: &RpcClient, cfg: &Config, height: u64) -> Result<()> {
    let Some(mut details) = rpc.get_block_details(height)? else {
        // Header not available yet (node not synced this far) - nothing
        // to do, next poll cycle will retry.
        return Ok(());
    };

    if details.retained.is_some() {
        store_block(conn, &details, "details")?;
        if cfg.decoder_selfcheck {
            selfcheck(conn, rpc, height, &details);
        }
        return Ok(());
    }

    if cfg.getblock_fallback {
        match try_getblock_fallback(rpc, height, &details.header.hash) {
            FallbackOutcome::Recovered(retained) => {
                let n = retained.transactions.len();
                details.retained = Some(retained);
                store_block(conn, &details, "getblock")?;
                info!("height {height}: body recovered via getBlock ({n} tx)");
                return Ok(());
            }
            FallbackOutcome::NoBody => {
                record_gap_block(
                    conn,
                    &details,
                    "no body via getBlockDetails nor getBlock (outside serving window)",
                )?;
                return Ok(());
            }
            FallbackOutcome::DecodeFailed(err) => {
                record_gap_block(conn, &details, &format!("getBlock body could not be decoded: {err}"))?;
                return Ok(());
            }
        }
    }

    record_gap_block(conn, &details, "body already pruned by node on first ingest attempt")?;
    Ok(())
}

fn recheck_height(conn: &Connection, rpc: &RpcClient, cfg: &Config, height: u64) -> Result<()> {
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

            let Some(mut details) = rpc.get_block_details(height)? else {
                record_gap(conn, height, Some(&header.hash), &now, "reorg: new canonical body already unavailable at recheck time")?;
                return Ok(());
            };

            if details.retained.is_some() {
                store_block(conn, &details, "details")?;
                return Ok(());
            }

            if cfg.getblock_fallback {
                match try_getblock_fallback(rpc, height, &details.header.hash) {
                    FallbackOutcome::Recovered(retained) => {
                        let n = retained.transactions.len();
                        details.retained = Some(retained);
                        store_block(conn, &details, "getblock")?;
                        info!("height {height}: reorg replacement body recovered via getBlock ({n} tx)");
                        return Ok(());
                    }
                    FallbackOutcome::NoBody => {
                        record_gap_block(
                            conn,
                            &details,
                            "no body via getBlockDetails nor getBlock (outside serving window)",
                        )?;
                        return Ok(());
                    }
                    FallbackOutcome::DecodeFailed(err) => {
                        record_gap_block(conn, &details, &format!("getBlock body could not be decoded: {err}"))?;
                        return Ok(());
                    }
                }
            }

            record_gap_block(conn, &details, "reorg: new canonical body already unavailable at recheck time")?;
        }
    }
    Ok(())
}

/// Retries a previously recorded gap that is still inside the getBlock
/// serving window. Leaves the gap open (tries again next cycle) if
/// getBlock still has nothing or decode fails - `ingest_gaps` already has
/// the original detection note, no need to overwrite it on every retry.
fn backfill_gap(conn: &Connection, rpc: &RpcClient, height: u64) -> Result<()> {
    let Some((_block_id, hash)) = db::canonical_hash_at(conn, height)? else {
        return Ok(());
    };
    let Some(uncaptured_id) = db::uncaptured_block_id(conn, height, &hash)? else {
        // Already recovered by ingest_height/recheck_height in the meantime.
        return Ok(());
    };

    match try_getblock_fallback(rpc, height, &hash) {
        FallbackOutcome::Recovered(retained) => {
            let n = retained.transactions.len();
            insert_transactions(conn, uncaptured_id, &retained)?;
            db::mark_body_recovered(conn, uncaptured_id, "getblock")?;
            let now = Utc::now().to_rfc3339();
            db::resolve_gap(conn, height, &now, "recovered via getBlock")?;
            info!("height {height}: gap recovered via getBlock ({n} tx)");
        }
        FallbackOutcome::NoBody | FallbackOutcome::DecodeFailed(_) => {
            // Stays open; already logged by try_getblock_fallback if it
            // was a decode failure. Next cycle will retry until it falls
            // out of the serving window.
        }
    }
    Ok(())
}

/// Runs the fallback decoder against a block whose getBlockDetails call
/// already returned full data, purely to cross-check the decoder's output
/// against the RPC's own - see config.rs `decoder_selfcheck`. Never
/// affects storage; only logs and counts mismatches.
fn selfcheck(conn: &Connection, rpc: &RpcClient, height: u64, details: &BlockDetailsInfo) {
    let raw = match rpc.get_block_raw(height) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return, // already outside the getBlock window somehow; not a mismatch
        Err(e) => {
            warn!("height {height}: selfcheck getBlock call failed: {e:#}");
            return;
        }
    };
    let decoded = match decode::decode_retained_block(&raw, height, &details.header.hash) {
        Ok(d) => d,
        Err(e) => {
            // A hash mismatch here usually means the two RPC calls
            // straddled a reorg, not a decoder bug - don't count it.
            warn!("height {height}: selfcheck decode inconclusive this cycle: {e:#}");
            return;
        }
    };
    let mine = serde_json::to_value(&decoded).ok();
    let theirs = details.retained.as_ref().and_then(|r| serde_json::to_value(r).ok());
    if mine != theirs {
        error!("height {height}: decoder selfcheck MISMATCH against getBlockDetails output");
        if let Err(e) = increment_mismatch_counter(conn) {
            warn!("failed to record selfcheck mismatch counter: {e:#}");
        }
    }
}

fn increment_mismatch_counter(conn: &Connection) -> Result<()> {
    let current: i64 = db::get_state(conn, "decoder_mismatches")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    db::set_state(conn, "decoder_mismatches", &(current + 1).to_string())
}

fn record_gap(conn: &Connection, height: u64, hash: Option<&str>, now: &str, note: &str) -> Result<()> {
    db::record_gap(conn, height, hash, now, note)
}

/// Inserts (or no-ops if already present) the `blocks` row for a header
/// that has no usable body, and records the gap. Idempotent the same way
/// `store_block` is.
fn record_gap_block(conn: &Connection, details: &BlockDetailsInfo, note: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let h = &details.header;

    if db::canonical_hash_at(conn, h.height)?.is_some_and(|(_, known_hash)| known_hash == h.hash) {
        // Row already exists (e.g. from an earlier attempt) - just make
        // sure the gap is on record, in case this note is more specific.
        db::record_gap(conn, h.height, Some(&h.hash), &now, note)?;
        return Ok(());
    }

    insert_block_header_row(conn, h, false, None, &now)?;
    db::record_gap(conn, h.height, Some(&h.hash), &now, note)?;
    Ok(())
}

/// Stores a block whose body IS available (`details.retained` must be
/// `Some`), from whichever source. `body_source` is `"details"` when it
/// came straight from getBlockDetails, `"getblock"` when the fallback
/// decoder had to reconstruct it.
fn store_block(conn: &Connection, details: &BlockDetailsInfo, body_source: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let h = &details.header;
    let retained = details
        .retained
        .as_ref()
        .expect("store_block called without a retained body - caller bug");

    // Already recorded with this exact hash? Nothing new to do (idempotent
    // re-poll of an unchanged height), unless it was previously stored as
    // a gap and we can now upgrade it in place.
    if let Some((_, known_hash)) = db::canonical_hash_at(conn, h.height)? {
        if known_hash == h.hash {
            if let Some(block_id) = db::uncaptured_block_id(conn, h.height, &h.hash)? {
                insert_transactions(conn, block_id, retained)?;
                db::mark_body_recovered(conn, block_id, body_source)?;
                db::resolve_gap(conn, h.height, &now, &format!("recovered via {body_source}"))?;
            }
            return Ok(());
        }
    }

    let block_id = insert_block_header_row(conn, h, true, Some(body_source), &now)?;
    insert_transactions(conn, block_id, retained)?;
    Ok(())
}

fn insert_block_header_row(
    conn: &Connection,
    h: &BlockHeaderInfo,
    body_captured: bool,
    body_source: Option<&str>,
    now: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner,
            nonce_hex, difficulty_target, body_captured, body_source, first_seen_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
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
            body_captured as i64,
            body_source,
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

    Ok(block_id)
}

fn insert_transactions(conn: &Connection, block_id: i64, retained: &RetainedBlockInfo) -> Result<()> {
    conn.execute(
        "UPDATE blocks SET proof_class = ?2, reward_micronoid = ?3, total_fees_micronoid = ?4 WHERE id = ?1",
        params![
            block_id,
            retained.proof_class,
            retained.reward_micronoid as i64,
            retained.total_fees_micronoid,
        ],
    )?;

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

/// Refreshes core::db::address_balance_cache for every address this
/// permanode has ever recorded, straight from the node's Live State
/// (paranoid_getSlotsByOwner) - same mechanism as the address page's live
/// figures, just done proactively for the whole known-address set instead
/// of on demand for one address. One node RPC call per address; a single
/// failure just gets skipped, not fatal to the pass.
fn refresh_known_address_balances(conn: &Connection, rpc: &RpcClient) -> Result<usize> {
    let addresses = db::known_addresses(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut refreshed = 0;
    for address in &addresses {
        let slots = match rpc.get_slots_by_owner(address) {
            Ok(s) => s,
            Err(e) => {
                warn!("address balance refresh: {address}: {e:#}");
                continue;
            }
        };
        let live: Vec<_> = slots.into_iter().filter(|s| !s.empty).collect();
        let balance: u64 = live.iter().map(|s| s.value).sum();
        db::upsert_address_balance_cache(conn, address, &balance.to_string(), live.len() as i64, &now)?;
        refreshed += 1;
    }
    Ok(refreshed)
}

/// Sweep every populated segment of the node's Live State
/// (`paranoid_getSlot` over each 65,536-slot bucket that
/// `paranoid_getStateMap` reports as holding live slots) to discover every
/// address that currently holds a balance - not just ones
/// `known_addresses()` already knows about (which only covers addresses
/// that moved funds in a transaction this permanode has itself recorded
/// since it started indexing).
///
/// The node's allocator does not fill the slot index space contiguously:
/// each zone of 65,536 mints lands in a segment chosen by a permutation
/// (see `noid_chain::consensus::allocator`), and the node's own block
/// template prefers reusing holes in already-populated segments. So the
/// state map, not any range heuristic, decides what gets swept - and its
/// per-segment counts are the exact figure the sweep must reproduce
/// whenever no block landed while it ran. Any shortfall at an unchanged
/// tip is logged as a coverage problem rather than papered over.
///
/// Called from its own background thread spawned in `run()` - never call
/// this on the main ingest connection/loop, it's tens of thousands of
/// sequential RPC round-trips per populated segment.
fn scan_live_state(conn: &Connection, rpc: &RpcClient) -> Result<usize> {
    let tip_before = rpc.block_count()?;
    let map = rpc.get_state_map()?;
    let expected: u64 = map.live_counts.iter().sum();
    let populated: Vec<(usize, u64)> = map
        .live_counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(i, c)| (i, *c))
        .collect();
    info!(
        "live state sweep: {} populated segment(s) of {} ({} slots each), {expected} live slot(s) at #{tip_before}",
        populated.len(),
        map.live_counts.len(),
        map.bucket_capacity
    );

    let mut owners: HashMap<String, (u128, u64)> = HashMap::new();
    let mut queried = 0u64;
    let mut found = 0u64;
    for (segment, count) in &populated {
        let start = *segment as u64 * map.bucket_capacity;
        let mut seg_found = 0u64;
        for idx in start..start + map.bucket_capacity {
            let slot = match rpc.get_slot(idx) {
                Ok(s) => s,
                Err(e) => {
                    warn!("live state sweep: getSlot({idx}) failed, skipping: {e:#}");
                    continue;
                }
            };
            queried += 1;
            if slot.empty {
                continue;
            }
            seg_found += 1;
            let entry = owners.entry(slot.owner).or_insert((0u128, 0u64));
            entry.0 += slot.value as u128;
            entry.1 += 1;
        }
        found += seg_found;
        if seg_found != *count {
            info!("live state sweep: segment {segment} swept {seg_found} live slot(s), state map said {count}");
        }
    }

    let now = Utc::now().to_rfc3339();
    for (address, (total, count)) in &owners {
        db::upsert_address_balance_cache(conn, address, &total.to_string(), *count as i64, &now)?;
    }
    // Bounds from the earlier range-based sweep design; no longer read.
    conn.execute("DELETE FROM indexer_state WHERE key IN ('slot_scan_low', 'slot_scan_high')", [])?;

    let tip_after = rpc.block_count().unwrap_or(tip_before);
    if tip_after == tip_before && found != expected {
        warn!(
            "live state sweep: found {found} live slot(s) but the node reported {expected} at the same height #{tip_before} - the sweep is missing part of the state"
        );
    } else if tip_after == tip_before {
        info!("live state sweep: queried {queried} slot(s), found {found} live slot(s) across {} address(es) - exact match with the node at #{tip_before}", owners.len());
    } else {
        info!(
            "live state sweep: queried {queried} slot(s), found {found} live slot(s) across {} address(es); node said {expected} at #{tip_before}, chain advanced to #{tip_after} during the sweep",
            owners.len()
        );
    }
    Ok(owners.len())
}
