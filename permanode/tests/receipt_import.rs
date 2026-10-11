//! `import-receipts`: reading receipt files, verifying receipts and
//! completing header-only blocks. The receipts are made from the real
//! blocks in `tests/fixtures` with the node's own receipt code, so they are
//! genuine; a stand-in answers for the node.

use noid_chain::consensus::receipt::{generate_receipt, ParanoidReceipt};
use parano1d_permanode::decode::decode_retained_block;
use parano1d_permanode::export;
use parano1d_permanode::receipts::{self, Outcome, ReceiptEntry};
use parano1d_permanode::rpc::{BlockDetailsInfo, BlockHeaderInfo, ReceiptVerdict};
use permanode_core::{db, queries};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::PathBuf;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("permanode-test-receipts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn db(&self) -> Connection {
        db::open(self.0.join("permanode.sqlite3").to_str().unwrap()).unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_json(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// A fixture block and a receipt for each of its spends, made the way the
/// wallet gets them from the node.
struct Fixture {
    header: BlockHeaderInfo,
    bytes: Vec<u8>,
    /// (txid, receipt hex) in block order.
    receipts: Vec<(String, String)>,
}

fn fixture(base: &str) -> Fixture {
    let bytes = hex::decode(read_json(&format!("{base}.getBlock.json")).as_str().unwrap()).unwrap();
    let details: BlockDetailsInfo = serde_json::from_value(read_json(&format!("{base}.getBlockDetails.json"))).unwrap();
    let block = noid_chain::Block::from_bytes(&bytes).unwrap();
    let stream = noid_chain::validate_block_page_stream(&block.transactions).unwrap();
    let ids: Vec<[u8; 32]> = noid_chain::try_compute_logical_txids(&block.transactions).unwrap().iter().map(|h| h.0).collect();
    let receipts = stream
        .groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let start = stream.user_body_start(usize::from(group.start_page));
            let pages = &block.transactions[start..start + usize::from(group.page_count)];
            let receipt = generate_receipt(&block.header, pages, stream.user_logical_index(index), &ids);
            (hex::encode(receipt.summary.logical_txid), hex::encode(receipt.to_bytes()))
        })
        .collect();
    Fixture { header: details.header, bytes, receipts }
}

fn header_only(conn: &Connection, h: &BlockHeaderInfo) {
    let b = db::HeaderOnlyBlock {
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
    };
    assert!(db::insert_header_only_block(conn, &b, "now").unwrap());
}

/// A block of the archive at `height`, with a body (`captured`) or as a gap,
/// holding `txids` at positions 0, 1, ...
fn archive_block(conn: &Connection, height: u64, hash: &str, captured: bool, txids: &[&str]) {
    conn.execute(
        "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex, difficulty_target,
            reward_micronoid, total_fees_micronoid, body_captured, body_source, first_seen_at)
         VALUES (?1, ?2, '', '', '', 1789670000, 'o1miner', '', '', 45000000, '0', ?3, ?4, '')",
        params![height as i64, hash, captured as i64, captured.then_some("details")],
    )
    .unwrap();
    let block_id = conn.last_insert_rowid();
    conn.execute("INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'canonical', '')", params![block_id]).unwrap();
    for (position, txid) in txids.iter().enumerate() {
        conn.execute(
            "INSERT INTO transactions (block_id, position, txid, page_count, fee_micronoid, coinbase, development_payout, epoch_anchor,
                input_sum_micronoid, output_sum_micronoid)
             VALUES (?1, ?2, ?3, 1, 0, ?4, 0, '', '0', '0')",
            params![block_id, position as i64, txid, (position == 0) as i64],
        )
        .unwrap();
    }
}

fn entry(key: Option<&str>, hex: &str) -> ReceiptEntry {
    ReceiptEntry { origin: "test".into(), key: key.map(str::to_string), hex: hex.to_string() }
}

