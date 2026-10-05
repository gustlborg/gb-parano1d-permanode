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
//! - Below the archive's first block, the header backfill adds the node's
//!   permanent headers as `blocks` rows with `body_source = 'header'`
//!   (see `HEADER_ONLY_SOURCE`): canonical, never a body, never a gap.
//!   They are not part of the recorded archive, and every query about it
//!   (coverage, counts, gaps, balances, export, pruning) excludes them
//!   explicitly with `ARCHIVED` / `archived_filter_on`.
//! - `import-receipts` adds transactions proven by Parano1d payment
//!   receipts to header-only blocks (`transactions.source = 'receipt'`, see
//!   `RECEIPT_SOURCE`), with the receipt itself in `tx_receipts`. They are
//!   not part of the recorded archive either: balances, the UTXO sweep, the
//!   known-address refresh, counts and the export leave them out with
//!   `RECORDED` / `recorded_filter_on`; block, transaction and address
//!   pages show them, marked.

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// `blocks.body_source` of a header-only row: a block below the archive's
/// first block whose header the backfill copied from the node, which keeps
/// every header for ever. Its body was pruned long before this permanode
/// started, so it has no transactions, reward or fees on record.
pub const HEADER_ONLY_SOURCE: &str = "header";

/// SQL predicate on the `blocks` table: the row belongs to the recorded
/// archive, i.e. it is not a header-only row. NULL-safe (rows from before
/// `body_source` existed hold NULL and are archive rows). The partial
/// indexes in `migrate_locked` are defined on exactly this expression, so
/// queries using it stay as fast as before the backfill.
pub const ARCHIVED: &str = "body_source IS NOT 'header'";

/// `ARCHIVED` for a table alias, e.g. `archived_filter_on("b")`.
pub fn archived_filter_on(alias: &str) -> String {
    format!("{alias}.{ARCHIVED}")
}

/// `transactions.source` of a transaction imported from a Parano1d payment
/// receipt (`import-receipts`) into a header-only block. Everything else the
/// permanode stores came from a block body and has `source` NULL.
pub const RECEIPT_SOURCE: &str = "receipt";

/// SQL predicate on the `transactions` table: the row was recorded from a
/// block body (the indexer, a gap backfill, `import-bodies`), not
/// reconstructed from a receipt. NULL-safe like `ARCHIVED`. Every query
/// about the recorded history - balances, the live-state picture, the
/// known-address set, counts, the export - filters with it: a receipt
/// proves one transaction of a block this permanode never recorded, and
/// the creation ids of its outputs are unknown.
pub const RECORDED: &str = "source IS NOT 'receipt'";

/// `RECORDED` for a table alias, e.g. `recorded_filter_on("t")`.
pub fn recorded_filter_on(alias: &str) -> String {
    format!("{alias}.{RECORDED}")
}

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
///
/// The write lock is taken right away (`BEGIN IMMEDIATE`), so writers on
/// different connections - the ingest loop, the sweep, the header
/// backfill - queue for each other through the busy timeout. A deferred
/// transaction that reads first would fail at once with "database is
/// locked" whenever another connection committed between its first read
/// and its first write: in WAL mode its snapshot is then stale and SQLite
/// does not wait.
pub fn write_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?)
}

/// Idempotent schema migrations for columns added after the initial
/// release. Runs on every open; each ALTER is guarded by a
/// PRAGMA table_info check so it's safe against the live systemd-managed
/// database, not just a fresh one from init_schema.
fn migrate(conn: &Connection) -> Result<()> {
    // A table rebuild (`relax_output_creation_id`) follows SQLite's
    // documented procedure, which runs with foreign-key enforcement off.
    // The pragma has no effect inside a transaction, so it is switched
    // around it and always switched back on.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = (|| -> Result<()> {
        // The indexer, the API server and the sweep each open their own
        // connection, often at the same moment. Taking the write lock up
        // front makes the second opener wait and then see the columns the
        // first one added, instead of both racing into "duplicate column
        // name".
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        migrate_locked(&tx)?;
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
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
    // Header backfill: mining statistics look blocks up by miner (with
    // body_source and height in the index they are counted from the index
    // alone), and the archive's own figures (first height, oldest body,
    // block count) must not have to step over the header-only rows below
    // the archive. The partial indexes use the exact `ARCHIVED` expression
    // so the planner can pick them for every query that filters with it;
    // body_source in them makes them cover such a filter, otherwise the
    // planner prefers walking the whole miner index.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_blocks_miner ON blocks(miner, body_source, height);
         CREATE INDEX IF NOT EXISTS idx_blocks_header_only ON blocks(height) WHERE body_source = 'header';
         CREATE INDEX IF NOT EXISTS idx_blocks_archive_height ON blocks(height, body_source) WHERE body_source IS NOT 'header';
         CREATE INDEX IF NOT EXISTS idx_blocks_archive_timestamp ON blocks(timestamp, body_source) WHERE body_source IS NOT 'header';",
    )?;
    // Receipt imports: a transaction proven by a payment receipt, stored in
    // its header-only block with `source = 'receipt'` (see `RECORDED`), the
    // receipt kept in `tx_receipts` so it can be verified again, and the
    // block's transaction count as the receipt's Merkle proof binds it in
    // `blocks.tx_count_total` (NULL everywhere else). The partial index
    // makes counting them, and leaving them out, cost nothing.
    add_column_if_missing(conn, "transactions", "source", "TEXT")?;
    add_column_if_missing(conn, "blocks", "tx_count_total", "INTEGER")?;
    // When this permanode first saw the block as the node's tip (unix ms),
    // noted by the tip watcher: the closest observable moment to when its
    // hash was found. NULL for blocks it did not see arrive.
    add_column_if_missing(conn, "blocks", "tip_seen_at_ms", "INTEGER")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tx_receipts (
             tx_id        INTEGER PRIMARY KEY REFERENCES transactions(id),
             receipt_hex  TEXT NOT NULL,
             imported_at  TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_tx_receipt ON transactions(block_id) WHERE source = 'receipt';",
    )?;
    relax_output_creation_id(conn)?;
    // Raw block archive (`archive_raw_blocks`): each block's bytes as the
    // node served them, compressed (`codec`), with their length before
    // compression; `source` says where they came from (node, peer,
    // import). One row per block row, so a block that was reorged away
    // keeps its bytes as well.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS raw_blocks (
             block_id   INTEGER PRIMARY KEY REFERENCES blocks(id),
             codec      TEXT NOT NULL,
             raw_len    INTEGER NOT NULL,
             data       BLOB NOT NULL,
             source     TEXT NOT NULL,
             stored_at  TEXT NOT NULL
         );",
    )?;
    Ok(())
}

