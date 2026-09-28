//! SQLite storage layer. Schema design notes:
//!
//! - `blocks` is append-only: every distinct (height, hash) ever observed
//!   gets its own row, including blocks later reorged out. We never
//!   overwrite a row to change its canonical status.
//! - `block_status_log` records every canonical/orphaned transition with a
//!   timestamp, so a reorg leaves a visible trail instead of silently
//!   erasing evidence that a transaction was once included.
//! - Transaction detail (`transactions`, `tx_inputs`, `tx_outputs`,
//!   `tx_page_hashes`) is what gets pruned after `retention_days`; the
//!   `blocks` header row itself is kept forever (cheap, and mirrors the
//!   node's own permanent header retention).
//! - `ingest_gaps` records any height where we lost the race against the
//!   node's own body pruning — the node's documented 18-block retention
//!   window was NOT reliable in a live check on 17.09.2026 (single-block
//!   gaps observed inside what looked like a retained span, almost
//!   certainly reorg-related) — so treat every gap as expected, not a bug
//!   to silently ignore.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // FULL is SQLite's default, set explicitly because it is what makes a
    // committed block survive a power cut: every commit fsyncs the WAL
    // before returning, and recovery replays only whole, checksummed
    // frames. (NORMAL would keep the file consistent but could drop the
    // last few commits.) Writers must also wrap each logical unit - a
    // block with all its rows - in one transaction, see `write_tx`.
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // The indexer's main loop, its sweep thread and the API all share the
    // file; WAL lets readers run alongside a writer, but two writers still
    // queue - long enough for a whole block insert or sweep commit.
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    init_schema(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Starts a write transaction on a shared `&Connection`. Callers commit
/// with `tx.commit()`; dropping it rolls back, so a crash or error between
/// the statements of one logical unit leaves nothing half-written.
pub fn write_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(conn.unchecked_transaction()?)
}

/// Idempotent schema migrations for columns added after the initial
/// release. Runs on every open; each ALTER is guarded by a
/// PRAGMA table_info check so it's safe against the live systemd-managed
/// database, not just a fresh one from init_schema.
fn migrate(conn: &Connection) -> Result<()> {
    // The indexer, the API server and the sweep each open their own
    // connection, often at the same moment. Taking the write lock up front
    // makes the second opener wait and then see the columns the first one
    // added, instead of both racing into "duplicate column name".
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    migrate_locked(&tx)?;
    tx.commit()?;
    Ok(())
}

