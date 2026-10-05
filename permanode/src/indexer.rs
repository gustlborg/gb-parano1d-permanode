use crate::config::Config;
use crate::decode;
use crate::raw;
use crate::rpc::{BlockDetailsInfo, BlockHeaderInfo, RetainedBlockInfo, RpcClient, StateMapInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use log::{error, info, warn};
use permanode_core::db;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Observed width of the node's getBlock serving window (see
/// docs/node-rpc-marker-bug/REPORT.md section 6, measured with
/// `repro_retained_null.py 42` against a live node). Only used
/// to bound how far back the gap-backfill sweep still bothers looking -
/// anything older than this is permanently gone even via getBlock.
pub const GETBLOCK_SERVING_WINDOW: u64 = 42;

pub fn run(conn: &Connection, rpc: &RpcClient, cfg: &Config) -> Result<()> {
    let mut cycles: u64 = 0;
    let closed = db::resolve_gaps_with_bodies(conn, &Utc::now().to_rfc3339())?;
    if closed > 0 {
        info!("{closed} gap entr{} closed: the block has a body on record", if closed == 1 { "y" } else { "ies" });
    }
    // Guards against two live-state sweeps overlapping if one is still
    // running (tens of thousands of RPC calls per populated segment) when
    // its next trigger comes around - the flag lives for the whole run().
    let slot_scan_running = Arc::new(AtomicBool::new(false));
    if cfg.header_backfill_per_second > 0 {
        let db_path = cfg.db_path.clone();
        let rpc = rpc.clone();
        let per_second = cfg.header_backfill_per_second;
        let reorg_check_depth = cfg.reorg_check_depth;
        // Its own connection, like the sweep: its short batch commits
        // queue behind the ingest loop's, never the other way round.
        thread::Builder::new()
            .name("header-backfill".into())
            .spawn(move || header_backfill_thread(&db_path, &rpc, per_second, reorg_check_depth))?;
    }
    if !cfg.backfill_peers.is_empty() {
        let db_path = cfg.db_path.clone();
        let peers = cfg.backfill_peers.clone();
        let every = Duration::from_secs(cfg.peer_backfill_interval_seconds.max(30));
        let keep_raw = cfg.archive_raw_blocks;
        thread::Builder::new().name("peer-backfill".into()).spawn(move || peer_backfill_thread(&db_path, &peers, every, keep_raw))?;
    }
    // The tip watcher wakes the loop below as soon as a new block is on the
    // node and leaves its sighting for the next poll to write; without it
    // the receiver just times out every poll interval.
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let sightings: Sightings = Arc::new(Mutex::new(Vec::new()));
    if cfg.tip_watch_ms > 0 {
        let rpc_url = cfg.rpc_url.clone();
        let every = Duration::from_millis(cfg.tip_watch_ms.max(200));
        let seen = Arc::clone(&sightings);
        thread::Builder::new()
            .name("tip-watch".into())
            .spawn(move || tip_watch_thread(&rpc_url, every, seen, wake_tx))?;
    } else {
        drop(wake_tx);
    }
    loop {
        if let Err(e) = poll_once(conn, rpc, cfg) {
            warn!("poll cycle failed, will retry: {e:#}");
        }
        note_sightings(conn, &sightings);

        cycles += 1;
        if cfg.retention_days > 0 && cycles % cfg.prune_every_cycles == 0 {
            let cutoff = Utc::now().timestamp() - (cfg.retention_days as i64 * 86_400);
            match db::prune_older_than(conn, cutoff) {
                Ok(n) if n > 0 => info!("pruned transaction detail for {n} block(s) older than {} day(s)", cfg.retention_days),
                Ok(_) => {}
                Err(e) => warn!("pruning pass failed: {e:#}"),
            }
        }

        // Both balance jobs also run on the first cycle, so a fresh
        // install has every address and balance within seconds of
        // starting rather than after the first full interval.
        if cycles == 1 || cycles % cfg.refresh_addresses_every_cycles == 0 {
            match refresh_known_address_balances(conn, rpc) {
                Ok(n) => info!("refreshed live balance cache for {n} known address(es)"),
                Err(e) => warn!("address balance refresh pass failed: {e:#}"),
            }
        }

        if cfg.scan_slots_every_cycles > 0 && (cycles == 1 || cycles % cfg.scan_slots_every_cycles == 0) {
            if slot_scan_running.swap(true, Ordering::SeqCst) {
                warn!("live state sweep trigger fired but a previous sweep is still running, skipping");
            } else {
                let db_path = cfg.db_path.clone();
                let rpc = rpc.clone();
                let max_per_second = cfg.state_scan_max_per_second;
                let running_flag = Arc::clone(&slot_scan_running);
                // Its own connection (WAL mode allows concurrent readers/
                // writers) so a segment read never holds up the main
                // ingest loop's connection.
                thread::spawn(move || {
                    let result = db::open(&db_path).and_then(|scan_conn| scan_live_state(&scan_conn, &rpc, max_per_second));
                    match result {
                        Ok(n) => info!("live state sweep finished: {n} distinct address(es) with a balance"),
                        Err(e) => warn!("live state sweep failed: {e:#}"),
                    }
                    running_flag.store(false, Ordering::SeqCst);
                });
            }
        }

        // Wait out the poll interval, but record a new block at once when
        // the tip watcher reports one. The cycle count - and with it the
        // pruning, balance refresh and state sweep cadence - still advances
        // once per poll interval.
        let deadline = Instant::now() + Duration::from_secs(cfg.poll_interval_seconds);
        loop {
            match wake_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => {
                    while wake_rx.try_recv().is_ok() {}
                    if let Err(e) = poll_once(conn, rpc, cfg) {
                        warn!("poll cycle failed, will retry: {e:#}");
                    }
                    note_sightings(conn, &sightings);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    break;
                }
            }
        }
    }
}