fn column_is_not_null(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let flags: Vec<(String, i64)> = stmt.query_map([], |row| Ok((row.get(1)?, row.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(flags.iter().any(|(name, notnull)| name == column && *notnull != 0))
}

/// The creation id of an output is assigned when its block is applied (a
/// running counter over every output of the block), so a transaction
/// reconstructed from a payment receipt cannot know it: such outputs store
/// NULL. Databases from before receipt imports declared the column NOT
/// NULL, and SQLite cannot drop a constraint in place, so the table is
/// rebuilt once, the documented way (sqlite.org/lang_altertable.html,
/// "Making Other Kinds Of Table Schema Changes"): create the new table,
/// copy every row with its rowid, drop the old one, rename, recreate the
/// indexes - all inside the migration's transaction, so a failure leaves
/// the old table as it was. Nothing references `tx_outputs`, and the
/// column keeps its type and every other constraint.
fn relax_output_creation_id(conn: &Connection) -> Result<()> {
    if !column_is_not_null(conn, "tx_outputs", "creation_id")? {
        return Ok(());
    }
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM tx_outputs", [], |r| r.get(0))?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS tx_outputs_new;
         CREATE TABLE tx_outputs_new (
             tx_id               INTEGER NOT NULL REFERENCES transactions(id),
             idx                 INTEGER NOT NULL,
             page                INTEGER NOT NULL,
             lane                INTEGER NOT NULL,
             slot_index          INTEGER NOT NULL,
             amount_micronoid    INTEGER NOT NULL,
             owner               TEXT NOT NULL,
             creation_id         TEXT,
             spent_in_gap        INTEGER NOT NULL DEFAULT 0,
             spent_in_gap_at     TEXT,
             PRIMARY KEY(tx_id, idx)
         );
         INSERT INTO tx_outputs_new (rowid, tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id, spent_in_gap, spent_in_gap_at)
             SELECT rowid, tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id, spent_in_gap, spent_in_gap_at FROM tx_outputs;",
    )?;
    let copied: i64 = conn.query_row("SELECT COUNT(*) FROM tx_outputs_new", [], |r| r.get(0))?;
    if copied != rows {
        bail!("tx_outputs rebuild copied {copied} of {rows} rows");
    }
    conn.execute_batch(
        "DROP TABLE tx_outputs;
         ALTER TABLE tx_outputs_new RENAME TO tx_outputs;
         CREATE INDEX IF NOT EXISTS idx_outputs_owner ON tx_outputs(owner);
         CREATE INDEX IF NOT EXISTS idx_outputs_creation ON tx_outputs(creation_id);
         CREATE INDEX IF NOT EXISTS idx_outputs_slot ON tx_outputs(slot_index);",
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

        -- creation_id is NULL only for outputs of a transaction imported
        -- from a payment receipt (see relax_output_creation_id).
        CREATE TABLE IF NOT EXISTS tx_outputs (
            tx_id               INTEGER NOT NULL REFERENCES transactions(id),
            idx                 INTEGER NOT NULL,
            page                INTEGER NOT NULL,
            lane                INTEGER NOT NULL,
            slot_index          INTEGER NOT NULL,
            amount_micronoid    INTEGER NOT NULL,
            owner               TEXT NOT NULL,
            creation_id         TEXT,
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

/// Notes when this permanode first saw a block as the node's tip (unix ms);
/// kept once - a later sighting (a reorg back to it) changes nothing.
/// Returns whether the block is on record yet.
pub fn set_tip_seen(conn: &Connection, height: u64, hash: &str, at_ms: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE blocks SET tip_seen_at_ms = COALESCE(tip_seen_at_ms, ?3) WHERE height = ?1 AND hash = ?2",
        params![height as i64, hash, at_ms],
    )?;
    Ok(n > 0)
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

/// Hash and parent hash of the canonical block on record at `height`.
pub fn canonical_link_at(conn: &Connection, height: u64) -> Result<Option<(String, String)>> {
    let Some((block_id, hash)) = canonical_hash_at(conn, height)? else {
        return Ok(None);
    };
    let prev: String = conn.query_row("SELECT prev_hash FROM blocks WHERE id = ?1", params![block_id], |r| r.get(0))?;
    Ok(Some((hash, prev)))
}

/// Whether the row `block_id` is a header-only block from the backfill.
pub fn is_header_only(conn: &Connection, block_id: i64) -> Result<bool> {
    let source: Option<String> = conn.query_row("SELECT body_source FROM blocks WHERE id = ?1", params![block_id], |r| r.get(0))?;
    Ok(source.as_deref() == Some(HEADER_ONLY_SOURCE))
}

/// First height of the recorded archive: the lowest block this permanode
/// recorded as it arrived (with a body or as a gap). Header-only rows lie
/// below it. `None` before the first block is recorded.
pub fn archive_first_height(conn: &Connection) -> Result<Option<u64>> {
    let h: Option<i64> = conn.query_row(&format!("SELECT MIN(height) FROM blocks WHERE {ARCHIVED}"), [], |r| r.get(0))?;
    Ok(h.map(|h| h as u64))
}

/// Lowest height with any block on record, header-only rows included.
pub fn lowest_recorded_height(conn: &Connection) -> Result<Option<u64>> {
    let h: Option<i64> = conn.query_row("SELECT MIN(height) FROM blocks", [], |r| r.get(0))?;
    Ok(h.map(|h| h as u64))
}

/// A block header as the node keeps it for ever (`paranoid_getBlockHeader`).
#[derive(Debug, Clone)]
pub struct HeaderOnlyBlock {
    pub height: u64,
    pub hash: String,
    pub prev_hash: String,
    pub state_root: String,
    pub tx_root: String,
    pub timestamp: u64,
    pub miner: String,
    pub nonce_hex: String,
    pub difficulty_target: String,
    pub log_slots: u32,
}

/// Records `b` as a header-only canonical block below the archive.
/// Idempotent: returns `false` and writes nothing if the same block is on
/// record already. A different block at that height is an error - the
/// backfill only ever runs below the archive, where nothing else is stored.
/// Reward and fees stay NULL: the stored reward is the coinbase value
/// (subsidy plus the fees the miner claimed), and the fees are unknown
/// without the body.
pub fn insert_header_only_block(conn: &Connection, b: &HeaderOnlyBlock, now: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT hash FROM blocks WHERE height = ?1")?;
    let existing: Vec<String> = stmt.query_map(params![b.height as i64], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if existing.contains(&b.hash) {
        return Ok(false);
    }
    if let Some(other) = existing.first() {
        bail!("height {}: block {other} is on record, the node's header says {}", b.height, b.hash);
    }
    conn.execute(
        "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex,
            difficulty_target, body_captured, body_source, first_seen_at, log_slots)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12)",
        params![
            b.height as i64,
            b.hash,
            b.prev_hash,
            b.state_root,
            b.tx_root,
            b.timestamp as i64,
            b.miner,
            b.nonce_hex,
            b.difficulty_target,
            HEADER_ONLY_SOURCE,
            now,
            b.log_slots as i64,
        ],
    )?;
    let block_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'canonical', ?2)",
        params![block_id, now],
    )?;
    Ok(true)
}

/// The canonical block on record at a height, with what a receipt is
/// checked against.
#[derive(Debug, Clone)]
pub struct CanonicalBlock {
    pub id: i64,
    pub hash: String,
    pub tx_root: String,
    pub timestamp: u64,
    pub body_captured: bool,
    pub header_only: bool,
}

pub fn canonical_block_at(conn: &Connection, height: u64) -> Result<Option<CanonicalBlock>> {
    let Some((id, hash)) = canonical_hash_at(conn, height)? else {
        return Ok(None);
    };
    let (tx_root, timestamp, body_captured, source): (String, i64, i64, Option<String>) = conn.query_row(
        "SELECT tx_root, timestamp, body_captured, body_source FROM blocks WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(Some(CanonicalBlock {
        id,
        hash,
        tx_root,
        timestamp: timestamp as u64,
        body_captured: body_captured != 0,
        header_only: source.as_deref() == Some(HEADER_ONLY_SOURCE),
    }))
}

