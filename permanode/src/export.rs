//! CSV export of the recorded history for offline analysis.
//!
//! Five files that join on stable keys: `tx_id` links transactions to
//! their inputs and outputs. The protocol `txid` is deliberately NOT the
//! join key — a transaction that survived a reorg is recorded once per
//! block it was in, so joining on it double-counts. Amounts stay in
//! µNOID, timestamps are Unix seconds plus a UTC string.

use anyhow::{Context, Result};
use permanode_core::queries;
use rusqlite::Connection;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct ExportReport {
    pub blocks: usize,
    pub transactions: usize,
    pub inputs: usize,
    pub outputs: usize,
    pub addresses: usize,
}

/// Quotes a field for CSV only where needed, and never emits a raw
/// newline: every value here is hex, bech32m, a number or a timestamp.
fn field(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn write_query(conn: &Connection, path: &Path, header: &str, sql: &str) -> Result<usize> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    writeln!(out, "{header}")?;
    let mut stmt = conn.prepare(sql)?;
    let cols = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        let mut line = String::new();
        for i in 0..cols {
            if i > 0 {
                line.push(',');
            }
            let value: rusqlite::types::Value = row.get(i)?;
            let text = match value {
                rusqlite::types::Value::Null => String::new(),
                rusqlite::types::Value::Integer(v) => v.to_string(),
                rusqlite::types::Value::Real(v) => v.to_string(),
                rusqlite::types::Value::Text(v) => field(&v),
                rusqlite::types::Value::Blob(_) => String::new(),
            };
            line.push_str(&text);
        }
        writeln!(out, "{line}")?;
        n += 1;
    }
    out.flush()?;
    Ok(n)
}

pub fn export_csv(conn: &Connection, dir: &Path) -> Result<ExportReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let canonical = queries::canonical_block_filter();
    let tip = queries::indexed_tip(conn)?.unwrap_or(0);

    let blocks = write_query(
        conn,
        &dir.join("blocks.csv"),
        "height,hash,prev_hash,timestamp,time_utc,miner,coinbase_value,total_fees,log_slots,body_captured,body_source,canonical,tx_count",
        &format!(
            "SELECT height, hash, prev_hash, timestamp, datetime(timestamp,'unixepoch'), miner,
                    reward_micronoid, CAST(total_fees_micronoid AS INTEGER), log_slots, body_captured, body_source,
                    ({canonical}), (SELECT COUNT(*) FROM transactions t WHERE t.block_id = blocks.id)
             FROM blocks ORDER BY height"
        ),
    )?;

    let transactions = write_query(
        conn,
        &dir.join("transactions.csv"),
        "tx_id,txid,block_height,block_hash,timestamp,time_utc,fee,canonical,finalized,coinbase,development_payout,sender,input_sum,output_sum,tx_position,page_count",
        &format!(
            "SELECT t.id, t.txid, b.height, b.hash, b.timestamp, datetime(b.timestamp,'unixepoch'),
                    t.fee_micronoid, ({canonical_b}), CASE WHEN {tip} - b.height + 1 >= 18 THEN 1 ELSE 0 END,
                    t.coinbase, t.development_payout, t.input_owner,
                    CAST(t.input_sum_micronoid AS INTEGER), CAST(t.output_sum_micronoid AS INTEGER),
                    t.position, t.page_count
             FROM transactions t JOIN blocks b ON b.id = t.block_id
             ORDER BY b.height, t.position",
            canonical_b = queries::canonical_block_filter_on("b")
        ),
    )?;

    let inputs = write_query(
        conn,
        &dir.join("inputs.csv"),
        "tx_id,txid,input_index,address,amount,slot_index,creation_id,block_height,canonical",
        &format!(
            "SELECT t.id, t.txid, i.idx, t.input_owner, i.amount_micronoid, i.slot_index, i.creation_id,
                    b.height, ({canonical_b})
             FROM tx_inputs i JOIN transactions t ON t.id = i.tx_id JOIN blocks b ON b.id = t.block_id
             ORDER BY b.height, t.position, i.idx",
            canonical_b = queries::canonical_block_filter_on("b")
        ),
    )?;

    let outputs = write_query(
        conn,
        &dir.join("outputs.csv"),
        "tx_id,txid,output_index,address,amount,slot_index,creation_id,block_height,canonical,spent,spent_in_gap",
        &format!(
            "SELECT t.id, t.txid, o.idx, o.owner, o.amount_micronoid, o.slot_index, o.creation_id,
                    b.height, ({canonical_b}),
                    CASE WHEN EXISTS (
                      SELECT 1 FROM tx_inputs i2 JOIN transactions t2 ON t2.id = i2.tx_id JOIN blocks b2 ON b2.id = t2.block_id
                      WHERE i2.creation_id = o.creation_id AND ({canonical_b2})
                    ) THEN 1 ELSE 0 END,
                    o.spent_in_gap
             FROM tx_outputs o JOIN transactions t ON t.id = o.tx_id JOIN blocks b ON b.id = t.block_id
             ORDER BY b.height, t.position, o.idx",
            canonical_b = queries::canonical_block_filter_on("b"),
            canonical_b2 = queries::canonical_block_filter_on("b2")
        ),
    )?;

    let addresses = write_query(
        conn,
        &dir.join("addresses.csv"),
        "address,live_balance,live_utxo_count,fetched_at",
        "SELECT address, CAST(live_balance_micronoid AS INTEGER), live_utxo_count, fetched_at
         FROM address_balance_cache ORDER BY CAST(live_balance_micronoid AS INTEGER) DESC",
    )?;

    std::fs::write(dir.join("README.txt"), README)?;
    Ok(ExportReport { blocks, transactions, inputs, outputs, addresses })
}

const README: &str = "\
CSV export of a parano1d-permanode database.

Amounts are in microNOID (1 NOID = 1 000 000 microNOID). Times are UTC.
Coverage begins where this permanode started recording; blocks with
body_captured = 0 have no known transactions.

Join key: tx_id (the database's own key), NOT txid. A transaction that
survived a reorg is recorded once per block it was in, so joining on txid
double-counts its inputs and outputs. Filter canonical = 1 unless you are
studying reorgs.

outputs.csv: unspent = canonical = 1 AND spent = 0 AND spent_in_gap = 0.
spent_in_gap marks an output that is gone from the node's UTXO state
although no recorded transaction spends it - its spend sits in a block
whose body this permanode never got.

addresses.csv holds live balances read from the node's UTXO state by the
periodic sweep. They are authoritative and cover addresses that never
appear in the recorded history.
";