fn migrate_locked(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "blocks", "body_source", "TEXT")?;
    add_column_if_missing(conn, "ingest_gaps", "resolved_at", "TEXT")?;
    add_column_if_missing(conn, "ingest_gaps", "resolution", "TEXT")?;
    // Set by the live-state sweep on outputs that are gone from the node's
    // state although no recorded input spent them: the spend happened in
    // a block this permanode has no body for. Balance queries treat them
    // as spent, so "recorded balance" cannot drift above the live one.
    add_column_if_missing(conn, "tx_outputs", "spent_in_gap", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "tx_outputs", "spent_in_gap_at", "TEXT")?;
    // State size at each block, for the emission schedule (subsidy halves
    // per expansion). NULL on rows recorded before this column existed;
    // readers treat that as the genesis value, which every block so far has.
    add_column_if_missing(conn, "blocks", "log_slots", "INTEGER")?;
    // v2 contract calls (bit 0: call, bit 1: closes the contract), read
    // from the raw block bytes because getBlockDetails does not report
    // them; `contracts_scanned` marks v2 blocks whose bytes were checked.
    add_column_if_missing(conn, "transactions", "contract_flags", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "blocks", "contracts_scanned", "INTEGER")?;
    // Live UTXOs the state sweep found in the node that this permanode has
    // no recorded output for - created before it started recording, or in
    // a block it has no body for. Together with the recorded outputs they
    // make up the permanode's full picture of the live state, which lets
    // the sweep compare per-segment counts with the node instead of reading
    // every slot. Replaced segment by segment whenever a segment is swept.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS state_slots (
             slot_index        INTEGER PRIMARY KEY,
             segment           INTEGER NOT NULL,
             creation_id       TEXT NOT NULL,
             owner             TEXT NOT NULL,
             amount_micronoid  INTEGER NOT NULL,
             seen_height       INTEGER NOT NULL,
             seen_at           TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_state_slots_segment ON state_slots(segment);
         CREATE INDEX IF NOT EXISTS idx_state_slots_creation ON state_slots(creation_id);
         CREATE INDEX IF NOT EXISTS idx_outputs_slot ON tx_outputs(slot_index);",
    )?;
    // Unspent-output queries match outputs against inputs by creation_id;
    // without these every such query is outputs x inputs.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_inputs_creation ON tx_inputs(creation_id);
         CREATE INDEX IF NOT EXISTS idx_outputs_creation ON tx_outputs(creation_id);
         CREATE INDEX IF NOT EXISTS idx_blocks_timestamp ON blocks(timestamp);
         CREATE INDEX IF NOT EXISTS idx_tx_contract ON transactions(block_id) WHERE contract_flags != 0;",
    )?;
    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == column);
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl_type}"), [])?;
    }
    Ok(())
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS blocks (
            id                      INTEGER PRIMARY KEY,
            height                  INTEGER NOT NULL,
            hash                    TEXT NOT NULL,
            prev_hash               TEXT NOT NULL,
            state_root              TEXT NOT NULL,
            tx_root                 TEXT NOT NULL,
            timestamp               INTEGER NOT NULL,
            miner                   TEXT NOT NULL,
            nonce_hex               TEXT NOT NULL,
            difficulty_target       TEXT NOT NULL,
            proof_class             TEXT,
            reward_micronoid        INTEGER,
            total_fees_micronoid    TEXT,
            body_captured           INTEGER NOT NULL,
            first_seen_at           TEXT NOT NULL,
            UNIQUE(height, hash)
        );
        CREATE INDEX IF NOT EXISTS idx_blocks_height ON blocks(height);

        CREATE TABLE IF NOT EXISTS block_status_log (
            id           INTEGER PRIMARY KEY,
            block_id     INTEGER NOT NULL REFERENCES blocks(id),
            status       TEXT NOT NULL CHECK(status IN ('canonical','orphaned')),
            observed_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_status_log_block ON block_status_log(block_id);

        CREATE TABLE IF NOT EXISTS transactions (
            id                      INTEGER PRIMARY KEY,
            block_id                INTEGER NOT NULL REFERENCES blocks(id),
            position                INTEGER NOT NULL,
            txid                    TEXT NOT NULL,
            page_count              INTEGER NOT NULL,
            fee_micronoid           INTEGER NOT NULL,
            coinbase                INTEGER NOT NULL,
            development_payout      INTEGER NOT NULL,
            epoch_anchor            TEXT NOT NULL,
            input_owner             TEXT,
            input_sum_micronoid     TEXT NOT NULL,
            output_sum_micronoid    TEXT NOT NULL,
            UNIQUE(block_id, position)
        );
        CREATE INDEX IF NOT EXISTS idx_tx_txid ON transactions(txid);
        CREATE INDEX IF NOT EXISTS idx_tx_block ON transactions(block_id);
        CREATE INDEX IF NOT EXISTS idx_tx_owner ON transactions(input_owner);

        CREATE TABLE IF NOT EXISTS tx_page_hashes (
            tx_id       INTEGER NOT NULL REFERENCES transactions(id),
            idx         INTEGER NOT NULL,
            page_hash   TEXT NOT NULL,
            PRIMARY KEY(tx_id, idx)
        );

        -- creation_id is documented as u64 but live values exceed i64::MAX
        -- (observed values just above 2^63 - looks like a deliberate
        -- high-bit sentinel scheme distinguishing reward-created slots from
        -- ordinary ones). SQLite has no unsigned 64-bit type, so it is
        -- stored as its exact decimal-string representation, same
        -- convention the protocol itself uses for oversized aggregates.
        CREATE TABLE IF NOT EXISTS tx_inputs (
            tx_id               INTEGER NOT NULL REFERENCES transactions(id),
            idx                 INTEGER NOT NULL,
            page                INTEGER NOT NULL,
            lane                INTEGER NOT NULL,
            slot_index          INTEGER NOT NULL,
            amount_micronoid    INTEGER NOT NULL,
            creation_id         TEXT NOT NULL,
            PRIMARY KEY(tx_id, idx)
        );

        CREATE TABLE IF NOT EXISTS tx_outputs (
            tx_id               INTEGER NOT NULL REFERENCES transactions(id),
            idx                 INTEGER NOT NULL,
            page                INTEGER NOT NULL,
            lane                INTEGER NOT NULL,
            slot_index          INTEGER NOT NULL,
            amount_micronoid    INTEGER NOT NULL,
            owner               TEXT NOT NULL,
            creation_id         TEXT NOT NULL,
            PRIMARY KEY(tx_id, idx)
        );
        CREATE INDEX IF NOT EXISTS idx_outputs_owner ON tx_outputs(owner);

        CREATE TABLE IF NOT EXISTS ingest_gaps (
            height       INTEGER PRIMARY KEY,
            hash         TEXT,
            detected_at  TEXT NOT NULL,
            note         TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS indexer_state (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        );

        -- Live balance (paranoid_getSlotsByOwner), periodically refreshed
        -- by the indexer for every address this permanode has ever seen -
        -- not reconstructed from historical transactions (nothing here
        -- overlaps with the pruning-window problem), just a cache of what
        -- the node's current state already says, saving a live RPC round
        -- trip for things like a rich list.
        CREATE TABLE IF NOT EXISTS address_balance_cache (
            address                 TEXT PRIMARY KEY,
            live_balance_micronoid  TEXT NOT NULL,
            live_utxo_count         INTEGER NOT NULL,
            fetched_at              TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

pub fn get_state(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM indexer_state WHERE key = ?1")?;
    let mut rows = stmt.query(params![key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO indexer_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Canonical block hash currently on record for a height, if any.
pub fn canonical_hash_at(conn: &Connection, height: u64) -> Result<Option<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.hash
         FROM blocks b
         JOIN block_status_log s ON s.block_id = b.id
         WHERE b.height = ?1
         AND s.observed_at = (
             SELECT MAX(s2.observed_at) FROM block_status_log s2
             WHERE s2.block_id = b.id
         )
         AND s.status = 'canonical'
         ORDER BY s.id DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![height as i64])?;
    if let Some(row) = rows.next()? {
        Ok(Some((row.get(0)?, row.get(1)?)))
    } else {
        Ok(None)
    }
}

pub fn mark_orphaned(conn: &Connection, block_id: i64, now: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'orphaned', ?2)",
        params![block_id, now],
    )?;
    Ok(())
}

pub fn record_gap(conn: &Connection, height: u64, hash: Option<&str>, now: &str, note: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO ingest_gaps (height, hash, detected_at, note) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(height) DO NOTHING",
        params![height as i64, hash, now, note],
    )?;
    Ok(())
}

/// Marks a previously recorded gap as resolved (e.g. by the getBlock
/// fallback decoder). The row is kept, not deleted - append-only, so the
/// gap's original detection stays visible alongside how it got fixed.
pub fn resolve_gap(conn: &Connection, height: u64, resolved_at: &str, resolution: &str) -> Result<()> {
    conn.execute(
        "UPDATE ingest_gaps SET resolved_at = ?2, resolution = ?3
         WHERE height = ?1 AND resolved_at IS NULL",
        params![height as i64, resolved_at, resolution],
    )?;
    Ok(())
}

/// Heights with an unresolved gap at or above `min_height` - the sweep only
/// bothers with heights still inside the node's getBlock serving window,
/// since anything older is permanently gone.
pub fn open_gap_heights(conn: &Connection, min_height: u64) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT height FROM ingest_gaps WHERE resolved_at IS NULL AND height >= ?1 ORDER BY height",
    )?;
    let rows = stmt.query_map(params![min_height as i64], |row| {
        Ok(row.get::<_, i64>(0)? as u64)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The block_id for an already-known (height, hash) pair whose body was not
/// captured yet, if any - used by the getBlock fallback to find the row it
/// should backfill instead of inserting a duplicate.
pub fn uncaptured_block_id(conn: &Connection, height: u64, hash: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM blocks WHERE height = ?1 AND hash = ?2 AND body_captured = 0",
            params![height as i64, hash],
            |row| row.get(0),
        )
        .optional()?)
}