/// A transaction proven by a Parano1d payment receipt: its pages hold every
/// field of the transaction, input amounts and creation ids included, and
/// the receipt's Merkle proof binds them to the block's `tx_root`. Only the
/// creation ids of its outputs are unknown - the block assigns them when it
/// is applied, counting over all its outputs - and stay NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptTransaction {
    /// Logical position in the block (`tx_index`), 0 being the coinbase.
    pub position: u32,
    /// The block's logical transactions, coinbase and development payout
    /// included (`tx_count`).
    pub tx_count: u32,
    pub txid: String,
    pub page_count: u32,
    pub fee_micronoid: u64,
    pub epoch_anchor: String,
    pub input_owner: String,
    pub input_sum_micronoid: String,
    pub output_sum_micronoid: String,
    pub contract_flags: u8,
    pub page_hashes: Vec<String>,
    pub inputs: Vec<ReceiptInput>,
    pub outputs: Vec<ReceiptOutput>,
    /// The receipt as the wallet stores it, kept so it can be verified again.
    pub receipt_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptInput {
    pub page: u32,
    pub lane: u32,
    pub slot_index: u64,
    pub amount_micronoid: u64,
    pub creation_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptOutput {
    pub page: u32,
    pub lane: u32,
    pub slot_index: u64,
    pub amount_micronoid: u64,
    pub owner: String,
}

/// Stores `tx` in the header-only block `block_id`. Returns `false` and
/// writes nothing if that transaction is on record there already. Refuses
/// (error) anything but a header-only block, a position another
/// transaction holds, and a transaction count that disagrees with an
/// earlier receipt of the same block. The caller has verified the receipt
/// against the block; this only keeps the rows consistent.
pub fn insert_receipt_transaction(conn: &Connection, block_id: i64, tx: &ReceiptTransaction, now: &str) -> Result<bool> {
    if !is_header_only(conn, block_id)? {
        bail!("block row {block_id} is not a header-only block; receipts only complete blocks below the archive");
    }
    if tx.position == 0 || tx.position >= tx.tx_count {
        bail!("position {} does not fit a block of {} transactions", tx.position, tx.tx_count);
    }
    let known: Option<i64> = conn.query_row("SELECT tx_count_total FROM blocks WHERE id = ?1", params![block_id], |r| r.get(0))?;
    if let Some(n) = known.filter(|n| *n != tx.tx_count as i64) {
        bail!("the block holds {n} transactions per an earlier receipt, this one says {}", tx.tx_count);
    }
    let existing: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT txid, source FROM transactions WHERE block_id = ?1 AND position = ?2",
            params![block_id, tx.position as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((txid, Some(source))) if txid == tx.txid && source == RECEIPT_SOURCE => return Ok(false),
        Some((txid, _)) => bail!("position {} holds {txid} already", tx.position),
        None => {}
    }
    conn.execute(
        "INSERT INTO transactions (block_id, position, txid, page_count, fee_micronoid, coinbase, development_payout,
            epoch_anchor, input_owner, input_sum_micronoid, output_sum_micronoid, contract_flags, source)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            block_id,
            tx.position as i64,
            tx.txid,
            tx.page_count as i64,
            tx.fee_micronoid as i64,
            tx.epoch_anchor,
            tx.input_owner,
            tx.input_sum_micronoid,
            tx.output_sum_micronoid,
            tx.contract_flags as i64,
            RECEIPT_SOURCE,
        ],
    )?;
    let tx_id = conn.last_insert_rowid();
    for (idx, hash) in tx.page_hashes.iter().enumerate() {
        conn.execute("INSERT INTO tx_page_hashes (tx_id, idx, page_hash) VALUES (?1, ?2, ?3)", params![tx_id, idx as i64, hash])?;
    }
    for (idx, i) in tx.inputs.iter().enumerate() {
        conn.execute(
            "INSERT INTO tx_inputs (tx_id, idx, page, lane, slot_index, amount_micronoid, creation_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![tx_id, idx as i64, i.page as i64, i.lane as i64, i.slot_index as i64, i.amount_micronoid as i64, i.creation_id.to_string()],
        )?;
    }
    for (idx, o) in tx.outputs.iter().enumerate() {
        conn.execute(
            "INSERT INTO tx_outputs (tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![tx_id, idx as i64, o.page as i64, o.lane as i64, o.slot_index as i64, o.amount_micronoid as i64, o.owner],
        )?;
    }
    conn.execute("INSERT INTO tx_receipts (tx_id, receipt_hex, imported_at) VALUES (?1, ?2, ?3)", params![tx_id, tx.receipt_hex, now])?;
    conn.execute("UPDATE blocks SET tx_count_total = ?2 WHERE id = ?1", params![block_id, tx.tx_count as i64])?;
    Ok(true)
}

/// Transactions imported from receipts.
pub fn receipt_transaction_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM transactions WHERE source = 'receipt'", [], |r| r.get(0))?)
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
/// should backfill instead of inserting a duplicate. Header-only rows are
/// never such a row: they are not part of the archive and are not turned
/// into it through the gap paths.
pub fn uncaptured_block_id(conn: &Connection, height: u64, hash: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            &format!("SELECT id FROM blocks WHERE height = ?1 AND hash = ?2 AND body_captured = 0 AND {ARCHIVED}"),
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

/// Id of the block row `(height, hash)`, canonical or not.
pub fn block_id(conn: &Connection, height: u64, hash: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM blocks WHERE height = ?1 AND hash = ?2", params![height as i64, hash], |r| r.get(0))
        .optional()?)
}

