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
use rusqlite::{params, Connection};

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    init_schema(&conn)?;
    Ok(conn)
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
        conn.execute(
            "DELETE FROM tx_page_hashes WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        conn.execute(
            "DELETE FROM tx_inputs WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        conn.execute(
            "DELETE FROM tx_outputs WHERE tx_id IN (SELECT id FROM transactions WHERE block_id = ?1)",
            params![block_id],
        )?;
        conn.execute("DELETE FROM transactions WHERE block_id = ?1", params![block_id])?;
        conn.execute(
            "UPDATE blocks SET body_captured = 0 WHERE id = ?1",
            params![block_id],
        )?;
    }
    Ok(block_ids.len())
}
