//! `import-receipts`: completes the header-only blocks below the archive
//! with the transactions Parano1d payment receipts prove.
//!
//! A receipt (`noid_chain::consensus::receipt::ParanoidReceipt`) holds a
//! transaction's pages without their authorization, its logical position,
//! the block's transaction count and the Merkle path to the block's
//! `tx_root`. The pages carry every field of the transaction - the input
//! amounts and creation ids too, since the transaction id commits to them -
//! so a receipt that verifies proves the whole transaction. Only the
//! creation ids of its outputs are unknown: the block assigns them when it
//! is applied, counting over all of its outputs.
//!
//! Each receipt is decoded exactly (one receipt, nothing after it), checked
//! offline (Merkle proof, summary, one input owner), verified by the node
//! (`paranoid_verifyReceipt`: Merkle proof against the node's canonical
//! header, whose answer must match the receipt field for field) and
//! compared with the header-only block on record at its height (tx_root
//! and time). Only header-only blocks are completed; receipts for blocks of
//! the archive, for gaps and for heights without a header yet are skipped
//! and reported. Importing the same receipt again changes nothing.
//!
//! Imported transactions are stored with `source = 'receipt'` and are not
//! part of the recorded history (see `permanode_core::db::RECORDED`).

use crate::rpc::{AuthenticatedSummary, ReceiptVerdict, SummaryInput, SummaryOutput};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use noid_chain::consensus::receipt::{verify_merkle_inclusion, ParanoidReceipt};
use noid_tx::{validate_paged_spend, PagedSpendIntent, PAGED_SPEND_CONTRACT_BIT, PAGED_SPEND_TERMINAL_BIT};
use permanode_core::db::{self, ReceiptInput, ReceiptOutput, ReceiptTransaction};
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// First bytes of the Parano1d wallet's receipt journal (`wallet.receipts`).
pub const JOURNAL_MAGIC: &[u8; 8] = b"NOIDRCJ1";
/// Domain of the journal's per-frame BLAKE3 checksum, as the wallet writes it.
const JOURNAL_FRAME_DOMAIN: &[u8] = b"NOID/local-wallet-journal/frame/v1";
const FRAME_HEADER_BYTES: usize = 16;
const FRAME_DIGEST_BYTES: usize = 32;
/// Far above any real wallet file; guards against pointing the command at
/// something else by mistake.
const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// One receipt as read from an input file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptEntry {
    /// The file it came from, for the report.
    pub origin: String,
    /// The txid it is filed under (journal and JSON map), which the receipt
    /// has to prove.
    pub key: Option<String>,
    /// Lowercase hex, whitespace and a `0x` prefix removed.
    pub hex: String,
}

/// Reads receipts from `path`: a wallet receipt journal (`wallet.receipts`,
/// starts with `NOIDRCJ1`), a JSON object `{"<txid>": "<receipt hex>"}` (the
/// wallet's older format), or text with one receipt hex per line (blank
/// lines and lines starting with `#` are skipped).
pub fn read_receipt_file(path: &Path) -> Result<Vec<ReceiptEntry>> {
    let size = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?.len();
    if size > MAX_INPUT_BYTES {
        bail!("{} is {size} bytes - not a receipt file", path.display());
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let origin = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    parse_receipt_file(&origin, &bytes).with_context(|| format!("read {}", path.display()))
}

/// `read_receipt_file` on bytes already in memory.
pub fn parse_receipt_file(origin: &str, bytes: &[u8]) -> Result<Vec<ReceiptEntry>> {
    let entry = |key: Option<String>, hex: &str| ReceiptEntry { origin: origin.to_string(), key, hex: clean_hex(hex) };
    if bytes.starts_with(JOURNAL_MAGIC) {
        return Ok(parse_journal(bytes)?.into_iter().map(|(k, v)| entry(Some(k), &v)).collect());
    }
    let text = std::str::from_utf8(bytes).context("neither a wallet receipt journal nor text")?;
    if text.trim_start().starts_with('{') {
        let map: BTreeMap<String, String> = serde_json::from_str(text).context("not a JSON object of txid -> receipt hex")?;
        return Ok(map.into_iter().map(|(k, v)| entry(Some(k.to_ascii_lowercase()), &v)).collect());
    }
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| entry(None, l))
        .collect())
}