pub fn has_raw_block(conn: &Connection, block_id: i64) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM raw_blocks WHERE block_id = ?1)", params![block_id], |r| r.get(0))?)
}

/// Keeps a block's raw bytes (already checked and compressed by the
/// caller). A second copy of the same block is ignored.
pub fn insert_raw_block(conn: &Connection, block_id: i64, codec: &str, raw_len: usize, data: &[u8], source: &str, now: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO raw_blocks (block_id, codec, raw_len, data, source, stored_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![block_id, codec, raw_len as i64, data, source, now],
    )?;
    Ok(())
}

/// `(codec, raw length, compressed bytes)` kept for block `(height, hash)`.
pub fn raw_block(conn: &Connection, height: u64, hash: &str) -> Result<Option<(String, usize, Vec<u8>)>> {
    Ok(conn
        .query_row(
            "SELECT r.codec, r.raw_len, r.data FROM raw_blocks r JOIN blocks b ON b.id = r.block_id WHERE b.height = ?1 AND b.hash = ?2",
            params![height as i64, hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize, r.get::<_, Vec<u8>>(2)?)),
        )
        .optional()?)
}

/// Canonical blocks with a body on record but no raw bytes, between
/// `from` and `to` (inclusive), lowest first, at most `limit`.
pub fn blocks_missing_raw(conn: &Connection, from: u64, to: u64, limit: usize) -> Result<Vec<(u64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT b.height, b.hash FROM blocks b
         WHERE b.height BETWEEN ?1 AND ?2 AND b.body_captured = 1 AND b.{ARCHIVED}
           AND NOT EXISTS (SELECT 1 FROM raw_blocks r WHERE r.block_id = b.id)
           AND {canonical}
         ORDER BY b.height LIMIT ?3",
        canonical = crate::queries::canonical_block_filter_on("b")
    ))?;
    let clamp = |h: u64| h.min(i64::MAX as u64) as i64;
    let rows = stmt.query_map(params![clamp(from), clamp(to), clamp(limit as u64)], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What the raw block archive holds: blocks, their size before and after
/// compression, and the lowest height with bytes (`None` while empty).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RawCoverage {
    pub blocks: u64,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    pub from_height: Option<u64>,
}

pub fn raw_coverage(conn: &Connection) -> Result<RawCoverage> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(r.raw_len), 0), COALESCE(SUM(LENGTH(r.data)), 0), MIN(b.height)
         FROM raw_blocks r JOIN blocks b ON b.id = r.block_id",
        [],
        |r| {
            Ok(RawCoverage {
                blocks: r.get::<_, i64>(0)? as u64,
                raw_bytes: r.get::<_, i64>(1)? as u64,
                stored_bytes: r.get::<_, i64>(2)? as u64,
                from_height: r.get::<_, Option<i64>>(3)?.map(|h| h as u64),
            })
        },
    )?)
}

pub fn mark_body_recovered(conn: &Connection, block_id: i64, body_source: &str) -> Result<()> {
    conn.execute(
        "UPDATE blocks SET body_captured = 1, body_source = ?2 WHERE id = ?1",
        params![block_id, body_source],
    )?;
    Ok(())
}

/// Delete transaction-level detail (and the raw bytes) for blocks older
/// than `cutoff_unix`, keeping the block header row itself. Returns the number of blocks
/// pruned. Archive blocks only: header-only blocks, and the transactions
/// imported from receipts into them, are never pruned.
pub fn prune_older_than(conn: &Connection, cutoff_unix: i64) -> Result<usize> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM blocks WHERE timestamp < ?1 AND body_captured = 1 AND {ARCHIVED}"
    ))?;
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
        tx.execute("DELETE FROM raw_blocks WHERE block_id = ?1", params![block_id])?;
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
/// works through (one node call each). Miners of header-only blocks below
/// the archive, and the parties of transactions imported from receipts,
/// do not count: they were not recorded as the chain went by, and the
/// sweep covers every address holding anything anyway.
pub fn known_addresses(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT input_owner FROM transactions WHERE input_owner IS NOT NULL AND {RECORDED}
         UNION
         SELECT owner FROM tx_outputs WHERE tx_id NOT IN (SELECT id FROM transactions WHERE source = '{RECEIPT_SOURCE}')
         UNION
         SELECT miner FROM blocks WHERE {ARCHIVED}"
    ))?;
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

// The live-state queries below all reach `blocks` through a transaction
// (tx_outputs/tx_inputs -> transactions -> blocks). Header-only rows have no
// transactions of their own; the only ones they can hold are imported from
// receipts, and every query here leaves those out (`RECORDED` on `t`/`t2`):
// the picture of the live state is built from recorded blocks and the
// node's state alone. `header_only_rows_leave_the_archive_alone` and
// `receipt_transactions_leave_the_archive_alone` check it.
const CANONICAL_B: &str = "(SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'";