/// New tips the watcher saw, `(height, hash, unix ms)`, waiting for their
/// block to be on record.
type Sightings = Arc<Mutex<Vec<(u64, String, i64)>>>;

/// Asks the node for its tip every `every` and notes the moment each new
/// tip block arrived: the closest observable time to when its hash was
/// found - a block's own timestamp is when the pool built its template,
/// typically half a minute earlier. Wakes the indexer, which records the
/// block and then writes the sighting (`note_sightings`), so the block never
/// appears without it. The tip present at start is skipped: when it arrived
/// was not seen.
fn tip_watch_thread(rpc_url: &str, every: Duration, sightings: Sightings, wake: mpsc::Sender<()>) {
    let rpc = permanode_core::live_rpc::RpcClient::new(rpc_url.to_string());
    let mut last: Option<(u64, String)> = None;
    loop {
        if let Ok(info) = rpc.get_chain_info() {
            let tip = (info.height, info.best_hash);
            if last.as_ref() != Some(&tip) {
                if last.is_some() {
                    sightings.lock().unwrap_or_else(|e| e.into_inner()).push((tip.0, tip.1.clone(), Utc::now().timestamp_millis()));
                    let _ = wake.send(());
                }
                last = Some(tip);
            }
        }
        thread::sleep(every);
    }
}

/// Writes the watcher's sightings onto the blocks now on record
/// (`blocks.tip_seen_at_ms`); one whose block is not recorded yet waits for
/// a later poll, for up to ten minutes.
fn note_sightings(conn: &Connection, sightings: &Sightings) {
    let now_ms = Utc::now().timestamp_millis();
    let mut list = sightings.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(height, hash, at)| match db::set_tip_seen(conn, *height, hash, *at) {
        Ok(true) => false,
        Ok(false) => now_ms - at < 600_000,
        Err(e) => {
            warn!("could not note when block {height} arrived: {e:#}");
            now_ms - at < 600_000
        }
    });
}

fn poll_once(conn: &Connection, rpc: &RpcClient, cfg: &Config) -> Result<()> {
    let tip = rpc.block_count()?;

    let last_processed: u64 = match db::get_state(conn, "last_processed_height")?.and_then(|s| s.parse().ok()) {
        Some(h) => h,
        None => {
            let start = first_start_height(rpc, tip);
            info!("fresh database: starting at height {start} (oldest body the node still serves)");
            start.saturating_sub(1)
        }
    };

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
            backfill_gap(conn, rpc, cfg, height)?;
        }
    }

    // Raw bytes that could not be kept at first ingest (getBlock failed,
    // or answered for a block a reorg had just replaced), and on the first
    // run after an upgrade the blocks the node still serves: fetched again
    // while the node has them.
    if cfg.archive_raw_blocks {
        let min_height = tip.saturating_sub(GETBLOCK_SERVING_WINDOW);
        for (height, hash) in db::blocks_missing_raw(conn, min_height, tip, GETBLOCK_SERVING_WINDOW as usize + 1)? {
            if let Some(bytes) = fetch_raw(rpc, height) {
                let tx = db::write_tx(conn)?;
                keep_raw(&tx, cfg, height, &hash, Some(&bytes), "node");
                tx.commit()?;
            }
        }
    }

    Ok(())
}

/// Keeps a block's raw bytes (`archive_raw_blocks`) in the caller's
/// transaction. Bytes that turn out not to be the block on record (getBlock
/// answered after a reorg replaced it) are left out with a warning - the
/// block itself is stored either way, and `poll_once` asks again while the
/// node still serves the body.
fn keep_raw(conn: &Connection, cfg: &Config, height: u64, hash: &str, bytes: Option<&[u8]>, source: &str) {
    if !cfg.archive_raw_blocks {
        return;
    }
    let Some(bytes) = bytes else {
        return;
    };
    if let Err(e) = raw::store(conn, height, hash, bytes, source) {
        warn!("height {height}: raw bytes not kept: {e:#}");
    }
}

/// What happened when we tried the getBlock fallback for a height whose
/// getBlockDetails came back with `retained: null`.
enum FallbackOutcome {
    /// The decoded body and the raw bytes it came from.
    Recovered(RetainedBlockInfo, Vec<u8>),
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
        Ok(retained) => FallbackOutcome::Recovered(retained, raw),
        Err(e) => {
            error!("height {height}: getBlock body could not be decoded: {e:#}");
            FallbackOutcome::DecodeFailed(e.to_string())
        }
    }
}