/// Marks an existing block row's body as now captured (via the getBlock
/// fallback), after its transaction rows have been inserted by the caller.
/// Records the contract calls of the block `(height, hash)` found in its
/// raw bytes and marks the block as scanned. Positions not listed are
/// ordinary transactions.
pub fn set_contract_flags(conn: &Connection, height: u64, hash: &str, flags: &[(u32, u8)]) -> Result<()> {
    let Some(block_id) = conn
        .query_row(
            "SELECT id FROM blocks WHERE height = ?1 AND hash = ?2",
            params![height as i64, hash],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    conn.execute("UPDATE transactions SET contract_flags = 0 WHERE block_id = ?1 AND contract_flags != 0", params![block_id])?;
    for (position, f) in flags {
        conn.execute(
            "UPDATE transactions SET contract_flags = ?3 WHERE block_id = ?1 AND position = ?2",
            params![block_id, *position as i64, *f as i64],
        )?;
    }
    conn.execute("UPDATE blocks SET contracts_scanned = 1 WHERE id = ?1", params![block_id])?;
    Ok(())
}

pub fn mark_body_recovered(conn: &Connection, block_id: i64, body_source: &str) -> Result<()> {
    conn.execute(
        "UPDATE blocks SET body_captured = 1, body_source = ?2 WHERE id = ?1",
        params![block_id, body_source],
    )?;
    Ok(())
}

/// Delete transaction-level detail for blocks older than `cutoff_unix`,
/// keeping the block header row itself. Returns the number of blocks
/// pruned.
pub fn prune_older_than(conn: &Connection, cutoff_unix: i64) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM blocks WHERE timestamp < ?1 AND body_captured = 1",
    )?;
    let block_ids: Vec<i64> = stmt
        .query_map(params![cutoff_unix], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for block_id in &block_ids {
        let tx = write_tx(conn)?;
        tx.execute(
            "DELETE FROM tx_page_hashes WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        tx.execute(
            "DELETE FROM tx_inputs WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        tx.execute(
            "DELETE FROM tx_outputs WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        tx.execute("DELETE FROM transactions WHERE block_id = ?1", params![block_id])?;
        tx.execute(
            "UPDATE blocks SET body_captured = 0 WHERE id = ?1",
            params![block_id],
        )?;
        tx.commit()?;
    }
    Ok(block_ids.len())
}

/// Every distinct address this permanode has ever recorded, as a sender,
/// a receiver, or a block's miner. The set the live-balance cache refresh
/// works through.
pub fn known_addresses(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT input_owner FROM transactions WHERE input_owner IS NOT NULL
         UNION
         SELECT owner FROM tx_outputs
         UNION
         SELECT miner FROM blocks",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn upsert_address_balance_cache(
    conn: &Connection,
    address: &str,
    live_balance_micronoid: &str,
    live_utxo_count: i64,
    fetched_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO address_balance_cache (address, live_balance_micronoid, live_utxo_count, fetched_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(address) DO UPDATE SET
           live_balance_micronoid = excluded.live_balance_micronoid,
           live_utxo_count = excluded.live_utxo_count,
           fetched_at = excluded.fetched_at",
        params![address, live_balance_micronoid, live_utxo_count, fetched_at],
    )?;
    Ok(())
}

const CANONICAL_B: &str = "(SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'";

/// "No input in a canonical block at or below height ?1 spends the coin
/// with creation id `{cid}`."
fn not_spent_by_recorded_input(cid: &str) -> String {
    format!(
        "NOT EXISTS (
           SELECT 1 FROM tx_inputs i
           JOIN transactions t2 ON t2.id = i.tx_id
           JOIN blocks b2 ON b2.id = t2.block_id
           WHERE i.creation_id = {cid} AND b2.height <= ?1
             AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b2.id ORDER BY s.id DESC LIMIT 1) = 'canonical'
         )"
    )
}