/// The node, agreeing with every receipt that decodes.
fn node_agrees(hex: &str) -> anyhow::Result<ReceiptVerdict> {
    let d = receipts::decode_receipt(hex).map_err(anyhow::Error::msg)?;
    Ok(ReceiptVerdict { merkle_valid: true, canonical: Some(true), error: None, authenticated_summary: Some(d.summary) })
}

#[test]
fn receipts_complete_header_only_blocks() {
    let dir = TempDir::new("complete");
    let conn = dir.db();
    let blocks = [fixture("block_108552"), fixture("block_108569")];
    archive_block(&conn, 108_600, "a108600", true, &["cb"]);
    for b in &blocks {
        header_only(&conn, &b.header);
    }
    let entries: Vec<ReceiptEntry> = blocks.iter().flat_map(|b| b.receipts.iter().map(|(txid, hex)| entry(Some(txid), hex))).collect();
    assert_eq!(entries.len(), 4);

    let report = receipts::import_receipts(&conn, &entries, &mut node_agrees).unwrap();
    assert!(report.results.iter().all(|r| r.outcome == Outcome::Imported), "{:?}", report.results.iter().map(|r| &r.outcome).collect::<Vec<_>>());
    assert_eq!(db::receipt_transaction_count(&conn).unwrap(), 4);

    // every row as the block decoder has it, less the outputs' creation ids
    for b in &blocks {
        let decoded = decode_retained_block(&b.bytes, b.header.height, &b.header.hash).unwrap();
        let block = queries::block_by_height(&conn, b.header.height as i64).unwrap().unwrap();
        assert!(!block.archived);
        assert_eq!(block.tx_count_total, Some(decoded.transactions.len() as i64));
        assert_eq!(block.transactions.len(), decoded.transactions.len() - 1, "all but the coinbase");
        for want in decoded.transactions.iter().filter(|t| !t.coinbase) {
            let got = queries::tx_by_txid(&conn, &want.txid).unwrap().unwrap();
            assert_eq!(got.source, Some("receipt"));
            assert_eq!(got.block.height, b.header.height as i64);
            assert_eq!(got.position, want.position as i64);
            assert_eq!(got.page_count, want.page_count as i64);
            assert_eq!(got.fee_micronoid, want.fee_micronoid as i64);
            assert_eq!(got.epoch_anchor, want.epoch_anchor);
            assert_eq!(got.input_owner, want.input_owner);
            assert_eq!((got.input_sum_micronoid.as_str(), got.output_sum_micronoid.as_str()), (want.input_sum_micronoid.as_str(), want.output_sum_micronoid.as_str()));
            assert_eq!(got.page_hashes, want.page_hashes);
            let inputs = |c: &Connection| -> Vec<(i64, i64, i64, i64, String)> {
                c.prepare("SELECT i.page, i.lane, i.slot_index, i.amount_micronoid, i.creation_id FROM tx_inputs i JOIN transactions t ON t.id = i.tx_id WHERE t.txid = ?1 ORDER BY i.idx")
                    .unwrap()
                    .query_map([&want.txid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            };
            let want_inputs: Vec<_> = want.inputs.iter().map(|i| (i.page as i64, i.lane as i64, i.slot_index as i64, i.amount_micronoid as i64, i.creation_id.to_string())).collect();
            assert_eq!(inputs(&conn), want_inputs, "input amounts and creation ids come from the receipt");
            let outputs: Vec<(i64, i64, i64, i64, String, Option<String>)> = conn
                .prepare("SELECT o.page, o.lane, o.slot_index, o.amount_micronoid, o.owner, o.creation_id FROM tx_outputs o JOIN transactions t ON t.id = o.tx_id WHERE t.txid = ?1 ORDER BY o.idx")
                .unwrap()
                .query_map([&want.txid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let want_outputs: Vec<_> = want.outputs.iter().map(|o| (o.page as i64, o.lane as i64, o.slot_index as i64, o.amount_micronoid as i64, o.owner.clone(), None)).collect();
            assert_eq!(outputs, want_outputs);
            let kept: String = conn
                .query_row("SELECT r.receipt_hex FROM tx_receipts r JOIN transactions t ON t.id = r.tx_id WHERE t.txid = ?1", [&want.txid], |r| r.get(0))
                .unwrap();
            assert!(receipts::decode_receipt(&kept).is_ok(), "the receipt is kept and verifies again");
        }
    }
    // the multi-page spend: 3 pages, 21 inputs
    let multi = queries::tx_by_txid(&conn, &blocks[1].receipts[1].0).unwrap().unwrap();
    assert_eq!((multi.page_count, multi.inputs.len()), (3, 21));

    // again, and twice in one run: nothing new
    let doubled: Vec<ReceiptEntry> = entries.iter().chain(entries.iter()).cloned().collect();
    let again = receipts::import_receipts(&conn, &doubled, &mut node_agrees).unwrap();
    assert_eq!(again.count(|o| *o == Outcome::AlreadyImported), 4);
    assert_eq!(again.count(|o| *o == Outcome::Duplicate), 4);
    assert_eq!(db::receipt_transaction_count(&conn).unwrap(), 4);
    // a receipt without a key (one hex per line) works the same
    let unkeyed = receipts::import_receipts(&conn, &[entry(None, &blocks[0].receipts[0].1)], &mut node_agrees).unwrap();
    assert_eq!(unkeyed.results[0].outcome, Outcome::AlreadyImported);
}

#[test]
fn receipts_that_do_not_fit_are_refused() {
    let dir = TempDir::new("refused");
    let conn = dir.db();
    let b = fixture("block_108552");
    let other = fixture("block_108569");
    archive_block(&conn, 108_600, "a108600", true, &["cb"]);
    header_only(&conn, &b.header);
    // a header on record that differs from the one the receipt was made for
    let mut wrong = other.header.clone();
    wrong.tx_root = "00".repeat(32);
    header_only(&conn, &wrong);
    let (txid, good) = &b.receipts[0];
    let tampered = |f: &dyn Fn(&mut ParanoidReceipt)| {
        let mut r = ParanoidReceipt::from_bytes(&hex::decode(good).unwrap()).unwrap();
        f(&mut r);
        hex::encode(r.to_bytes())
    };
    let reason = |entries: &[ReceiptEntry], node: &mut dyn FnMut(&str) -> anyhow::Result<ReceiptVerdict>| -> String {
        match &receipts::import_receipts(&conn, entries, node).unwrap().results[0].outcome {
            Outcome::Rejected(why) => why.clone(),
            other => panic!("expected a rejection, got {other:?}"),
        }
    };
    assert!(reason(&[entry(Some(&"ab".repeat(32)), good)], &mut node_agrees).contains("filed under txid"));
    assert!(reason(&[entry(None, &format!("{good}00"))], &mut node_agrees).contains("1 byte(s) follow the receipt"));
    assert!(reason(&[entry(None, &format!("{good}{good}"))], &mut node_agrees).contains("follow the receipt"), "two receipts in one hex");
    assert!(reason(&[entry(None, "zz")], &mut node_agrees).contains("not hex"));
    assert!(reason(&[entry(None, "00")], &mut node_agrees).contains("not a Parano1d receipt"));
    assert!(reason(&[entry(None, &tampered(&|r| r.merkle_path[0][0] ^= 1))], &mut node_agrees).contains("Merkle proof does not verify"));
    assert!(reason(&[entry(None, &tampered(&|r| r.summary.outputs[0].1 += 1))], &mut node_agrees).contains("Merkle proof does not verify"));
    assert!(reason(&[entry(None, &tampered(&|r| r.tx_count += 1))], &mut node_agrees).contains("Merkle proof does not verify"));
    // what the node says
    let verdict = |merkle_valid, canonical| {
        move |hex: &str| -> anyhow::Result<ReceiptVerdict> {
            let mut v = node_agrees(hex)?;
            (v.merkle_valid, v.canonical) = (merkle_valid, canonical);
            Ok(v)
        }
    };
    assert!(reason(&[entry(None, good)], &mut verdict(false, Some(true))).contains("rejects its Merkle proof"));
    assert!(reason(&[entry(None, good)], &mut verdict(true, Some(false))).contains("canonical"));
    assert!(reason(&[entry(None, good)], &mut verdict(true, None)).contains("canonical"));
    let mut differs = |hex: &str| -> anyhow::Result<ReceiptVerdict> {
        let mut v = node_agrees(hex)?;
        v.authenticated_summary.as_mut().unwrap().fee_micronoid += 1;
        Ok(v)
    };
    assert!(reason(&[entry(None, good)], &mut differs).contains("different data"));
    let mut unreachable = |_: &str| -> anyhow::Result<ReceiptVerdict> { anyhow::bail!("connection refused") };
    assert!(reason(&[entry(None, good)], &mut unreachable).contains("could not check it"));
    // the header on record is the judge too
    assert!(reason(&[entry(None, &other.receipts[0].1)], &mut node_agrees).contains("does not match the block header on record"));
    assert_eq!(db::receipt_transaction_count(&conn).unwrap(), 0, "nothing written");
    // and the good one goes in
    let ok = receipts::import_receipts(&conn, &[entry(Some(txid), good)], &mut node_agrees).unwrap();
    assert_eq!(ok.results[0].outcome, Outcome::Imported);
}

#[test]
fn receipts_for_the_archive_gaps_and_unknown_heights_are_skipped() {
    let dir = TempDir::new("skipped");
    let conn = dir.db();
    let b = fixture("block_108552");
    let other = fixture("block_108569");
    // #108552 is part of the archive, holding only its second spend
    archive_block(&conn, 108_552, &b.header.hash, true, &["cb", b.receipts[1].0.as_str()]);
    let outcomes = |entries: &[ReceiptEntry]| -> Vec<Outcome> {
        receipts::import_receipts(&conn, entries, &mut node_agrees).unwrap().results.into_iter().map(|r| r.outcome).collect()
    };
    let all: Vec<ReceiptEntry> = b.receipts.iter().chain(other.receipts.iter()).map(|(k, h)| entry(Some(k), h)).collect();
    assert_eq!(
        outcomes(&all),
        vec![Outcome::Archived { recorded: false }, Outcome::Archived { recorded: true }, Outcome::NoHeader, Outcome::NoHeader]
    );
    // #108569 as a gap of the archive
    archive_block(&conn, 108_569, &other.header.hash, false, &[]);
    assert_eq!(outcomes(&all[2..]), vec![Outcome::Gap, Outcome::Gap]);
    assert_eq!(db::receipt_transaction_count(&conn).unwrap(), 0);
}

/// The wallet's receipt journal: frames of length, inverted length, JSON
/// delta and a BLAKE3 checksum.
fn frame(json: &str) -> Vec<u8> {
    let payload = json.as_bytes();
    let mut header = Vec::new();
    header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    header.extend_from_slice(&(!(payload.len() as u64)).to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"NOID/local-wallet-journal/frame/v1");
    hasher.update(receipts::JOURNAL_MAGIC);
    hasher.update(&header);
    hasher.update(payload);
    [header, payload.to_vec(), hasher.finalize().as_bytes().to_vec()].concat()
}

#[test]
fn receipt_files_are_read_like_the_wallet_reads_them() {
    let (k1, k2, k3) = ("11".repeat(32), "22".repeat(32), "33".repeat(32));
    let mut journal = receipts::JOURNAL_MAGIC.to_vec();
    journal.extend(frame(&format!(r#"{{"reset":true,"changes":{{"{k1}":"aa","{k2}":"bb"}}}}"#)));
    journal.extend(frame(&format!(r#"{{"reset":false,"changes":{{"{k2}":null,"{k3}":"CC"}}}}"#)));
    let parsed = receipts::parse_receipt_file("wallet.receipts", &journal).unwrap();
    assert_eq!(parsed, vec![entry2("wallet.receipts", &k1, "aa"), entry2("wallet.receipts", &k3, "cc")]);
    // a write cut short is ignored, as the wallet ignores it
    let mut cut = journal.clone();
    cut.extend(&frame(&format!(r#"{{"reset":false,"changes":{{"{k2}":"dd"}}}}"#))[..30]);
    assert_eq!(receipts::parse_receipt_file("w", &cut).unwrap().len(), 2);
    // a complete frame that does not check out is an error
    let mut corrupt = journal.clone();
    let last = corrupt.len() - 40;
    corrupt[last] ^= 1;
    assert!(receipts::parse_journal(&corrupt).unwrap_err().to_string().contains("checksum"));
    let mut no_snapshot = receipts::JOURNAL_MAGIC.to_vec();
    no_snapshot.extend(frame(&format!(r#"{{"reset":false,"changes":{{"{k1}":"aa"}}}}"#)));
    assert!(receipts::parse_journal(&no_snapshot).is_err());
    assert!(receipts::parse_journal(receipts::JOURNAL_MAGIC).is_err(), "no frame at all");
    // the wallet's older format and plain text
    let json = format!(r#"{{"{k1}": "aa", "{k3}": "0xCC"}}"#);
    assert_eq!(receipts::parse_receipt_file("r.json", json.as_bytes()).unwrap(), vec![entry2("r.json", &k1, "aa"), entry2("r.json", &k3, "cc")]);
    let text = "# receipts\n\naa bb\n  0xCC  \n";
    let lines = receipts::parse_receipt_file("r.txt", text.as_bytes()).unwrap();
    assert_eq!(lines.iter().map(|e| (e.key.clone(), e.hex.as_str())).collect::<Vec<_>>(), vec![(None, "aabb"), (None, "cc")]);
}

fn entry2(origin: &str, key: &str, hex: &str) -> ReceiptEntry {
    ReceiptEntry { origin: origin.into(), key: Some(key.into()), hex: hex.into() }
}

#[test]
fn export_leaves_out_receipt_transactions() {
    let dir = TempDir::new("export");
    let conn = dir.db();
    let b = fixture("block_108552");
    archive_block(&conn, 108_600, "a108600", true, &["cb600"]);
    // an archive output with the creation id a receipt's input spends
    // (impossible on the chain, the receipt is older): its "spent" column
    // must not change
    let spent = receipts::decode_receipt(&b.receipts[0].1).unwrap().tx.inputs[0].creation_id;
    conn.execute(
        "INSERT INTO tx_outputs (tx_id, idx, page, lane, slot_index, amount_micronoid, owner, creation_id) VALUES (1, 0, 0, 0, 5, 45000000, 'o1miner', ?1)",
        [spent.to_string()],
    )
    .unwrap();
    header_only(&conn, &b.header);
    let out = dir.0.join("before");
    let before = export::export_csv(&conn, &out).unwrap();
    let entries: Vec<ReceiptEntry> = b.receipts.iter().map(|(k, h)| entry(Some(k), h)).collect();
    receipts::import_receipts(&conn, &entries, &mut node_agrees).unwrap();
    assert_eq!(db::receipt_transaction_count(&conn).unwrap(), 2);

    let out = dir.0.join("after");
    let after = export::export_csv(&conn, &out).unwrap();
    assert_eq!(
        (after.blocks, after.transactions, after.inputs, after.outputs),
        (before.blocks, before.transactions, before.inputs, before.outputs)
    );
    assert_eq!((after.transactions, after.outputs), (1, 1));
    for file in ["blocks.csv", "transactions.csv", "inputs.csv", "outputs.csv", "addresses.csv"] {
        let read = |d: &str| std::fs::read_to_string(dir.0.join(d).join(file)).unwrap();
        assert_eq!(read("after"), read("before"), "{file}");
    }
}