/// Raw getBlock bytes for `height`, `None` if the node has none (outside
/// its serving window) or the call failed (logged).
fn fetch_raw(rpc: &RpcClient, height: u64) -> Option<Vec<u8>> {
    match rpc.get_block_raw(height) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("height {height}: getBlock RPC call failed: {e:#}");
            None
        }
    }
}

/// Stores the contract calls of a v2 block, read from its raw bytes. Left
/// unscanned (`contracts_scanned` NULL) if the bytes are missing or do not
/// decode - the block itself is stored either way.
pub(crate) fn record_contract_flags(conn: &Connection, height: u64, hash: &str, raw: Option<&[u8]>) -> Result<()> {
    if !permanode_core::emission::v2_active(height) {
        return Ok(());
    }
    let Some(raw) = raw else {
        warn!("height {height}: no raw bytes, contract calls not scanned");
        return Ok(());
    };
    match decode::contract_flags(raw, height, hash) {
        Ok(flags) => {
            if !flags.is_empty() {
                info!("height {height}: {} contract call(s)", flags.len());
            }
            db::set_contract_flags(conn, height, hash, &flags)
        }
        Err(e) => {
            warn!("height {height}: contract scan inconclusive: {e:#}");
            Ok(())
        }
    }
}

/// Where a fresh database begins: the oldest height the node still serves
/// a body for, probed from the tip backwards. Assuming the full serving
/// window would record false gaps on a node that just synced from a
/// snapshot and only holds bodies from that point on.
fn first_start_height(rpc: &RpcClient, tip: u64) -> u64 {
    let mut start = tip;
    for h in (tip.saturating_sub(GETBLOCK_SERVING_WINDOW)..=tip).rev() {
        match rpc.get_block_raw(h) {
            Ok(Some(_)) => start = h,
            _ => break,
        }
    }
    start
}

/// What a height should be written as, decided before any database write
/// so the write itself can be a single transaction.
enum BodyOutcome {
    /// Details with a body, its source, and the raw getBlock bytes if they
    /// were already fetched.
    Store(BlockDetailsInfo, &'static str, Option<Vec<u8>>),
    Gap(BlockDetailsInfo, String),
}

/// Resolves a height's body: getBlockDetails first, then (if enabled) the
/// getBlock fallback decoder. Pure RPC, no database access.
fn resolve_body(rpc: &RpcClient, cfg: &Config, mut details: BlockDetailsInfo, missing_note: &str) -> BodyOutcome {
    if details.retained.is_some() {
        return BodyOutcome::Store(details, "details", None);
    }
    if cfg.getblock_fallback {
        return match try_getblock_fallback(rpc, details.header.height, &details.header.hash) {
            FallbackOutcome::Recovered(retained, raw) => {
                details.retained = Some(retained);
                BodyOutcome::Store(details, "getblock", Some(raw))
            }
            FallbackOutcome::NoBody => BodyOutcome::Gap(
                details,
                "no body via getBlockDetails nor getBlock (outside serving window)".to_string(),
            ),
            FallbackOutcome::DecodeFailed(err) => {
                BodyOutcome::Gap(details, format!("getBlock body could not be decoded: {err}"))
            }
        };
    }
    BodyOutcome::Gap(details, missing_note.to_string())
}

fn ingest_height(conn: &Connection, rpc: &RpcClient, cfg: &Config, height: u64) -> Result<()> {
    let Some(details) = rpc.get_block_details(height)? else {
        // Header not available yet (node not synced this far) - nothing
        // to do, next poll cycle will retry.
        return Ok(());
    };

    match resolve_body(rpc, cfg, details, "body already pruned by node on first ingest attempt") {
        BodyOutcome::Store(details, source, raw) => {
            let wants_raw = cfg.decoder_selfcheck || cfg.archive_raw_blocks || permanode_core::emission::v2_active(height);
            let raw = match raw {
                Some(raw) => Some(raw),
                None if wants_raw => fetch_raw(rpc, height),
                None => None,
            };
            let tx = db::write_tx(conn)?;
            store_block(&tx, &details, source)?;
            record_contract_flags(&tx, height, &details.header.hash, raw.as_deref())?;
            keep_raw(&tx, cfg, height, &details.header.hash, raw.as_deref(), "node");
            tx.commit()?;
            if source == "getblock" {
                info!("height {height}: body recovered via getBlock ({} tx)", details.retained.as_ref().map_or(0, |r| r.transactions.len()));
            } else if cfg.decoder_selfcheck {
                if let Some(raw) = raw.as_deref() {
                    selfcheck(conn, height, &details, raw);
                }
            }
        }
        BodyOutcome::Gap(details, note) => {
            let tx = db::write_tx(conn)?;
            record_gap_block(&tx, &details, &note)?;
            tx.commit()?;
        }
    }
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
        Some((block_id, known_hash)) if db::is_header_only(conn, block_id)? => {
            // A header-only row below the archive with a different hash.
            // The backfill only writes heights deeper than this window and
            // the protocol's reorg limit, so this cannot happen short of a
            // broken node; the row is not part of the archive and must not
            // turn into an orphan plus a gap through this path.
            warn!("height {height}: header-only block {known_hash} on record, the node now reports {} - left untouched", header.hash);
        }
        Some((old_block_id, old_hash)) => {
            warn!(
                "reorg detected at height {height}: {old_hash} is no longer canonical, \
                 new canonical hash is {}",
                header.hash
            );
            // Fetch the replacement before touching the database, so the
            // orphan mark and the replacement land in the same transaction:
            // a failure in between must not leave the height with no
            // canonical block at all.
            let replacement = match rpc.get_block_details(height)? {
                Some(details) => Some(resolve_body(
                    rpc,
                    cfg,
                    details,
                    "reorg: new canonical body already unavailable at recheck time",
                )),
                None => None,
            };

            let tx = db::write_tx(conn)?;
            db::mark_orphaned(&tx, old_block_id, &now)?;
            match replacement {
                Some(BodyOutcome::Store(details, source, raw)) => {
                    let raw = match raw {
                        Some(raw) => Some(raw),
                        None if cfg.archive_raw_blocks || permanode_core::emission::v2_active(height) => fetch_raw(rpc, height),
                        None => None,
                    };
                    store_block(&tx, &details, source)?;
                    record_contract_flags(&tx, height, &details.header.hash, raw.as_deref())?;
                    keep_raw(&tx, cfg, height, &details.header.hash, raw.as_deref(), "node");
                    if source == "getblock" {
                        let n = details.retained.as_ref().map_or(0, |r| r.transactions.len());
                        info!("height {height}: reorg replacement body recovered via getBlock ({n} tx)");
                    }
                }
                Some(BodyOutcome::Gap(details, note)) => record_gap_block(&tx, &details, &note)?,
                None => record_gap(
                    &tx,
                    height,
                    Some(&header.hash),
                    &now,
                    "reorg: new canonical body already unavailable at recheck time",
                )?,
            }
            tx.commit()?;
        }
    }
    Ok(())
}