/// Recorded outputs on canonical blocks up to `max_height` with a slot
/// index in `slots` that no recorded input up to that height has spent and
/// that aren't flagged yet - the candidates for the sweep's spent-in-gap
/// reconciliation of one segment. Returns (rowid, creation_id).
pub fn unspent_recorded_outputs(conn: &Connection, max_height: u64, slots: std::ops::Range<u64>) -> Result<Vec<(i64, String)>> {
    let sql = format!(
        "SELECT o.rowid, o.creation_id
         FROM tx_outputs o
         JOIN transactions t ON t.id = o.tx_id
         JOIN blocks b ON b.id = t.block_id
         WHERE b.height <= ?1 AND o.slot_index >= ?2 AND o.slot_index < ?3
           AND o.spent_in_gap = 0
           AND {CANONICAL_B}
           AND {}",
        not_spent_by_recorded_input("o.creation_id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![max_height as i64, slots.start as i64, slots.end as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Live UTXOs per state segment as this permanode knows them at `height`:
/// recorded outputs up to that height that no recorded input up to that
/// height spent (and not spent in a gap), plus the sweep's own
/// `state_slots` not spent by a recorded input. Segment = slot index /
/// `segment_size`. Compared against the node's state map at the same
/// height, it tells the sweep which segments need reading.
pub fn live_count_per_segment(conn: &Connection, height: u64, segment_size: u64) -> Result<std::collections::HashMap<u64, u64>> {
    let mut out = std::collections::HashMap::new();
    let recorded = format!(
        "SELECT o.slot_index / ?2 AS seg, COUNT(*)
         FROM tx_outputs o
         JOIN transactions t ON t.id = o.tx_id
         JOIN blocks b ON b.id = t.block_id
         WHERE b.height <= ?1 AND o.spent_in_gap = 0 AND {CANONICAL_B} AND {}
         GROUP BY seg",
        not_spent_by_recorded_input("o.creation_id")
    );
    let foreign = format!(
        "SELECT f.segment, COUNT(*) FROM state_slots f WHERE {} GROUP BY f.segment",
        not_spent_by_recorded_input("f.creation_id")
    );
    for (sql, with_size) in [(recorded, true), (foreign, false)] {
        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64));
        let rows: Vec<(u64, u64)> = if with_size {
            stmt.query_map(params![height as i64, segment_size as i64], map)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map(params![height as i64], map)?.collect::<rusqlite::Result<_>>()?
        };
        for (seg, n) in rows {
            *out.entry(seg).or_insert(0) += n;
        }
    }
    Ok(out)
}

/// Creation ids of every recorded output (canonical block, spent or not)
/// whose slot index lies in `slots` - a live slot of the node with one of
/// these ids is already part of the recorded history.
pub fn recorded_creation_ids(conn: &Connection, slots: std::ops::Range<u64>) -> Result<std::collections::HashSet<String>> {
    let sql = format!(
        "SELECT o.creation_id FROM tx_outputs o
         JOIN transactions t ON t.id = o.tx_id
         JOIN blocks b ON b.id = t.block_id
         WHERE o.slot_index >= ?1 AND o.slot_index < ?2 AND {CANONICAL_B}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![slots.start as i64, slots.end as i64], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One live UTXO as read from the node.
pub struct StateSlot {
    pub slot_index: u64,
    pub creation_id: String,
    pub owner: String,
    pub amount_micronoid: u64,
}

/// Replaces the sweep's own slots of one segment with what a fresh read of
/// that segment found.
pub fn replace_state_slots(conn: &Connection, segment: u64, slots: &[StateSlot], height: u64, now: &str) -> Result<()> {
    conn.execute("DELETE FROM state_slots WHERE segment = ?1", params![segment as i64])?;
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO state_slots (slot_index, segment, creation_id, owner, amount_micronoid, seen_height, seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for s in slots {
        stmt.execute(params![s.slot_index as i64, segment as i64, s.creation_id, s.owner, s.amount_micronoid as i64, height as i64, now])?;
    }
    Ok(())
}

/// Balance and UTXO count of every address in the permanode's picture of
/// the live state (recorded unspent outputs plus the sweep's own slots),
/// as of everything recorded so far.
pub fn live_balances(conn: &Connection) -> Result<std::collections::HashMap<String, (u128, u64)>> {
    let recorded = format!(
        "SELECT o.owner, o.amount_micronoid
         FROM tx_outputs o
         JOIN transactions t ON t.id = o.tx_id
         JOIN blocks b ON b.id = t.block_id
         WHERE b.height <= ?1 AND o.spent_in_gap = 0 AND {CANONICAL_B} AND {}",
        not_spent_by_recorded_input("o.creation_id")
    );
    let foreign = format!("SELECT f.owner, f.amount_micronoid FROM state_slots f WHERE {}", not_spent_by_recorded_input("f.creation_id"));
    let mut out: std::collections::HashMap<String, (u128, u64)> = std::collections::HashMap::new();
    for sql in [recorded, foreign] {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![i64::MAX], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (owner, amount) = row?;
            let e = out.entry(owner).or_insert((0, 0));
            e.0 += amount as u128;
            e.1 += 1;
        }
    }
    Ok(out)
}

/// Sets the cached balance of every address not in `keep` to zero - used
/// once the sweep's picture of the state matches the node exactly, so an
/// address whose last coins were spent in a gap does not keep a stale
/// balance.
pub fn zero_balance_cache_except(conn: &Connection, keep: &std::collections::HashSet<String>, now: &str) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT address FROM address_balance_cache WHERE live_utxo_count > 0")?;
    let stale: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter(|a| !keep.contains(a))
        .collect();
    for a in &stale {
        upsert_address_balance_cache(conn, a, "0", 0, now)?;
    }
    Ok(stale.len())
}

/// Undoes `mark_spent_in_gap` for outputs whose spending transaction has
/// since been recorded (a gap backfilled or imported after the sweep had
/// judged it), so they count as ordinary spent outputs again. Returns how
/// many were cleared.
pub fn clear_spent_in_gap_with_recorded_spend(conn: &Connection) -> Result<usize> {
    // Only a spend in a canonical block counts; orphaned blocks keep their
    // transactions on record and may well spend the same output.
    Ok(conn.execute(
        "UPDATE tx_outputs SET spent_in_gap = 0, spent_in_gap_at = NULL
         WHERE spent_in_gap = 1
           AND EXISTS (
             SELECT 1 FROM tx_inputs i
             JOIN transactions t ON t.id = i.tx_id
             JOIN blocks b ON b.id = t.block_id
             WHERE i.creation_id = tx_outputs.creation_id
               AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'
           )",
        [],
    )?)
}

/// Closes gap entries whose canonical block does have a body after all
/// (recorded through a path that didn't touch `ingest_gaps`, such as a
/// re-ingest after a reorg). Returns how many were closed.
pub fn resolve_gaps_with_bodies(conn: &Connection, now: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE ingest_gaps SET resolved_at = ?1, resolution = 'body on record'
         WHERE resolved_at IS NULL
           AND EXISTS (
             SELECT 1 FROM blocks b
             WHERE b.height = ingest_gaps.height AND b.body_captured = 1
               AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'
           )",
        params![now],
    )?)
}

