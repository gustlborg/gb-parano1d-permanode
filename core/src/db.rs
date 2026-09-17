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
    add_column_if_missing(conn, "blocks", "body_source", "TEXT")?;
    add_column_if_missing(conn, "ingest_gaps", "resolved_at", "TEXT")?;
    add_column_if_missing(conn, "ingest_gaps", "resolution", "TEXT")?;
    // Unspent-output queries match outputs against inputs by creation_id;
    // without these every such query is outputs x inputs.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_inputs_creation ON tx_inputs(creation_id);
         CREATE INDEX IF NOT EXISTS idx_outputs_creation ON tx_outputs(creation_id);
         CREATE INDEX IF NOT EXISTS idx_blocks_timestamp ON blocks(timestamp);",
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