/// Retries a previously recorded gap that is still inside the getBlock
/// serving window. Leaves the gap open (tries again next cycle) if
/// getBlock still has nothing or decode fails - `ingest_gaps` already has
/// the original detection note, no need to overwrite it on every retry.
fn backfill_gap(conn: &Connection, rpc: &RpcClient, cfg: &Config, height: u64) -> Result<()> {
    let Some((_block_id, hash)) = db::canonical_hash_at(conn, height)? else {
        return Ok(());
    };
    let Some(uncaptured_id) = db::uncaptured_block_id(conn, height, &hash)? else {
        // Already recovered by ingest_height/recheck_height in the meantime.
        return Ok(());
    };

    match try_getblock_fallback(rpc, height, &hash) {
        FallbackOutcome::Recovered(retained, raw) => {
            let n = retained.transactions.len();
            let tx = db::write_tx(conn)?;
            insert_transactions(&tx, uncaptured_id, &retained)?;
            db::mark_body_recovered(&tx, uncaptured_id, "getblock")?;
            record_contract_flags(&tx, height, &hash, Some(&raw))?;
            keep_raw(&tx, cfg, height, &hash, Some(&raw), "node");
            let now = Utc::now().to_rfc3339();
            db::resolve_gap(&tx, height, &now, "recovered via getBlock")?;
            tx.commit()?;
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
fn selfcheck(conn: &Connection, height: u64, details: &BlockDetailsInfo, raw: &[u8]) {
    let decoded = match decode::decode_retained_block(raw, height, &details.header.hash) {
        Ok(d) => d,
        Err(e) => {
            // A hash mismatch here usually means the two RPC calls
            // straddled a reorg, not a decoder bug - don't count it.
            warn!("height {height}: selfcheck decode inconclusive this cycle: {e:#}");
            return;
        }
    };
    let mut mine = serde_json::to_value(&decoded).ok();
    let mut theirs = details.retained.as_ref().and_then(|r| serde_json::to_value(r).ok());
    if permanode_core::emission::v2_active(height) {
        // The v2 proof class lives only in the proof, which getBlock does
        // not carry - the decoder cannot know it, so it is no mismatch.
        for value in [&mut mine, &mut theirs].into_iter().flatten() {
            if let Some(object) = value.as_object_mut() {
                object.remove("proof_class");
            }
        }
    }
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
pub fn record_gap_block(conn: &Connection, details: &BlockDetailsInfo, note: &str) -> Result<()> {
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
pub fn store_block(conn: &Connection, details: &BlockDetailsInfo, body_source: &str) -> Result<()> {
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
            nonce_hex, difficulty_target, body_captured, body_source, first_seen_at, log_slots)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
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
            h.log_slots as i64,
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

pub(crate) fn insert_transactions(conn: &Connection, block_id: i64, retained: &RetainedBlockInfo) -> Result<()> {
    conn.execute(
        // Never replace a known proof class with the node's "unavailable"
        // (v2 proofs are pruned after ~42 blocks); upgrade the other way.
        "UPDATE blocks SET
            proof_class = CASE WHEN ?2 LIKE '%unavailable%' AND proof_class IS NOT NULL THEN proof_class ELSE ?2 END,
            reward_micronoid = ?3, total_fees_micronoid = ?4
         WHERE id = ?1",
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
    let mut fresh: Vec<(String, u128, usize)> = Vec::new();
    for address in addresses {
        let slots = match rpc.get_slots_by_owner(&address) {
            Ok(s) => s,
            Err(e) => {
                warn!("address balance refresh: {address}: {e:#}");
                continue;
            }
        };
        let live: Vec<_> = slots.into_iter().filter(|s| !s.empty).collect();
        let balance: u128 = live.iter().map(|s| u128::from(s.value)).sum();
        fresh.push((address, balance, live.len()));
    }
    let tx = db::write_tx(conn)?;
    for (address, balance, count) in &fresh {
        db::upsert_address_balance_cache(&tx, address, &balance.to_string(), *count as i64, &now)?;
    }
    tx.commit()?;
    Ok(fresh.len())
}

/// Keeps the permanode's picture of the node's Live State - every
/// recorded unspent output plus the live UTXOs it has no recorded output
/// for (`state_slots`: created before it started recording, or in a gap) -
/// in step with the node, and derives every address's balance from it.
///
/// A run reads the node's state map (live UTXOs per 65,536-slot segment)
/// at a height the indexer has fully processed and compares it segment by
/// segment with its own picture at that height. Only segments whose counts
/// differ are read slot by slot (`paranoid_getSlot`); normally none do, so
/// a run costs one node call. The node's allocator scatters UTXOs over
/// many segments (a zone of mints lands in a segment chosen by a
/// permutation, see `noid_chain::consensus::allocator`) - since late
/// September 2026 over 70 of them - so reading every populated segment on
/// every run (4.7 million calls) would keep the node busy for most of the
/// time. The first run after an upgrade, or after a gap, reads the
/// segments that differ once and remembers what it found.
///
/// Reads are throttled to `max_per_second` and pause while the indexer is
/// behind the node, so the sweep never starves block ingestion or the node
/// itself. A segment is only committed after it was read completely; a
/// node that stops answering ends the run (the next one picks up where
/// this one stopped, as finished segments already match).
///
/// The same pass reconciles the recorded history: a recorded output that
/// is gone from a freshly read segment although no recorded input spent it
/// was spent in a block this permanode has no body for.
///
/// Runs in its own background thread spawned in `run()`.
fn scan_live_state(conn: &Connection, rpc: &RpcClient, max_per_second: u64) -> Result<usize> {
    let (height, map) = consistent_state_map(conn, rpc)?;
    let segment_size = map.bucket_capacity;
    let node_total: u64 = map.live_counts.iter().sum();
    let differing = differing_segments(conn, &map, height)?;
    let populated = map.live_counts.iter().filter(|c| **c > 0).count();
    info!(
        "live state sweep at #{height}: node holds {node_total} live UTXO(s) in {populated} segment(s); {} segment(s) differ from the recorded state{}",
        differing.len(),
        if differing.is_empty() { String::new() } else { format!(", reading them at up to {max_per_second} slot(s)/s") }
    );

    let mut limiter = Throttle::new(max_per_second);
    for (segment, node_count, own_count) in &differing {
        let started = Instant::now();
        let start_height = rpc.block_count()?;
        let slots = segment * segment_size..(segment + 1) * segment_size;
        let live = read_segment(conn, rpc, slots.clone(), &mut limiter)?;
        let now = Utc::now().to_rfc3339();
        let tx = db::write_tx(conn)?;
        let recorded = db::recorded_creation_ids(&tx, slots.clone())?;
        let (known, foreign): (Vec<_>, Vec<_>) = live.into_iter().partition(|s| recorded.contains(&s.creation_id));
        db::replace_state_slots(&tx, *segment, &foreign, start_height, &now)?;
        let live_ids: HashSet<&str> = known.iter().chain(foreign.iter()).map(|s| s.creation_id.as_str()).collect();
        let back = db::unmark_live_outputs(&tx, slots.clone(), &live_ids)?;
        let gone: Vec<i64> = db::unspent_recorded_outputs(&tx, start_height, slots)?
            .into_iter()
            .filter(|(_, cid)| !live_ids.contains(cid.as_str()))
            .map(|(rowid, _)| rowid)
            .collect();
        if !gone.is_empty() {
            db::mark_spent_in_gap(&tx, &gone, &now)?;
        }
        tx.commit()?;
        info!(
            "live state sweep: segment {segment} read in {:.0} s (node {node_count}, recorded {own_count}): {} live, {} without a recorded output{}",
            started.elapsed().as_secs_f64(),
            known.len() + foreign.len(),
            foreign.len(),
            if gone.is_empty() { String::new() } else { format!(", {} recorded output(s) spent in a gap", gone.len()) }
        );
        if back > 0 {
            info!("live state sweep: segment {segment}: {back} output(s) earlier marked as spent in a gap are live after all - mark taken back");
        }
    }

    let now = Utc::now().to_rfc3339();
    let tx = db::write_tx(conn)?;
    let cleared = db::clear_spent_in_gap_with_recorded_spend(&tx)?;
    if cleared > 0 {
        info!("live state sweep: {cleared} output(s) marked as spent in a gap now have their spend on record");
    }
    let balances = db::live_balances(&tx)?;
    for (address, (total, count)) in &balances {
        db::upsert_address_balance_cache(&tx, address, &total.to_string(), *count as i64, &now)?;
    }
    tx.execute("DELETE FROM indexer_state WHERE key IN ('slot_scan_low', 'slot_scan_high')", [])?;
    tx.commit()?;

    // Check the result against the node once more.
    let (check_height, check_map) = consistent_state_map(conn, rpc)?;
    let still = differing_segments(conn, &check_map, check_height)?;
    if still.is_empty() {
        let keep: HashSet<String> = balances.keys().cloned().collect();
        let zeroed = db::zero_balance_cache_except(conn, &keep, &now)?;
        info!(
            "live state sweep: every segment matches the node at #{check_height} ({} live UTXO(s), {} address(es) with a balance{})",
            check_map.live_counts.iter().sum::<u64>(),
            balances.len(),
            if zeroed > 0 { format!(", {zeroed} emptied since the last run") } else { String::new() }
        );
    } else {
        let detail: Vec<String> = still.iter().take(5).map(|(s, n, o)| format!("{s}: node {n}, recorded {o}")).collect();
        warn!(
            "live state sweep: {} segment(s) still differ from the node at #{check_height} ({}) - the next run reads them",
            still.len(),
            detail.join("; ")
        );
    }
    Ok(balances.len())
}

/// The node's state map at a height the indexer has fully processed: the
/// map and the tip are read back to back and retried if a block landed in
/// between, and the call waits (up to two minutes) for the indexer to
/// catch up with that tip.
fn consistent_state_map(conn: &Connection, rpc: &RpcClient) -> Result<(u64, StateMapInfo)> {
    for _ in 0..10 {
        let tip = rpc.block_count()?;
        wait_for_indexer(conn, rpc, tip, Duration::from_secs(120))?;
        let map = rpc.get_state_map()?;
        if rpc.block_count()? == tip {
            return Ok((tip, map));
        }
    }
    bail!("the chain kept moving while reading the state map")
}

fn processed_height(conn: &Connection) -> Result<u64> {
    Ok(db::get_state(conn, "last_processed_height")?.and_then(|s| s.parse().ok()).unwrap_or(0))
}

/// The node's tip, as soon as the indexer is at most one block behind it.
/// Background jobs call this between chunks of work so they never compete
/// with block ingestion; `job` names them in the one log line per pause.
fn tip_once_indexer_current(conn: &Connection, rpc: &RpcClient, job: &str) -> Result<u64> {
    let mut paused = false;
    loop {
        let tip = rpc.block_count()?;
        if processed_height(conn)? + 1 >= tip {
            return Ok(tip);
        }
        if !paused {
            info!("{job}: pausing while the indexer catches up with the node (#{tip})");
            paused = true;
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Blocks until the indexer has processed `height`, or fails after `limit`.
fn wait_for_indexer(conn: &Connection, rpc: &RpcClient, height: u64, limit: Duration) -> Result<()> {
    let started = Instant::now();
    while processed_height(conn)? < height {
        if started.elapsed() > limit {
            bail!("the indexer is behind the node (#{} of #{}), sweep postponed", processed_height(conn)?, rpc.block_count().unwrap_or(height));
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// Segments whose live-UTXO count in the node's map differs from the
/// permanode's picture at `height`: (segment, node count, own count).
fn differing_segments(conn: &Connection, map: &StateMapInfo, height: u64) -> Result<Vec<(u64, u64, u64)>> {
    let own = db::live_count_per_segment(conn, height, map.bucket_capacity)?;
    let segments = (map.live_counts.len() as u64).max(own.keys().max().map_or(0, |m| m + 1));
    Ok((0..segments)
        .filter_map(|seg| {
            let node = map.live_counts.get(seg as usize).copied().unwrap_or(0);
            let mine = own.get(&seg).copied().unwrap_or(0);
            (node != mine).then_some((seg, node, mine))
        })
        .collect())
}

/// Spaces calls out to at most `per_second` (0 = unthrottled).
struct Throttle {
    interval: Duration,
    next: Instant,
}

impl Throttle {
    fn new(per_second: u64) -> Self {
        let interval = if per_second == 0 { Duration::ZERO } else { Duration::from_secs_f64(1.0 / per_second as f64) };
        Throttle { interval, next: Instant::now() }
    }

    fn wait(&mut self) {
        let now = Instant::now();
        if self.next > now {
            thread::sleep(self.next - now);
        }
        self.next = self.next.max(now) + self.interval;
    }
}

/// Reads every slot of one segment, returning the live ones. Pauses while
/// the indexer lags more than one block behind the node; a slot that fails
/// five times in a row ends the read with an error, so nothing half-read is
/// ever committed and a node that went away is not hammered further.
fn read_segment(conn: &Connection, rpc: &RpcClient, slots: std::ops::Range<u64>, limiter: &mut Throttle) -> Result<Vec<db::StateSlot>> {
    let mut live = Vec::new();
    for idx in slots {
        if idx % 2_048 == 0 {
            tip_once_indexer_current(conn, rpc, "live state sweep")?;
        }
        let mut attempt = 0;
        let slot = loop {
            limiter.wait();
            match rpc.get_slot(idx) {
                Ok(s) => break s,
                Err(e) if attempt < 4 => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(500 * attempt));
                    if attempt == 4 {
                        warn!("live state sweep: getSlot({idx}) keeps failing: {e:#}");
                    }
                }
                Err(e) => bail!("getSlot({idx}) failed five times, ending this run: {e:#}"),
            }
        };
        if !slot.empty {
            live.push(db::StateSlot { slot_index: idx, creation_id: slot.creation_id.to_string(), owner: slot.owner, amount_micronoid: slot.value });
        }
    }
    Ok(live)
}

// ---------------------------------------------------------------- header backfill

/// Headers fetched per database transaction: the write lock is held for
/// milliseconds, a pause for the indexer takes effect within seconds, and
/// the fsync per commit does not matter.
/// Gaps offered to the peers per round.
const PEER_BACKFILL_BATCH: usize = 500;

/// Offers the open gaps to `peers` every `every`, on its own connection,
/// and with `keep_raw` asks them for the raw bytes this permanode missed
/// (`import::fill_raw_from_peers`). Quiet while nothing changes; says so
/// when a peer becomes unreachable or reachable again.
fn peer_backfill_thread(db_path: &str, peers: &[String], every: Duration, keep_raw: bool) {
    info!("peer backfill: filling gaps from {} every {} s", peers.join(", "), every.as_secs());
    let mut unreachable: Vec<String> = Vec::new();
    loop {
        let round = db::open(db_path).and_then(|conn| {
            let gaps = crate::import::fill_from_peers(&conn, peers, PEER_BACKFILL_BATCH, keep_raw)?;
            let raw = if keep_raw {
                let below = processed_height(&conn)?.saturating_sub(GETBLOCK_SERVING_WINDOW);
                Some(crate::import::fill_raw_from_peers(&conn, peers, below, PEER_BACKFILL_BATCH)?)
            } else {
                None
            };
            Ok((gaps, raw))
        });
        match round {
            Ok((r, raw)) => {
                if r.imported > 0 || r.rejected > 0 {
                    info!("peer backfill: {} gap(s) filled, {} rejected, {} not on any peer, {} open before", r.imported, r.rejected, r.not_in_source, r.gaps);
                }
                if let Some(raw) = raw.filter(|x| x.kept > 0 || x.rejected > 0) {
                    info!("peer backfill: raw bytes of {} block(s) taken over, {} rejected, {} not on any peer", raw.kept, raw.rejected, raw.not_in_source);
                }
                for p in r.unreachable.iter().filter(|p| !unreachable.contains(p)) {
                    warn!("peer backfill: {p} unreachable");
                }
                for p in unreachable.iter().filter(|p| !r.unreachable.contains(p)) {
                    info!("peer backfill: {p} reachable again");
                }
                unreachable = r.unreachable;
            }
            Err(e) => warn!("peer backfill round failed: {e:#}"),
        }
        thread::sleep(every);
    }
}

const HEADER_BATCH: u64 = 200;
/// The protocol's maximum reorg depth is 17; from 18 confirmations on a
/// block can no longer change.
const FINAL_DEPTH: u64 = 18;
/// Progress in `indexer_state`: the next height to copy, or `done`.
const HEADER_BACKFILL_KEY: &str = "header_backfill_next";
/// One progress line per this many headers.
const HEADER_LOG_EVERY: u64 = 10_000;

/// Runs the header backfill until it is complete. After an error (node
/// unreachable, an answer that does not fit) it waits and tries again,
/// longer each time, so a struggling node is never hammered.
fn header_backfill_thread(db_path: &str, rpc: &RpcClient, per_second: u64, reorg_check_depth: u64) {
    let mut delay = Duration::from_secs(60);
    loop {
        let mut progressed = false;
        let result = db::open(db_path).and_then(|conn| backfill_headers(&conn, rpc, per_second, reorg_check_depth, &mut progressed));
        let Err(e) = result else {
            return;
        };
        if progressed {
            delay = Duration::from_secs(60);
        }
        warn!("header backfill stopped, next attempt in {} min: {e:#}", delay.as_secs() / 60);
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(1_800));
    }
}

/// Copies the node's permanent block headers below the archive's first
/// block into `blocks` as header-only rows (`db::HEADER_ONLY_SOURCE`), top
/// down to genesis:
///
/// - at most `per_second` header calls, and paused while the indexer is
///   behind the node (checked before every batch);
/// - only heights deeper than both the reorg re-check window and the
///   protocol's reorg limit, so a header-only row is final when written and
///   never meets the re-check;
/// - every header must be the parent of the block above it (the archive's
///   first block, then the header copied before), so an answer that does
///   not fit ends the run instead of being stored;
/// - each batch is one transaction together with its progress entry, so a
///   restart goes on exactly where the last run stopped and nothing is
///   written twice.
fn backfill_headers(conn: &Connection, rpc: &RpcClient, per_second: u64, reorg_check_depth: u64, progressed: &mut bool) -> Result<()> {
    let archive_first = loop {
        match db::archive_first_height(conn)? {
            Some(h) => break h,
            // a fresh database: wait for the indexer's first block
            None => thread::sleep(Duration::from_secs(30)),
        }
    };
    let state = db::get_state(conn, HEADER_BACKFILL_KEY)?;
    if state.as_deref() == Some("done") {
        return Ok(());
    }
    // The rows from the lowest one on record up to the archive have no
    // holes (whole batches, top down), so the lowest row says where to go
    // on; the progress entry is committed with it and has to agree.
    let lowest = db::lowest_recorded_height(conn)?.unwrap_or(archive_first);
    let Some(mut next) = lowest.checked_sub(1) else {
        db::set_state(conn, HEADER_BACKFILL_KEY, "done")?;
        return Ok(());
    };
    if let Some(s) = state.as_deref().filter(|s| s.parse::<u64>().ok() != Some(next)) {
        warn!("header backfill: progress entry says #{s}, the database #{next} - going on from #{next}");
    }
    let (_, mut expected_hash) =
        db::canonical_link_at(conn, next + 1)?.with_context(|| format!("no canonical block at #{} to start below", next + 1))?;
    if lowest == archive_first {
        info!("header backfill: copying {} header(s) below the archive (#{archive_first}) at up to {per_second}/s", next + 1);
    } else {
        info!("header backfill: going on at #{next}, {} header(s) to go, at up to {per_second}/s", next + 1);
    }

    let mut limiter = Throttle::new(per_second);
    let mut copied: u64 = 0;
    let mut waiting_for_finality = false;
    loop {
        let tip = tip_once_indexer_current(conn, rpc, "header backfill")?;
        let horizon = tip.saturating_sub(reorg_check_depth.max(FINAL_DEPTH) + 1);
        if next > horizon {
            // A young database: the archive still starts inside the
            // re-check window. Its parent will be final in a few blocks.
            if !waiting_for_finality {
                info!("header backfill: waiting until #{next} is final");
                waiting_for_finality = true;
            }
            thread::sleep(Duration::from_secs(60));
            continue;
        }
        let low = next.saturating_sub(HEADER_BATCH - 1);
        let mut batch = Vec::with_capacity((next - low + 1) as usize);
        let mut genesis_missing = false;
        for height in (low..=next).rev() {
            limiter.wait();
            let Some(header) = fetch_header(rpc, height)? else {
                if height == 0 {
                    genesis_missing = true;
                    break;
                }
                bail!("the node has no header at #{height}");
            };
            if header.height != height || header.hash != expected_hash {
                bail!(
                    "the node's header at #{height} ({} at #{}) is not the parent of #{} on record ({expected_hash})",
                    header.hash,
                    header.height,
                    height + 1
                );
            }
            expected_hash = header.prev_hash.clone();
            batch.push(header);
        }

        let done = low == 0;
        let now = Utc::now().to_rfc3339();
        let tx = db::write_tx(conn)?;
        for header in &batch {
            db::insert_header_only_block(&tx, &header_only_block(header), &now)?;
        }
        db::set_state(&tx, HEADER_BACKFILL_KEY, &if done { "done".to_string() } else { (low - 1).to_string() })?;
        tx.commit()?;
        *progressed = true;

        let before = copied;
        copied += batch.len() as u64;
        if done {
            let first = if genesis_missing { 1 } else { 0 };
            info!("header backfill complete: headers from #{first} up to the archive (#{archive_first}) are on record ({copied} copied in this run)");
            if genesis_missing {
                warn!("header backfill: the node serves no header for genesis (#0)");
            }
            return Ok(());
        }
        if copied / HEADER_LOG_EVERY != before / HEADER_LOG_EVERY {
            info!("header backfill: down to #{low}, {low} header(s) to go");
        }
        next = low - 1;
    }
}

/// One header, retried a few times; a node that keeps failing ends the run.
fn fetch_header(rpc: &RpcClient, height: u64) -> Result<Option<BlockHeaderInfo>> {
    let mut attempt = 0;
    loop {
        match rpc.get_block_header(height) {
            Ok(header) => return Ok(header),
            Err(e) if attempt < 4 => {
                attempt += 1;
                thread::sleep(Duration::from_millis(500 * attempt));
                if attempt == 4 {
                    warn!("header backfill: getBlockHeader({height}) keeps failing: {e:#}");
                }
            }
            Err(e) => bail!("getBlockHeader({height}) failed five times: {e:#}"),
        }
    }
}

fn header_only_block(h: &BlockHeaderInfo) -> db::HeaderOnlyBlock {
    db::HeaderOnlyBlock {
        height: h.height,
        hash: h.hash.clone(),
        prev_hash: h.prev_hash.clone(),
        state_root: h.state_root.clone(),
        tx_root: h.tx_root.clone(),
        timestamp: h.timestamp,
        miner: h.miner.clone(),
        nonce_hex: h.nonce_hex.clone(),
        difficulty_target: h.difficulty_target.clone(),
        log_slots: h.log_slots,
    }
}