/// Takes back `mark_spent_in_gap` for recorded outputs in `slots` that a
/// fresh read found live after all (an earlier sweep could not read their
/// slot). Returns how many were taken back.
pub fn unmark_live_outputs(conn: &Connection, slots: std::ops::Range<u64>, live: &std::collections::HashSet<&str>) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT rowid, creation_id FROM tx_outputs WHERE spent_in_gap = 1 AND slot_index >= ?1 AND slot_index < ?2")?;
    let flagged: Vec<(i64, String)> = stmt
        .query_map(params![slots.start as i64, slots.end as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for (rowid, _) in flagged.iter().filter(|(_, cid)| live.contains(cid.as_str())) {
        n += conn.execute("UPDATE tx_outputs SET spent_in_gap = 0, spent_in_gap_at = NULL WHERE rowid = ?1", params![rowid])?;
    }
    Ok(n)
}

pub fn mark_spent_in_gap(conn: &Connection, rowids: &[i64], now: &str) -> Result<()> {
    let mut stmt = conn.prepare("UPDATE tx_outputs SET spent_in_gap = 1, spent_in_gap_at = ?2 WHERE rowid = ?1")?;
    for id in rowids {
        stmt.execute(params![id, now])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct Db {
        conn: Connection,
        path: std::path::PathBuf,
    }

    impl Drop for Db {
        fn drop(&mut self) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.path.display()));
            }
        }
    }

    fn db(name: &str) -> Db {
        let path = std::env::temp_dir().join(format!("permanode-{name}-{}.sqlite3", std::process::id()));
        let conn = open(path.to_str().unwrap()).unwrap();
        Db { conn, path }
    }

    /// A block at `height` with the given status and one transaction:
    /// inputs spend creation ids, outputs are (slot, owner, amount, creation id).
    fn block(c: &Connection, height: i64, hash: &str, status: &str, inputs: &[&str], outputs: &[(i64, &str, i64, &str)]) {
        c.execute(
            "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex, difficulty_target, body_captured, first_seen_at)
             VALUES (?1, ?2, '', '', '', 0, 'o1miner', '', '', 1, '')",
            params![height, hash],
        )
        .unwrap();
        let block_id = c.last_insert_rowid();
        c.execute("INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, ?2, '')", params![block_id, status]).unwrap();
        c.execute(
            "INSERT INTO transactions (block_id, position, txid, page_count, fee_micronoid, coinbase, development_payout, epoch_anchor, input_sum_micronoid, output_sum_micronoid)
             VALUES (?1, 1, ?2, 1, 0, 0, 0, '', '0', '0')",
            params![block_id, format!("tx{hash}")],
        )
        .unwrap();
        let tx_id = c.last_insert_rowid();
        for (i, cid) in inputs.iter().enumerate() {
            c.execute(
                "INSERT INTO tx_inputs (tx_id, idx, page, lane, slot_index, amount_micronoid, creation_id) VALUES (?1, ?2, 0, 0, 0, 0, ?3)",
                params![tx_id, i as i64, cid],
            )
            .unwrap();
        }
        for (i, (slot, owner, amount, cid)) in outputs.iter().enumerate() {
            c.execute(
                "INSERT INTO tx_outputs (tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id) VALUES (?1, ?2, 0, 0, ?3, ?4, ?5, ?6)",
                params![tx_id, i as i64, slot, amount, owner, cid],
            )
            .unwrap();
        }
    }

    fn slot(i: u64, cid: &str, owner: &str, amount: u64) -> StateSlot {
        StateSlot { slot_index: i, creation_id: cid.into(), owner: owner.into(), amount_micronoid: amount }
    }

    #[test]
    fn picture_of_the_live_state() {
        let d = db("state");
        let c = &d.conn;
        const SEG: u64 = 65_536;
        // recorded: A (seg 0) and B (seg 1) at #10; #11 spends A and creates C;
        // an orphaned #11 that spends B must not count; #12 spends the
        // foreign E and creates F.
        block(c, 10, "a10", "canonical", &[], &[(5, "o1x", 100, "1"), (70_000, "o1y", 50, "2")]);
        block(c, 11, "a11", "canonical", &["1"], &[(6, "o1y", 90, "3")]);
        block(c, 11, "b11", "orphaned", &["2"], &[]);
        block(c, 12, "a12", "canonical", &["8"], &[(9, "o1z", 39, "4")]);
        // found by an earlier sweep, no recorded output: D and E
        replace_state_slots(c, 0, &[slot(7, "9", "o1z", 40), slot(8, "8", "o1w", 10)], 9, "t").unwrap();

        let at = |h| live_count_per_segment(c, h, SEG).unwrap();
        assert_eq!(at(10).get(&0), Some(&3), "A + D + E");
        assert_eq!(at(10).get(&1), Some(&1), "B");
        assert_eq!(at(11).get(&0), Some(&3), "C + D + E, A spent");
        assert_eq!(at(11).get(&1), Some(&1), "the orphaned spend of B does not count");
        assert_eq!(at(12).get(&0), Some(&3), "C + D + F, E spent");

        let bal = live_balances(c).unwrap();
        assert_eq!(bal.get("o1y"), Some(&(140, 2)));
        assert_eq!(bal.get("o1z"), Some(&(79, 2)));
        assert_eq!(bal.get("o1x"), None, "spent everything");
        assert_eq!(bal.get("o1w"), None, "its only coin was spent");

        let mut unspent: Vec<String> = unspent_recorded_outputs(c, 12, 0..SEG).unwrap().into_iter().map(|(_, cid)| cid).collect();
        unspent.sort();
        assert_eq!(unspent, vec!["3", "4"]);
        let ids = recorded_creation_ids(c, 0..SEG).unwrap();
        assert_eq!(ids, HashSet::from(["1".to_string(), "3".to_string(), "4".to_string()]));

        // a fresh read of segment 0 replaces the old foreign slots
        replace_state_slots(c, 0, &[slot(7, "9", "o1z", 40)], 12, "t").unwrap();
        assert_eq!(live_count_per_segment(c, 12, SEG).unwrap().get(&0), Some(&3), "C + D + F");
        assert_eq!(live_count_per_segment(c, 12, SEG).unwrap().get(&1), Some(&1));

        // an output wrongly flagged as spent in a gap comes back once read live
        let c_row: i64 = c.query_row("SELECT rowid FROM tx_outputs WHERE creation_id = '3'", [], |r| r.get(0)).unwrap();
        mark_spent_in_gap(c, &[c_row], "t").unwrap();
        assert_eq!(live_count_per_segment(c, 12, SEG).unwrap().get(&0), Some(&2), "C flagged");
        assert_eq!(unmark_live_outputs(c, 0..SEG, &HashSet::from(["3", "9", "4"])).unwrap(), 1);
        assert_eq!(live_count_per_segment(c, 12, SEG).unwrap().get(&0), Some(&3), "C back");

        upsert_address_balance_cache(c, "o1old", "5", 1, "t").unwrap();
        upsert_address_balance_cache(c, "o1y", "140", 2, "t").unwrap();
        let keep: HashSet<String> = HashSet::from(["o1y".to_string()]);
        assert_eq!(zero_balance_cache_except(c, &keep, "t").unwrap(), 1);
        let n: i64 = c.query_row("SELECT live_utxo_count FROM address_balance_cache WHERE address = 'o1old'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