/// "No input in a canonical block at or below height ?1 spends the coin
/// with creation id `{cid}`."
fn not_spent_by_recorded_input(cid: &str) -> String {
    format!(
        "NOT EXISTS (
           SELECT 1 FROM tx_inputs i
           JOIN transactions t2 ON t2.id = i.tx_id
           JOIN blocks b2 ON b2.id = t2.block_id
           WHERE i.creation_id = {cid} AND b2.height <= ?1 AND t2.{RECORDED}
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
           AND o.spent_in_gap = 0 AND t.{RECORDED}
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
         WHERE b.height <= ?1 AND o.spent_in_gap = 0 AND t.{RECORDED} AND {CANONICAL_B} AND {}
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
         WHERE o.slot_index >= ?1 AND o.slot_index < ?2 AND t.{RECORDED} AND {CANONICAL_B}"
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
         WHERE b.height <= ?1 AND o.spent_in_gap = 0 AND t.{RECORDED} AND {CANONICAL_B} AND {}",
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
    // Only a recorded spend in a canonical block counts; orphaned blocks
    // keep their transactions on record and may well spend the same output.
    Ok(conn.execute(
        &format!(
            "UPDATE tx_outputs SET spent_in_gap = 0, spent_in_gap_at = NULL
             WHERE spent_in_gap = 1
               AND EXISTS (
                 SELECT 1 FROM tx_inputs i
                 JOIN transactions t ON t.id = i.tx_id
                 JOIN blocks b ON b.id = t.block_id
                 WHERE i.creation_id = tx_outputs.creation_id AND t.{RECORDED}
                   AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'
               )"
        ),
        [],
    )?)
}

/// Closes gap entries whose canonical block does have a body after all
/// (recorded through a path that didn't touch `ingest_gaps`, such as a
/// re-ingest after a reorg). Returns how many were closed.
pub fn resolve_gaps_with_bodies(conn: &Connection, now: &str) -> Result<usize> {
    Ok(conn.execute(
        &format!(
            "UPDATE ingest_gaps SET resolved_at = ?1, resolution = 'body on record'
             WHERE resolved_at IS NULL
               AND EXISTS (
                 SELECT 1 FROM blocks b
                 WHERE b.height = ingest_gaps.height AND b.body_captured = 1 AND {}
                   AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'
               )",
            archived_filter_on("b")
        ),
        params![now],
    )?)
}

/// Takes back `mark_spent_in_gap` for recorded outputs in `slots` that a
/// fresh read found live after all (an earlier sweep could not read their
/// slot). Returns how many were taken back.
pub fn unmark_live_outputs(conn: &Connection, slots: std::ops::Range<u64>, live: &std::collections::HashSet<&str>) -> Result<usize> {
    // Only recorded outputs are ever flagged (see `unspent_recorded_outputs`).
    let mut stmt = conn.prepare(&format!(
        "SELECT rowid, creation_id FROM tx_outputs
         WHERE spent_in_gap = 1 AND slot_index >= ?1 AND slot_index < ?2
           AND tx_id NOT IN (SELECT id FROM transactions WHERE source = '{RECEIPT_SOURCE}')"
    ))?;
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
        block_at(c, height, hash, status, 0, inputs, outputs);
    }

    /// `block` with a timestamp.
    fn block_at(c: &Connection, height: i64, hash: &str, status: &str, timestamp: i64, inputs: &[&str], outputs: &[(i64, &str, i64, &str)]) {
        c.execute(
            "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex, difficulty_target, body_captured, first_seen_at)
             VALUES (?1, ?2, '', '', '', ?3, 'o1miner', '', '', 1, '')",
            params![height, hash, timestamp],
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

    /// A header-only row as the backfill writes it.
    fn header(c: &Connection, height: u64, miner: &str, timestamp: u64) -> Result<bool> {
        let b = HeaderOnlyBlock {
            height,
            hash: format!("h{height}"),
            prev_hash: format!("h{}", height.wrapping_sub(1)),
            state_root: "s".into(),
            tx_root: "t".into(),
            timestamp,
            miner: miner.into(),
            nonce_hex: "n".into(),
            difficulty_target: "d".into(),
            log_slots: 24,
        };
        insert_header_only_block(c, &b, "now")
    }

    /// Everything that describes the recorded archive: the API's figures,
    /// the sweep's picture of the live state, the gap bookkeeping, the
    /// recorded balances and the payment service's coverage query.
    fn archive_view(c: &Connection) -> serde_json::Value {
        use crate::queries;
        let stats = queries::chain_stats(c).unwrap();
        let coverage = |sql: &str| c.query_row(sql, [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap();
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let mut balances: Vec<(String, (u128, u64))> = live_balances(c).unwrap().into_iter().collect();
        balances.sort();
        let mut segments: Vec<(u64, u64)> = live_count_per_segment(c, 100, 65_536).unwrap().into_iter().collect();
        segments.sort();
        let mut unspent = unspent_recorded_outputs(c, 100, 0..65_536).unwrap();
        unspent.sort();
        let addresses: Vec<serde_json::Value> = ["o1x", "o1y", "o1z", "o1w", "o1new"]
            .iter()
            .map(|a| serde_json::to_value(queries::address_balance(c, a).unwrap()).unwrap())
            .collect();
        serde_json::json!({
            "indexed_blocks": stats.indexed_blocks,
            "indexed_transactions": stats.indexed_transactions,
            "gaps": stats.gaps,
            "gaps_resolved": stats.gaps_resolved,
            "oldest_retained_timestamp": stats.oldest_retained_timestamp,
            "orphaned_blocks": stats.orphaned_blocks,
            "live_utxos": stats.live_utxos,
            "transactions_24h": stats.transactions_24h,
            "burned_fees_24h": stats.burned_fees_24h_micronoid,
            "archive_from_height": stats.archive_from_height,
            "coverage": coverage(&format!("SELECT COALESCE(MIN(height), 0), COALESCE(MIN(timestamp), 0) FROM blocks WHERE body_captured = 1 AND {ARCHIVED}")),
            "coverage_as_before": coverage("SELECT COALESCE(MIN(height), 0), COALESCE(MIN(timestamp), 0) FROM blocks WHERE body_captured = 1"),
            "known_addresses": sorted(known_addresses(c).unwrap()),
            "live_balances": format!("{balances:?}"),
            "segments": segments,
            "unspent": unspent,
            "recorded_ids": sorted(recorded_creation_ids(c, 0..65_536).unwrap().into_iter().collect()),
            "open_gaps": open_gap_heights(c, 0).unwrap(),
            "addresses": addresses,
            "flagged": c.query_row("SELECT COUNT(*) FROM tx_outputs WHERE spent_in_gap = 1", [], |r| r.get::<_, i64>(0)).unwrap(),
            "burn_by_block": queries::burn_by_block(c).unwrap(),
            "state_activity": serde_json::to_value(queries::state_activity(c, 0).unwrap()).unwrap(),
        })
    }

    /// Two connections that each read, then write, in a loop - the ingest
    /// loop and the header backfill do exactly that. Neither may ever see
    /// "database is locked": they have to wait for each other.
    #[test]
    fn writers_on_two_connections_wait_for_each_other() {
        let d = db("writers");
        let path = d.path.to_str().unwrap().to_string();
        let writer = |name: &'static str| {
            let path = path.clone();
            std::thread::spawn(move || -> Result<()> {
                let conn = open(&path)?;
                for i in 0..300 {
                    let tx = write_tx(&conn)?;
                    let n: i64 = tx.query_row("SELECT COUNT(*) FROM indexer_state", [], |r| r.get(0))?;
                    std::thread::sleep(std::time::Duration::from_micros(200));
                    set_state(&tx, &format!("{name}-{i}"), &n.to_string())?;
                    tx.commit()?;
                }
                Ok(())
            })
        };
        let (a, b) = (writer("a"), writer("b"));
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
        let n: i64 = d.conn.query_row("SELECT COUNT(*) FROM indexer_state", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 600);
    }

    fn now_unix() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    /// The archive: #10..#13, a reorg at #11, a gap at #12, a resolved one
    /// at #11, and two live UTXOs found by the sweep.
    fn archive(c: &Connection, now: i64) {
        block_at(c, 10, "a10", "canonical", now - 500, &[], &[(5, "o1x", 100, "1"), (70_000, "o1y", 50, "2")]);
        block_at(c, 11, "a11", "canonical", now - 400, &["1"], &[(6, "o1y", 90, "3")]);
        block_at(c, 11, "b11", "orphaned", now - 400, &["2"], &[]);
        c.execute(
            "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex, difficulty_target, body_captured, first_seen_at)
             VALUES (12, 'a12', 'a11', '', '', ?1, 'o1miner', '', '', 0, '')",
            params![now - 300],
        )
        .unwrap();
        c.execute("INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'canonical', '')", params![c.last_insert_rowid()]).unwrap();
        record_gap(c, 12, Some("a12"), "t", "no body").unwrap();
        record_gap(c, 11, Some("a11"), "t", "no body").unwrap();
        resolve_gap(c, 11, "t", "recovered via getblock").unwrap();
        block_at(c, 13, "a13", "canonical", now - 200, &["8"], &[(9, "o1z", 39, "4")]);
        c.execute("UPDATE blocks SET prev_hash = 'h9' WHERE hash = 'a10'", []).unwrap();
        c.execute("UPDATE blocks SET miner = 'o1pool' WHERE hash = 'a13'", []).unwrap();
        c.execute("UPDATE blocks SET reward_micronoid = 45000100, total_fees_micronoid = '300' WHERE body_captured = 1", []).unwrap();
        replace_state_slots(c, 0, &[slot(7, "9", "o1z", 40), slot(8, "8", "o1w", 10)], 9, "t").unwrap();
    }

    /// The header backfill below the archive: genesis and #1..#9.
    fn headers_below(c: &Connection, now: i64) {
        for h in 0..10u64 {
            let miner = match h {
                0 => "o1genesis",
                h if h % 2 == 1 => "o1old",
                _ => "o1x",
            };
            assert!(header(c, h, miner, (now - 100_000) as u64 + h).unwrap());
        }
    }

    #[test]
    fn header_only_rows_leave_the_archive_alone() {
        use crate::queries;
        let d = db("headers");
        let c = &d.conn;
        let now = now_unix();
        archive(c, now);
        let before = archive_view(c);
        let txs_before = serde_json::to_value(queries::txs_by_address(c, "o1y", 1, 50).unwrap()).unwrap();

        headers_below(c, now);
        assert_eq!(archive_view(c), before, "the archive's figures must not move");
        assert_eq!(serde_json::to_value(queries::txs_by_address(c, "o1y", 1, 50).unwrap()).unwrap(), txs_before);

        // idempotent, and never on top of a block on record
        assert!(!header(c, 5, "o1old", 0).unwrap(), "same block again: nothing written");
        assert!(header(c, 10, "o1old", 0).is_err(), "#10 holds an archive block");
        assert!(header(c, 12, "o1old", 0).is_err(), "#12 holds a gap");

        let stats = queries::chain_stats(c).unwrap();
        assert_eq!(stats.indexed_blocks, 4);
        assert_eq!(stats.header_only_blocks, 10);
        assert_eq!(stats.headers_from_height, Some(0));
        assert_eq!(stats.archive_from_height, Some(10));
        assert_eq!(archive_first_height(c).unwrap(), Some(10));
        assert_eq!(lowest_recorded_height(c).unwrap(), Some(0));
        assert_eq!(canonical_link_at(c, 10).unwrap(), Some(("a10".to_string(), "h9".to_string())));

        // the gap paths never pick up a header-only row
        let (id5, _) = canonical_hash_at(c, 5).unwrap().unwrap();
        assert!(is_header_only(c, id5).unwrap());
        assert_eq!(uncaptured_block_id(c, 5, "h5").unwrap(), None);
        assert!(uncaptured_block_id(c, 12, "a12").unwrap().is_some());

        // block pages and paging run on below the archive
        let page = queries::recent_blocks(c, 4, Some(12)).unwrap();
        assert_eq!(page.iter().map(|b| (b.height, b.archived)).collect::<Vec<_>>(), vec![(11, true), (10, true), (9, false), (8, false)]);
        let newest = queries::recent_blocks(c, 2, None).unwrap();
        assert_eq!(newest.iter().map(|b| b.height).collect::<Vec<_>>(), vec![13, 12]);
        let b5 = queries::block_by_height(c, 5).unwrap().unwrap();
        assert!(!b5.archived && b5.canonical && b5.transactions.is_empty());
        assert_eq!((b5.reward_micronoid, b5.total_fees_micronoid.as_deref(), b5.archive_from_height), (None, None, Some(10)));
        assert_eq!(b5.miner_subsidy_micronoid, 45_000_000);
        assert_eq!(queries::block_by_height(c, 0).unwrap().unwrap().miner_subsidy_micronoid, 0, "genesis mints nothing");
        let b10 = queries::block_by_hash(c, "a10").unwrap().unwrap();
        assert!(b10.archived && b10.archive_from_height.is_none());

        // mining history over the whole chain; genesis is nobody's block
        let mined = queries::blocks_mined(c, "o1old").unwrap();
        assert_eq!((mined.count, mined.first_height, mined.last_height, mined.counted_from_height), (5, Some(1), Some(9), Some(1)));
        assert_eq!(queries::blocks_mined(c, "o1genesis").unwrap().count, 0);
        assert_eq!(queries::blocks_mined(c, "o1pool").unwrap().count, 1);
        let all = queries::miners(c, None, 100).unwrap();
        assert!(all.complete);
        assert_eq!((all.blocks, all.from_height, all.to_height, all.miner_count), (13, Some(1), Some(13), 4));
        assert_eq!((all.miners[0].address.as_str(), all.miners[0].blocks), ("o1old", 5));
        assert!((all.miners.iter().map(|m| m.share).sum::<f64>() - 1.0).abs() < 1e-9);
        let recent = queries::miners(c, Some(now - 1_000), 1).unwrap();
        assert_eq!((recent.blocks, recent.miner_count, recent.miners.len(), recent.complete), (4, 2, 1, true));
        // the mining figures skip the status lookup for header-only rows; the
        // full canonical filter must agree
        let full: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM blocks b WHERE b.height >= 1
                   AND (SELECT s.status FROM block_status_log s WHERE s.block_id = b.id ORDER BY s.id DESC LIMIT 1) = 'canonical'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(full, all.blocks);
        assert_eq!(queries::chain_stats(c).unwrap().orphaned_blocks, 1);
        assert_eq!(queries::orphaned_blocks(c, 10).unwrap().iter().map(|o| o.hash.as_str()).collect::<Vec<_>>(), vec!["b11"]);

        // the archive's own queries use the partial indexes
        let plan = |sql: &str| -> String {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3)).unwrap().map(|r| r.unwrap()).collect::<Vec<_>>().join("; ")
        };
        assert!(plan(&format!("SELECT MIN(height) FROM blocks WHERE {ARCHIVED}")).contains("idx_blocks_archive_height"));
        assert!(plan(&format!("SELECT MIN(timestamp) FROM blocks WHERE body_captured = 1 AND {ARCHIVED}")).contains("idx_blocks_archive_timestamp"));
        assert!(plan("SELECT COUNT(*) FROM blocks WHERE body_source = 'header'").contains("idx_blocks_header_only"));
        let count_plan = plan(&format!(
            "SELECT COUNT(*) FROM blocks WHERE blocks.{ARCHIVED}
               AND (SELECT s.status FROM block_status_log s WHERE s.block_id = blocks.id ORDER BY s.id DESC LIMIT 1) = 'canonical'"
        ));
        assert!(count_plan.contains("idx_blocks_archive_"), "{count_plan}");
        assert!(plan("SELECT COUNT(*), MIN(height) FROM blocks WHERE miner = 'o1old' AND height >= 1").contains("COVERING INDEX idx_blocks_miner"));

        // retention prunes archive bodies only
        assert_eq!(prune_older_than(c, now + 1).unwrap(), 4, "#10, #11 (both versions), #13");
        let headers: i64 = c.query_row("SELECT COUNT(*) FROM blocks WHERE body_source = 'header' AND body_captured = 0", [], |r| r.get(0)).unwrap();
        assert_eq!(headers, 10);
    }

    /// A transaction as a receipt proves it: inputs (slot, amount, creation
    /// id), outputs (slot, amount, owner).
    fn receipt_tx(position: u32, tx_count: u32, txid: &str, owner: &str, inputs: &[(u64, u64, u64)], outputs: &[(u64, u64, &str)], fee: u64) -> ReceiptTransaction {
        let out_sum: u64 = outputs.iter().map(|o| o.1).sum();
        ReceiptTransaction {
            position,
            tx_count,
            txid: txid.into(),
            page_count: 1,
            fee_micronoid: fee,
            epoch_anchor: "ea".into(),
            input_owner: owner.into(),
            input_sum_micronoid: (out_sum + fee).to_string(),
            output_sum_micronoid: out_sum.to_string(),
            contract_flags: 0,
            page_hashes: vec![format!("ph{txid}")],
            inputs: inputs.iter().enumerate().map(|(lane, &(slot_index, amount_micronoid, creation_id))| ReceiptInput { page: 0, lane: lane as u32, slot_index, amount_micronoid, creation_id }).collect(),
            outputs: outputs.iter().enumerate().map(|(lane, &(slot_index, amount_micronoid, owner))| ReceiptOutput { page: 0, lane: lane as u32, slot_index, amount_micronoid, owner: owner.into() }).collect(),
            receipt_hex: format!("00{txid}"),
        }
    }

    /// Transactions imported from receipts show on block, transaction and
    /// address pages, but the recorded archive - balances, the sweep's
    /// picture of the live state, spent-in-gap flags, the known-address set,
    /// counts - does not move. The receipts below touch archive addresses and
    /// spend creation ids of live archive outputs and of a UTXO the sweep
    /// found (impossible on a real chain: they are older than the archive),
    /// so every missing filter shows.
    #[test]
    fn receipt_transactions_leave_the_archive_alone() {
        use crate::queries;
        let d = db("receipts");
        let c = &d.conn;
        let now = now_unix();
        archive(c, now);
        headers_below(c, now);
        // a young permanode: the blocks right below its archive are recent
        c.execute("UPDATE blocks SET timestamp = ?1 WHERE height = 7", params![now - 600]).unwrap();
        // B (creation id 2) is gone from the node's state per the sweep
        let b_row: i64 = c.query_row("SELECT rowid FROM tx_outputs WHERE creation_id = '2'", [], |r| r.get(0)).unwrap();
        mark_spent_in_gap(c, &[b_row], "t").unwrap();
        let before = archive_view(c);
        let txs_before = queries::txs_by_address(c, "o1y", 1, 50).unwrap().1;

        let block_id = |h: u64| canonical_block_at(c, h).unwrap().unwrap().id;
        // spends C (3, live) and D (9, a sweep slot); pays o1new and o1y
        let r1 = receipt_tx(1, 3, "r1", "o1x", &[(6, 90, 3), (7, 40, 9)], &[(20, 70, "o1new"), (70_001, 59, "o1y")], 1);
        // spends B (2, flagged as spent in a gap)
        let r2 = receipt_tx(2, 3, "r2", "o1y", &[(70_000, 50, 2)], &[(21, 49, "o1x")], 1);
        let r3 = receipt_tx(1, 2, "r3", "o1new", &[(20, 70, 77)], &[(22, 69, "o1w")], 1);
        assert!(insert_receipt_transaction(c, block_id(5), &r1, "now").unwrap());
        assert!(insert_receipt_transaction(c, block_id(5), &r2, "now").unwrap());
        assert!(insert_receipt_transaction(c, block_id(7), &r3, "now").unwrap());

        assert_eq!(archive_view(c), before, "the archive's figures must not move");
        assert_eq!(clear_spent_in_gap_with_recorded_spend(c).unwrap(), 0, "a receipt is no recorded spend");
        assert_eq!(unmark_live_outputs(c, 0..65_536 * 2, &HashSet::from(["2"])).unwrap(), 1, "B is live after all");
        mark_spent_in_gap(c, &[b_row], "t").unwrap();
        assert_eq!(archive_view(c), before);

        // idempotent; never on top of another transaction, a different
        // count, the coinbase position, or a block of the archive
        assert!(!insert_receipt_transaction(c, block_id(5), &r1, "now").unwrap(), "same receipt again: nothing written");
        assert!(insert_receipt_transaction(c, block_id(5), &receipt_tx(1, 3, "rx", "o1x", &[(1, 2, 3)], &[(4, 1, "o1x")], 1), "now").is_err());
        assert!(insert_receipt_transaction(c, block_id(5), &receipt_tx(3, 4, "ry", "o1x", &[(1, 2, 3)], &[(4, 1, "o1x")], 1), "now").is_err());
        assert!(insert_receipt_transaction(c, block_id(9), &receipt_tx(0, 2, "rz", "o1x", &[(1, 2, 3)], &[(4, 1, "o1x")], 1), "now").is_err());
        assert!(insert_receipt_transaction(c, block_id(10), &receipt_tx(1, 3, "ra", "o1x", &[(1, 2, 3)], &[(4, 1, "o1x")], 1), "now").is_err());
        assert!(insert_receipt_transaction(c, block_id(12), &receipt_tx(1, 3, "rg", "o1x", &[(1, 2, 3)], &[(4, 1, "o1x")], 1), "now").is_err());
        assert_eq!(archive_view(c), before);

        // what the pages show
        let stats = queries::chain_stats(c).unwrap();
        assert_eq!((stats.receipt_transactions, stats.indexed_transactions), (3, 4));
        let b5 = queries::block_by_height(c, 5).unwrap().unwrap();
        assert!(!b5.archived && b5.reward_micronoid.is_none());
        assert_eq!(b5.tx_count_total, Some(3));
        assert_eq!(b5.transactions.iter().map(|t| (t.position, t.txid.as_str(), t.source)).collect::<Vec<_>>(), vec![(1, "r1", Some("receipt")), (2, "r2", Some("receipt"))]);
        assert_eq!(queries::block_by_height(c, 6).unwrap().unwrap().tx_count_total, None);
        assert_eq!(queries::block_by_height(c, 10).unwrap().unwrap().tx_count_total, None);
        let listed = queries::recent_blocks(c, 10, Some(8)).unwrap();
        let b5_row = listed.iter().find(|b| b.height == 5).unwrap();
        assert_eq!((b5_row.tx_count, b5_row.tx_count_total, b5_row.archived), (2, Some(3), false));
        let t = queries::tx_by_txid(c, "r1").unwrap().unwrap();
        assert_eq!((t.source, t.block.height, t.position, t.input_sum_micronoid.as_str()), (Some("receipt"), 5, 1, "130"));
        assert_eq!(t.inputs.iter().map(|i| (i.amount_micronoid, i.creation_id.as_str())).collect::<Vec<_>>(), vec![(90, "3"), (40, "9")]);
        assert!(t.outputs.iter().all(|o| o.creation_id.is_none()));
        assert_eq!(t.page_hashes, vec!["phr1"]);
        assert_eq!(queries::tx_by_txid(c, "txa11").unwrap().unwrap().source, None, "recorded transactions carry no source");
        let (new_txs, total) = queries::txs_by_address(c, "o1new", 1, 50).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            new_txs.iter().map(|t| (t.txid.as_str(), t.source, t.address_delta_micronoid.as_deref())).collect::<Vec<_>>(),
            vec![("r3", Some("receipt"), Some("-70")), ("r1", Some("receipt"), Some("70"))]
        );
        let (y_txs, y_total) = queries::txs_by_address(c, "o1y", 1, 50).unwrap();
        assert_eq!(y_total, txs_before + 2, "the address history shows them");
        assert_eq!(y_txs.iter().filter(|t| t.source.is_some()).count(), 2);
        assert_eq!(queries::receipt_txs_by_address(c, "o1y").unwrap(), 2);
        assert_eq!(queries::receipt_txs_by_address(c, "o1new").unwrap(), 2);
        assert_eq!(queries::receipt_txs_by_address(c, "o1z").unwrap(), 0);

        // retention never touches them
        assert_eq!(prune_older_than(c, now + 1).unwrap(), 4);
        assert_eq!(receipt_transaction_count(c).unwrap(), 3);
        let kept: i64 = c.query_row("SELECT COUNT(*) FROM tx_receipts", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 3);
    }

    /// A database from before receipt imports declares
    /// `tx_outputs.creation_id NOT NULL`; opening it rebuilds the table once
    /// without that constraint and keeps every row, rowid and index.
    #[test]
    fn output_creation_ids_become_nullable_once() {
        let d = db("rebuild");
        let c = &d.conn;
        block(c, 10, "a10", "canonical", &[], &[(5, "o1x", 100, "1"), (70_000, "o1y", 50, "2")]);
        block(c, 11, "a11", "canonical", &["1"], &[(6, "o1y", 90, "3")]);
        let rows = |c: &Connection| -> Vec<(i64, i64, i64, String, String, i64)> {
            let mut stmt = c.prepare("SELECT rowid, tx_id, idx, owner, creation_id, spent_in_gap FROM tx_outputs ORDER BY rowid").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        // the table as it was before: creation_id NOT NULL, rowids with a hole
        c.execute("DELETE FROM tx_outputs WHERE creation_id = '2'", []).unwrap();
        c.execute("UPDATE tx_outputs SET spent_in_gap = 1, spent_in_gap_at = 't' WHERE creation_id = '1'", []).unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE old_outputs (
                 tx_id INTEGER NOT NULL REFERENCES transactions(id), idx INTEGER NOT NULL, page INTEGER NOT NULL,
                 lane INTEGER NOT NULL, slot_index INTEGER NOT NULL, amount_micronoid INTEGER NOT NULL, owner TEXT NOT NULL,
                 creation_id TEXT NOT NULL, spent_in_gap INTEGER NOT NULL DEFAULT 0, spent_in_gap_at TEXT, PRIMARY KEY(tx_id, idx));
             INSERT INTO old_outputs (rowid, tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id, spent_in_gap, spent_in_gap_at)
                 SELECT rowid, tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id, spent_in_gap, spent_in_gap_at FROM tx_outputs;
             DROP TABLE tx_outputs;
             ALTER TABLE old_outputs RENAME TO tx_outputs;
             CREATE INDEX idx_outputs_owner ON tx_outputs(owner);
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        assert!(column_is_not_null(c, "tx_outputs", "creation_id").unwrap());
        let before = rows(c);
        assert_eq!(before.len(), 2);

        let reopened = open(d.path.to_str().unwrap()).unwrap();
        assert!(!column_is_not_null(&reopened, "tx_outputs", "creation_id").unwrap());
        assert!(column_is_not_null(&reopened, "tx_outputs", "owner").unwrap(), "the other constraints stay");
        assert_eq!(rows(&reopened), before, "every row with its rowid");
        let indexes: Vec<String> = reopened
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'tx_outputs' AND sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(indexes, vec!["idx_outputs_creation", "idx_outputs_owner", "idx_outputs_slot"]);
        let fk: i64 = reopened.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(fk, 1, "foreign keys are enforced again");
        let problems: i64 = reopened.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
        assert_eq!(problems, 0);
        let ok: String = reopened.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        // a NULL creation id is accepted now, and a second open changes nothing
        reopened.execute("INSERT INTO tx_outputs (tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id) VALUES (1, 9, 0, 0, 1, 1, 'o1r', NULL)", []).unwrap();
        drop(reopened);
        let again = open(d.path.to_str().unwrap()).unwrap();
        assert_eq!(rows_with_null(&again), 3);
    }

    fn rows_with_null(c: &Connection) -> i64 {
        c.query_row("SELECT COUNT(*) FROM tx_outputs", [], |r| r.get(0)).unwrap()
    }
}