fn clean_hex(raw: &str) -> String {
    let joined: String = raw.split_whitespace().collect();
    joined.strip_prefix("0x").unwrap_or(&joined).to_ascii_lowercase()
}

#[derive(serde::Deserialize)]
struct JournalDelta {
    reset: bool,
    changes: BTreeMap<String, Option<String>>,
}

/// The receipts a wallet journal holds, read the way the wallet reads it:
/// frames of `u64 length, u64 !length, JSON delta, BLAKE3 checksum`; the
/// first frame is a snapshot (`reset`), later ones add or delete (`null`)
/// receipts. An incomplete last frame (a write cut short) is ignored like
/// the wallet ignores it; a complete frame that does not check out is an
/// error.
pub fn parse_journal(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    if !bytes.starts_with(JOURNAL_MAGIC) {
        bail!("not a wallet receipt journal");
    }
    let mut receipts = BTreeMap::new();
    let mut pos = JOURNAL_MAGIC.len();
    let mut frames = 0;
    while bytes.len() - pos >= FRAME_HEADER_BYTES {
        let header: [u8; FRAME_HEADER_BYTES] = bytes[pos..pos + FRAME_HEADER_BYTES].try_into().expect("16 bytes");
        let length = u64::from_le_bytes(header[..8].try_into().expect("8 bytes"));
        let inverted = u64::from_le_bytes(header[8..].try_into().expect("8 bytes"));
        if length != !inverted {
            bail!("corrupt frame length at byte {pos}");
        }
        let Some(frame_len) = usize::try_from(length).ok().and_then(|l| l.checked_add(FRAME_HEADER_BYTES + FRAME_DIGEST_BYTES)) else {
            bail!("frame length overflow at byte {pos}");
        };
        if frame_len > bytes.len() - pos {
            break; // incomplete final frame
        }
        let payload = &bytes[pos + FRAME_HEADER_BYTES..pos + frame_len - FRAME_DIGEST_BYTES];
        let digest = &bytes[pos + frame_len - FRAME_DIGEST_BYTES..pos + frame_len];
        let mut hasher = blake3::Hasher::new();
        hasher.update(JOURNAL_FRAME_DOMAIN);
        hasher.update(JOURNAL_MAGIC);
        hasher.update(&header);
        hasher.update(payload);
        if hasher.finalize().as_bytes() != digest {
            bail!("corrupt frame checksum at byte {pos}");
        }
        let delta: JournalDelta = serde_json::from_slice(payload).with_context(|| format!("frame at byte {pos}"))?;
        if frames == 0 && !delta.reset {
            bail!("the journal lacks its initial snapshot");
        }
        if delta.reset {
            receipts.clear();
        }
        let mut seen = HashSet::new();
        for (key, value) in delta.changes {
            let key = key.to_ascii_lowercase();
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("frame at byte {pos}: key {key} is not a txid");
            }
            if !seen.insert(key.clone()) {
                bail!("frame at byte {pos}: txid {key} twice");
            }
            match value {
                Some(hex) => {
                    receipts.insert(key, hex);
                }
                None if !delta.reset => {
                    receipts.remove(&key);
                }
                None => bail!("frame at byte {pos}: a snapshot with a deletion"),
            }
        }
        frames += 1;
        pos += frame_len;
    }
    if frames == 0 {
        bail!("the journal holds no complete frame");
    }
    if pos != bytes.len() {
        log::warn!("ignoring an incomplete last journal frame ({} byte(s)), as the wallet does", bytes.len() - pos);
    }
    Ok(receipts)
}

