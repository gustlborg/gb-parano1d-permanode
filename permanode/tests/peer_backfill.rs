//! Gap filling between permanodes: a body taken over from another
//! permanode (its database, or its peer endpoint over the network) must
//! fit the header this permanode recorded - same hash, transactions that
//! rebuild the tx_root, sums that add up - and lands exactly as the other
//! side recorded it.

use parano1d_permanode::config::Config;
use parano1d_permanode::rpc::BlockDetailsInfo;
use parano1d_permanode::{import, indexer, serve};
use permanode_core::db;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::time::Duration;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("permanode-test-peer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self, file: &str) -> String {
        self.0.join(file).to_str().unwrap().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn details(base: &str) -> BlockDetailsInfo {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(format!("{base}.getBlockDetails.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const BLOCKS: [&str; 3] = ["block_108552", "block_108569", "block_108574"];

/// The source has the bodies, the target the same headers with gaps.
fn source_and_target(dir: &TempDir) -> (Connection, Connection) {
    let source = db::open(&dir.path("source.sqlite3")).unwrap();
    let target = db::open(&dir.path("target.sqlite3")).unwrap();
    for b in BLOCKS {
        let d = details(b);
        indexer::store_block(&source, &d, "details").unwrap();
        indexer::record_gap_block(&target, &d, "body pruned before we asked").unwrap();
    }
    (source, target)
}

/// Everything recorded about a block's body, in order.
fn body_rows(conn: &Connection, height: u64) -> Vec<String> {
    let mut out = Vec::new();
    let q = |sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map(params![height as i64], |r| {
            let n = r.as_ref().column_count();
            Ok((0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    out.extend(q("SELECT b.proof_class, b.reward_micronoid, b.total_fees_micronoid, b.body_captured FROM blocks b WHERE b.height = ?1"));
    out.extend(q(
        "SELECT t.position, t.txid, t.page_count, t.fee_micronoid, t.coinbase, t.development_payout, t.epoch_anchor, t.input_owner, t.input_sum_micronoid, t.output_sum_micronoid
         FROM transactions t JOIN blocks b ON b.id = t.block_id WHERE b.height = ?1 ORDER BY t.position",
    ));
    out.extend(q("SELECT t.position, i.idx, i.page, i.lane, i.slot_index, i.amount_micronoid, i.creation_id FROM tx_inputs i JOIN transactions t ON t.id = i.tx_id JOIN blocks b ON b.id = t.block_id WHERE b.height = ?1 ORDER BY t.position, i.idx"));
    out.extend(q("SELECT t.position, o.idx, o.page, o.lane, o.slot_index, o.amount_micronoid, o.owner, o.creation_id FROM tx_outputs o JOIN transactions t ON t.id = o.tx_id JOIN blocks b ON b.id = t.block_id WHERE b.height = ?1 ORDER BY t.position, o.idx"));
    out.extend(q("SELECT t.position, p.idx, p.page_hash FROM tx_page_hashes p JOIN transactions t ON t.id = p.tx_id JOIN blocks b ON b.id = t.block_id WHERE b.height = ?1 ORDER BY t.position, p.idx"));
    out
}

fn open_gaps(conn: &Connection) -> usize {
    db::open_gap_heights(conn, 0).unwrap().len()
}

#[test]
fn gaps_are_filled_from_another_database_exactly() {
    let dir = TempDir::new("db");
    let (source, target) = source_and_target(&dir);
    assert_eq!(open_gaps(&target), 3);
    let r = import::import_bodies(&target, std::path::Path::new(&dir.path("source.sqlite3")), true).unwrap();
    assert_eq!((r.gaps, r.imported, r.rejected, r.not_in_source), (3, 3, 0, 0));
    assert_eq!(open_gaps(&target), 0);
    for b in BLOCKS {
        let h = details(b).header.height;
        assert_eq!(body_rows(&target, h), body_rows(&source, h), "{b}: body differs from the source");
    }
    // a second run finds nothing to do
    let r = import::import_bodies(&target, std::path::Path::new(&dir.path("source.sqlite3")), true).unwrap();
    assert_eq!((r.gaps, r.imported), (0, 0));
}

#[test]
fn a_body_that_does_not_fit_the_header_is_refused() {
    let d = details("block_108552");
    let body = d.retained.clone().unwrap();
    let root = &d.header.tx_root;
    import::check_body(&body, root).unwrap();

    let refused = |f: &dyn Fn(&mut parano1d_permanode::rpc::RetainedBlockInfo), why: &str| {
        let mut b = body.clone();
        f(&mut b);
        let e = import::check_body(&b, root).expect_err(why);
        e.to_string()
    };
    // a transaction exchanged for another one: the tx_root no longer fits
    let e = refused(&|b| b.transactions.last_mut().unwrap().txid = "11".repeat(32), "foreign txid");
    assert!(e.contains("tx_root"), "{e}");
    // a transaction left out
    assert!(body.transactions.len() > 1, "fixture needs a user transaction");
    refused(
        &|b| {
            b.transactions.pop();
            b.logical_transactions -= 1;
        },
        "missing transaction",
    );
    // an amount changed without its sum
    refused(&|b| b.transactions.last_mut().unwrap().outputs[0].amount_micronoid += 1, "output changed");
    // a changed coinbase
    refused(&|b| b.transactions[0].outputs[0].amount_micronoid += 1, "coinbase changed");
    // the order changed
    refused(&|b| b.transactions.swap(0, 1), "order changed");
    // the header of another block
    let other = details("block_108569").header.tx_root;
    let e = import::check_body(&body, &other).expect_err("other header");
    assert!(e.to_string().contains("tx_root"));
}

#[test]
fn gaps_are_filled_from_a_peer_endpoint() {
    let dir = TempDir::new("peer");
    let (source, target) = source_and_target(&dir);
    drop(source);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = Config {
        db_path: dir.path("source.sqlite3"),
        listen: "127.0.0.1:0".into(),
        peer_listen: Some(format!("127.0.0.1:{port}")),
        ..Config::default()
    };
    std::thread::spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(serve::run(&cfg)));
    let peer = format!("http://127.0.0.1:{port}");
    let status = (0..50)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ureq::get(&format!("{peer}/peer/v1/status")).call().ok()?.body_mut().read_json::<serde_json::Value>().ok()
        })
        .expect("peer endpoint up");
    assert_eq!(status["open_gaps"], 0);
    assert_eq!(status["tip"], details("block_108574").header.height);

    // an unreachable peer is reported, the next one asked
    let dead = "http://127.0.0.1:1".to_string();
    let r = import::fill_from_peers(&target, &[dead.clone(), peer.clone()], usize::MAX, true).unwrap();
    assert_eq!((r.gaps, r.imported, r.rejected), (3, 3, 0));
    assert_eq!(r.unreachable, vec![dead]);
    assert_eq!(open_gaps(&target), 0);
    let source = Connection::open(dir.path("source.sqlite3")).unwrap();
    for b in BLOCKS {
        let h = details(b).header.height;
        assert_eq!(body_rows(&target, h), body_rows(&source, h), "{b}: body differs from the peer's");
    }
    let body_source: String = target.query_row("SELECT body_source FROM blocks WHERE height = ?1", params![details(BLOCKS[0]).header.height as i64], |r| r.get(0)).unwrap();
    assert_eq!(body_source, "peer");

    // a block the peer does not have: counted, left open
    let t2 = db::open(&dir.path("target2.sqlite3")).unwrap();
    let mut d = details("block_108552");
    d.header.hash = "ab".repeat(32);
    indexer::record_gap_block(&t2, &d, "test").unwrap();
    let r = import::fill_from_peers(&t2, &[peer.clone()], usize::MAX, true).unwrap();
    assert_eq!((r.imported, r.not_in_source), (0, 1));
    assert_eq!(open_gaps(&t2), 1);

    // the endpoint refuses what is not a hash
    let resp = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent().get(&format!("{peer}/peer/v1/body/1/not-a-hash")).call().unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[test]
fn peer_settings_default_to_off() {
    let cfg = Config::default();
    assert!(cfg.peer_listen.is_none() && cfg.backfill_peers.is_empty());
    assert_eq!(cfg.peer_backfill_interval_seconds, 300);
    let example: Config = toml::from_str(include_str!("../permanode.example.toml")).unwrap();
    assert!(example.peer_listen.is_none() && example.backfill_peers.is_empty());
    let set: Config = toml::from_str("peer_listen = \"[fd00::1]:8421\"\nbackfill_peers = [\"http://[fd00::2]:8421\"]").unwrap();
    assert_eq!(set.backfill_peers, vec!["http://[fd00::2]:8421".to_string()]);
}