/// A receipt taken apart: the transaction it proves and where.
#[derive(Debug, Clone)]
pub struct DecodedReceipt {
    pub height: u64,
    /// Block time the receipt claims (`confirmed_unix`).
    pub timestamp: u64,
    /// The block's `tx_root` the Merkle path leads to, hex.
    pub tx_root: String,
    pub tx: ReceiptTransaction,
    /// What the node has to confirm, in its own terms.
    pub summary: AuthenticatedSummary,
}

/// Decodes one receipt and checks everything that can be checked offline.
/// `Err` explains why it cannot be imported.
pub fn decode_receipt(hex_str: &str) -> std::result::Result<DecodedReceipt, String> {
    let bytes = hex::decode(hex_str).map_err(|_| "not hex".to_string())?;
    let receipt = ParanoidReceipt::from_bytes(&bytes).map_err(|e| format!("not a Parano1d receipt ({e})"))?;
    let canonical = receipt.to_bytes();
    if canonical.len() < bytes.len() {
        return Err(format!(
            "{} byte(s) follow the receipt - one receipt per entry (the node would check only the first)",
            bytes.len() - canonical.len()
        ));
    }
    if canonical != bytes {
        return Err("not in the receipt's canonical encoding".into());
    }
    // Merkle path from the transaction id to the claimed root, and a
    // summary that matches the pages (noid_chain's own offline check).
    if !verify_merkle_inclusion(&receipt) {
        return Err("its Merkle proof does not verify (damaged or altered)".into());
    }
    let intent = PagedSpendIntent::from_bytes(&receipt.paged_spend).map_err(|e| format!("pages: {e:?}"))?;
    let facts = validate_paged_spend(&intent.pages).map_err(|e| format!("pages: {e:?}"))?;
    let txid = hex::encode(receipt.summary.logical_txid);
    if hex::encode(facts.logical_txid.0) != txid {
        return Err("the pages do not hash to the receipt's txid".into());
    }
    // The chain's model: every input of a transaction has one owner.
    if receipt.summary.inputs.is_empty() || receipt.summary.inputs.iter().any(|(_, owner)| *owner != facts.input_owner) {
        return Err("its inputs do not share one owner".into());
    }

    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut page_hashes = Vec::new();
    // Rows exactly as the indexer stores a block's transaction (see
    // decode::decode_retained_block), less the outputs' creation ids.
    for (page_index, page) in intent.pages.iter().enumerate() {
        page_hashes.push(hex::encode(page.page_hash().0));
        for (lane, i) in page.body.live_inputs() {
            inputs.push(ReceiptInput {
                page: page_index as u32,
                lane: u32::from(lane as u8),
                slot_index: u64::from(i.slot_index),
                amount_micronoid: i.amount,
                creation_id: i.creation_id,
            });
        }
        for (lane, o) in page.body.live_outputs() {
            outputs.push(ReceiptOutput {
                page: page_index as u32,
                lane: u32::from(lane as u8),
                slot_index: u64::from(o.slot_index),
                amount_micronoid: o.amount,
                owner: o.owner.to_bech32(),
            });
        }
    }
    let bitmap = intent.pages[0].body.validity_bitmap;
    let mut contract_flags = 0u8;
    if bitmap & PAGED_SPEND_CONTRACT_BIT != 0 {
        contract_flags |= crate::decode::CONTRACT_CALL;
        if bitmap & PAGED_SPEND_TERMINAL_BIT != 0 {
            contract_flags |= crate::decode::CONTRACT_CLOSE;
        }
    }
    let summary = AuthenticatedSummary {
        txid: txid.clone(),
        claimed_height: receipt.claimed_height,
        confirmed_unix: receipt.summary.confirmed_unix,
        tx_index: u32::from(receipt.tx_index),
        tx_count: u32::from(receipt.tx_count),
        fee_micronoid: receipt.summary.fee_micronoid,
        inputs: receipt.summary.inputs.iter().map(|(slot, owner)| SummaryInput { slot_index: u64::from(*slot), owner: owner.to_bech32() }).collect(),
        outputs: receipt
            .summary
            .outputs
            .iter()
            .map(|(slot, amount, owner)| SummaryOutput { slot_index: u64::from(*slot), amount_micronoid: *amount, owner: owner.to_bech32() })
            .collect(),
    };
    Ok(DecodedReceipt {
        height: receipt.claimed_height,
        timestamp: receipt.summary.confirmed_unix,
        tx_root: hex::encode(receipt.claimed_root),
        summary,
        tx: ReceiptTransaction {
            position: u32::from(receipt.tx_index),
            tx_count: u32::from(receipt.tx_count),
            txid,
            page_count: intent.pages.len() as u32,
            fee_micronoid: facts.fee,
            epoch_anchor: hex::encode(facts.epoch_anchor),
            input_owner: facts.input_owner.to_bech32(),
            // validate_paged_spend enforces input_sum = output_sum + fee
            input_sum_micronoid: facts.input_sum.to_string(),
            output_sum_micronoid: facts.output_sum.to_string(),
            contract_flags,
            page_hashes,
            inputs,
            outputs,
            receipt_hex: hex_str.to_string(),
        },
    })
}

/// What became of one receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Imported,
    /// On record from an earlier import.
    AlreadyImported,
    /// The same receipt was listed before in this run.
    Duplicate,
    /// Its block is part of the recorded archive; `recorded` says whether
    /// the archive holds this transaction (it should).
    Archived { recorded: bool },
    /// Its block is an archive gap (recorded without a body). Gaps are
    /// filled from block bodies (`import-bodies`), not from receipts.
    Gap,
    /// No block on record at its height yet (the header backfill has not
    /// got there, or the indexer not yet).
    NoHeader,
    Rejected(String),
}

#[derive(Debug, Clone)]
pub struct EntryResult {
    pub entry: ReceiptEntry,
    pub decoded: Option<DecodedReceipt>,
    pub outcome: Outcome,
}

#[derive(Debug, Default)]
pub struct ReceiptImportReport {
    pub results: Vec<EntryResult>,
}

impl ReceiptImportReport {
    pub fn count(&self, f: impl Fn(&Outcome) -> bool) -> usize {
        self.results.iter().filter(|r| f(&r.outcome)).count()
    }
    pub fn rejected(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Rejected(_)))
    }
}

/// Imports `entries` into the header-only blocks of `conn`. `verify` asks
/// the node about one receipt (`RpcClient::verify_receipt`; tests pass a
/// stand-in). Each receipt is written in its own transaction.
pub fn import_receipts(
    conn: &Connection,
    entries: &[ReceiptEntry],
    verify: &mut dyn FnMut(&str) -> Result<ReceiptVerdict>,
) -> Result<ReceiptImportReport> {
    let mut report = ReceiptImportReport::default();
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();
    for entry in entries {
        if !seen.insert((entry.key.clone(), entry.hex.clone())) {
            report.results.push(EntryResult { entry: entry.clone(), decoded: None, outcome: Outcome::Duplicate });
            continue;
        }
        let (decoded, outcome) = match decode_receipt(&entry.hex) {
            Ok(decoded) => {
                let outcome = import_one(conn, entry, &decoded, verify)?;
                (Some(decoded), outcome)
            }
            Err(why) => (None, Outcome::Rejected(why)),
        };
        report.results.push(EntryResult { entry: entry.clone(), decoded, outcome });
    }
    Ok(report)
}

fn import_one(
    conn: &Connection,
    entry: &ReceiptEntry,
    d: &DecodedReceipt,
    verify: &mut dyn FnMut(&str) -> Result<ReceiptVerdict>,
) -> Result<Outcome> {
    if let Some(key) = entry.key.as_deref().filter(|k| *k != d.tx.txid) {
        return Ok(Outcome::Rejected(format!("filed under txid {key}, but it proves {}", d.tx.txid)));
    }
    let verdict = match verify(&entry.hex) {
        Ok(v) => v,
        Err(e) => return Ok(Outcome::Rejected(format!("the node could not check it: {e:#}"))),
    };
    if !verdict.merkle_valid {
        return Ok(Outcome::Rejected(format!("the node rejects its Merkle proof{}", verdict.error.map(|e| format!(" ({e})")).unwrap_or_default())));
    }
    if verdict.canonical != Some(true) {
        return Ok(Outcome::Rejected(format!("not on the node's canonical chain{}", verdict.error.map(|e| format!(" ({e})")).unwrap_or_default())));
    }
    if verdict.authenticated_summary.as_ref() != Some(&d.summary) {
        return Ok(Outcome::Rejected("the node authenticated different data than the receipt holds".into()));
    }

    let Some(block) = db::canonical_block_at(conn, d.height)? else {
        return Ok(Outcome::NoHeader);
    };
    if !block.header_only {
        if !block.body_captured {
            return Ok(Outcome::Gap);
        }
        let recorded: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM transactions WHERE block_id = ?1 AND txid = ?2",
            params![block.id, d.tx.txid],
            |r| r.get(0),
        )?;
        return Ok(Outcome::Archived { recorded });
    }
    if block.tx_root != d.tx_root || block.timestamp != d.timestamp {
        return Ok(Outcome::Rejected(format!("does not match the block header on record at #{} ({})", d.height, block.hash)));
    }
    let tx = db::write_tx(conn)?;
    let outcome = match db::insert_receipt_transaction(&tx, block.id, &d.tx, &Utc::now().to_rfc3339()) {
        Ok(true) => Outcome::Imported,
        Ok(false) => Outcome::AlreadyImported,
        Err(e) => return Ok(Outcome::Rejected(format!("{e:#}"))),
    };
    tx.commit()?;
    Ok(outcome)
}

/// µNOID as NOID with six decimals.
pub fn noid(micronoid: u128) -> String {
    format!("{}.{:06}", micronoid / 1_000_000, micronoid % 1_000_000)
}

/// One line per receipt for the operator.
pub fn describe(r: &EntryResult) -> String {
    let name = r.decoded.as_ref().map(|d| d.tx.txid.clone()).or_else(|| r.entry.key.clone()).unwrap_or_else(|| "(no txid)".into());
    let at = r.decoded.as_ref().map(|d| format!(" #{} position {} of {}", d.height, d.tx.position, d.tx.tx_count)).unwrap_or_default();
    let what = match &r.outcome {
        Outcome::Imported => {
            let d = r.decoded.as_ref().expect("imported receipts are decoded");
            let inputs: u128 = d.tx.inputs.iter().map(|i| u128::from(i.amount_micronoid)).sum();
            let outputs: u128 = d.tx.outputs.iter().map(|o| u128::from(o.amount_micronoid)).sum();
            format!(
                "imported: {} input(s) {} NOID -> {} output(s) {} NOID, fee {} NOID",
                d.tx.inputs.len(),
                noid(inputs),
                d.tx.outputs.len(),
                noid(outputs),
                noid(u128::from(d.tx.fee_micronoid))
            )
        }
        Outcome::AlreadyImported => "already imported".into(),
        Outcome::Duplicate => "listed twice, skipped".into(),
        Outcome::Archived { recorded: true } => "skipped: the block is part of the archive, which holds this transaction".into(),
        Outcome::Archived { recorded: false } => "skipped: the block is part of the archive, but the archive does NOT hold this transaction".into(),
        Outcome::Gap => "skipped: the block is an archive gap (fill gaps with import-bodies)".into(),
        Outcome::NoHeader => "skipped: no block header on record at this height yet".into(),
        Outcome::Rejected(why) => format!("REJECTED: {why}"),
    };
    format!("{} {name}{at}: {what}", r.entry.origin)
}
