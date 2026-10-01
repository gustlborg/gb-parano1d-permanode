//! The raw block archive (`archive_raw_blocks`): bytes are kept only when
//! they are the block on record - its height and hash, its tx_root, its
//! canonical encoding - come back exactly as they went in, travel between
//! permanodes (database copy, peer endpoint) checked byte for byte, and go
//! with the retention like the transactions.

use parano1d_permanode::config::Config;
use parano1d_permanode::rpc::BlockDetailsInfo;
use parano1d_permanode::{import, indexer, raw, serve};
use permanode_core::db;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("permanode-test-raw-{name}-{}", std::process::id()));
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

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn details(base: &str) -> BlockDetailsInfo {
    serde_json::from_str(&std::fs::read_to_string(fixture(&format!("{base}.getBlockDetails.json"))).unwrap()).unwrap()
}

fn bytes(base: &str) -> Vec<u8> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture(&format!("{base}.getBlock.json"))).unwrap()).unwrap();
    hex::decode(v.as_str().unwrap()).unwrap()
}

const BLOCKS: [&str; 3] = ["block_108552", "block_108569", "block_108574"];

fn id(base: &str) -> (u64, String) {
    let d = details(base);
    (d.header.height, d.header.hash)
}

fn raw_rows(conn: &Connection) -> Vec<(i64, String)> {
    let mut stmt = conn.prepare("SELECT b.height, r.source FROM raw_blocks r JOIN blocks b ON b.id = r.block_id ORDER BY b.height").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[test]
fn only_the_block_itself_passes() {
    for b in BLOCKS {
        let (height, hash) = id(b);
        raw::verify(&bytes(b), height, &hash).unwrap_or_else(|e| panic!("{b}: {e:#}"));
    }
    let (height, hash) = id("block_108552");
    let good = bytes("block_108552");
    // another block's hash or height
    assert!(raw::verify(&good, height, &id("block_108569").1).is_err());
    assert!(raw::verify(&good, height + 1, &hash).is_err());
    // something appended
    let mut longer = good.clone();
    longer.push(0);
    assert!(raw::verify(&longer, height, &hash).is_err());
    // every single changed byte after the header: either no longer a
    // block, or pages that do not rebuild the header's tx_root
    let header_end = 1 + noid_chain::BLOCK_HEADER_WIRE_SIZE;
    for i in (header_end..good.len()).step_by(7) {
        let mut changed = good.clone();
        changed[i] ^= 0x01;
        assert!(raw::verify(&changed, height, &hash).is_err(), "byte {i} changed and still accepted");
    }
    // a changed header byte changes the hash
    let mut changed = good.clone();
    changed[header_end - 1] ^= 0x01;
    assert!(raw::verify(&changed, height, &hash).is_err());
}

#[test]
fn compression_is_lossless_and_bounded() {
    let b = bytes("block_108569");
    let packed = raw::compress(&b).unwrap();
    assert!(packed.len() < b.len(), "{} >= {}", packed.len(), b.len());
    assert_eq!(raw::decompress(raw::CODEC, &packed, b.len()).unwrap(), b);
    assert!(raw::decompress(raw::CODEC, &packed, b.len() - 1).is_err(), "longer than recorded");
    assert!(raw::decompress(raw::CODEC, &packed, b.len() + 1).is_err(), "shorter than recorded");
    assert!(raw::decompress("zstd", &packed, b.len()).is_err());
    assert!(raw::decompress(raw::CODEC, &packed, raw::max_block_bytes() + 1).is_err());
}

#[test]
fn kept_bytes_come_back_and_go_with_the_retention() {
    let dir = TempDir::new("db");
    let conn = db::open(&dir.path("p.sqlite3")).unwrap();
    for b in BLOCKS {
        indexer::store_block(&conn, &details(b), "details").unwrap();
    }
    let (height, hash) = id("block_108552");
    // not on record: nothing kept
    assert!(!raw::store(&conn, height, &"ab".repeat(32), &bytes("block_108552"), "node").unwrap());
    // another block's bytes for this one: refused
    assert!(raw::store(&conn, height, &hash, &bytes("block_108569"), "node").is_err());
    for b in BLOCKS {
        let (h, x) = id(b);
        assert!(raw::store(&conn, h, &x, &bytes(b), "node").unwrap(), "{b}");
        assert!(!raw::store(&conn, h, &x, &bytes(b), "node").unwrap(), "{b}: kept twice");
        assert_eq!(raw::load(&conn, h, &x).unwrap().unwrap(), bytes(b), "{b}");
    }
    let c = db::raw_coverage(&conn).unwrap();
    assert_eq!((c.blocks, c.from_height), (3, Some(108_552)));
    assert_eq!(c.raw_bytes as usize, BLOCKS.iter().map(|b| bytes(b).len()).sum::<usize>());
    assert!(c.stored_bytes < c.raw_bytes);
    assert!(db::blocks_missing_raw(&conn, 0, u64::MAX, 100).unwrap().is_empty());

    // bytes damaged on disk are not handed out
    conn.execute("UPDATE raw_blocks SET data = ?1 WHERE block_id = (SELECT id FROM blocks WHERE height = ?2)", params![raw::compress(&bytes("block_108569")).unwrap(), height as i64]).unwrap();
    assert!(raw::load(&conn, height, &hash).is_err());

    // the retention prunes them with the transactions
    let pruned = db::prune_older_than(&conn, i64::MAX).unwrap();
    assert_eq!(pruned, 3);
    assert_eq!(db::raw_coverage(&conn).unwrap().blocks, 0);
}

#[test]
fn only_canonical_blocks_with_a_body_miss_bytes() {
    let dir = TempDir::new("missing");
    let conn = db::open(&dir.path("p.sqlite3")).unwrap();
    indexer::store_block(&conn, &details("block_108552"), "details").unwrap();
    indexer::store_block(&conn, &details("block_108569"), "details").unwrap();
    indexer::record_gap_block(&conn, &details("block_108574"), "test").unwrap();
    let missing = db::blocks_missing_raw(&conn, 0, u64::MAX, 100).unwrap();
    assert_eq!(missing, vec![id("block_108552"), id("block_108569")], "the gap has no body to go with bytes");
    assert_eq!(db::blocks_missing_raw(&conn, 108_553, u64::MAX, 100).unwrap(), vec![id("block_108569")]);
    assert_eq!(db::blocks_missing_raw(&conn, 0, u64::MAX, 1).unwrap().len(), 1);
    // a block that was reorged away
    let (height, _) = id("block_108552");
    let (block_id, _) = db::canonical_hash_at(&conn, height).unwrap().unwrap();
    db::mark_orphaned(&conn, block_id, "2099-01-01T00:00:00+00:00").unwrap();
    assert_eq!(db::blocks_missing_raw(&conn, 0, u64::MAX, 100).unwrap(), vec![id("block_108569")]);
}

/// Source with bodies and raw bytes, target with the same headers as gaps.
fn source_and_target(dir: &TempDir) -> (Connection, Connection) {
    let source = db::open(&dir.path("source.sqlite3")).unwrap();
    let target = db::open(&dir.path("target.sqlite3")).unwrap();
    for b in BLOCKS {
        let d = details(b);
        indexer::store_block(&source, &d, "details").unwrap();
        raw::store(&source, d.header.height, &d.header.hash, &bytes(b), "node").unwrap();
        indexer::record_gap_block(&target, &d, "body pruned before we asked").unwrap();
    }
    (source, target)
}

fn body_rows(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT t.position || ' ' || t.txid || ' ' || t.fee_micronoid || ' ' || COALESCE(t.input_owner, '') || ' ' || t.input_sum_micronoid || ' ' || t.output_sum_micronoid
             FROM transactions t JOIN blocks b ON b.id = t.block_id ORDER BY b.height, t.position",
        )
        .unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[test]
fn a_database_copy_hands_over_its_raw_bytes() {
    let dir = TempDir::new("import");
    let (source, target) = source_and_target(&dir);
    let r = import::import_bodies(&target, Path::new(&dir.path("source.sqlite3")), true).unwrap();
    assert_eq!((r.gaps, r.imported, r.rejected), (3, 3, 0));
    assert_eq!(body_rows(&target), body_rows(&source), "bodies decoded from the bytes equal the recorded ones");
    assert_eq!(raw_rows(&target), BLOCKS.iter().map(|b| (id(b).0 as i64, "import".to_string())).collect::<Vec<_>>());

    // bytes for blocks whose body the target already has
    let t2 = db::open(&dir.path("t2.sqlite3")).unwrap();
    for b in BLOCKS {
        indexer::store_block(&t2, &details(b), "details").unwrap();
    }
    let r = import::import_raw(&t2, Path::new(&dir.path("source.sqlite3"))).unwrap();
    assert_eq!((r.missing, r.kept, r.rejected), (3, 3, 0));
    assert_eq!(import::import_raw(&t2, Path::new(&dir.path("source.sqlite3"))).unwrap().missing, 0);

    // without the switch the bodies come over, the bytes stay behind
    let t3 = db::open(&dir.path("t3.sqlite3")).unwrap();
    for b in BLOCKS {
        indexer::record_gap_block(&t3, &details(b), "test").unwrap();
    }
    let r = import::import_bodies(&t3, Path::new(&dir.path("source.sqlite3")), false).unwrap();
    assert_eq!(r.imported, 3);
    assert!(raw_rows(&t3).is_empty());
}

#[test]
fn damaged_bytes_in_the_source_are_refused() {
    let dir = TempDir::new("damaged");
    let (source, target) = source_and_target(&dir);
    let (height, _) = id("block_108569");
    let mut wrong = bytes("block_108569");
    let last = wrong.len() - 1;
    wrong[last] ^= 0x01;
    source
        .execute("UPDATE raw_blocks SET data = ?1 WHERE block_id = (SELECT id FROM blocks WHERE height = ?2)", params![raw::compress(&wrong).unwrap(), height as i64])
        .unwrap();
    drop(source);
    let r = import::import_bodies(&target, Path::new(&dir.path("source.sqlite3")), true).unwrap();
    assert_eq!((r.imported, r.rejected), (2, 1));
    assert_eq!(db::open_gap_heights(&target, 0).unwrap(), vec![height]);
}

fn start_peer(dir: &TempDir, db_file: &str) -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = Config { db_path: dir.path(db_file), listen: "127.0.0.1:0".into(), peer_listen: Some(format!("127.0.0.1:{port}")), ..Config::default() };
    std::thread::spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(serve::run(&cfg)));
    let peer = format!("http://127.0.0.1:{port}");
    (0..50)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ureq::get(&format!("{peer}/peer/v1/status")).call().ok()
        })
        .expect("peer endpoint up");
    peer
}

#[test]
fn peers_exchange_raw_bytes() {
    let dir = TempDir::new("peer");
    let (source, target) = source_and_target(&dir);
    drop(source);
    let peer = start_peer(&dir, "source.sqlite3");
    let status: serde_json::Value = ureq::get(&format!("{peer}/peer/v1/status")).call().unwrap().body_mut().read_json().unwrap();
    assert_eq!((status["raw_from"].as_u64(), status["raw_blocks"].as_u64()), (Some(108_552), Some(3)));
    let (height, hash) = id("block_108574");
    let served = ureq::get(&format!("{peer}/peer/v1/raw/{height}/{hash}")).call().unwrap().body_mut().read_to_vec().unwrap();
    assert_eq!(served, bytes("block_108574"));
    let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
    assert_eq!(agent.get(&format!("{peer}/peer/v1/raw/{height}/{}", "ab".repeat(32))).call().unwrap().status().as_u16(), 404);
    assert_eq!(agent.get(&format!("{peer}/peer/v1/raw/{height}/nothex")).call().unwrap().status().as_u16(), 400);

    // gaps: filled from the bytes, which are kept
    let r = import::fill_from_peers(&target, std::slice::from_ref(&peer), usize::MAX, true).unwrap();
    assert_eq!((r.gaps, r.imported, r.rejected), (3, 3, 0));
    let source = Connection::open(dir.path("source.sqlite3")).unwrap();
    assert_eq!(body_rows(&target), body_rows(&source));
    assert_eq!(raw_rows(&target).len(), 3);
    assert!(raw_rows(&target).iter().all(|(_, s)| s == "peer"));

    // bytes missed while the bodies came from the node
    let t2 = db::open(&dir.path("t2.sqlite3")).unwrap();
    for b in BLOCKS {
        indexer::store_block(&t2, &details(b), "details").unwrap();
    }
    // only below the node's serving window
    let r = import::fill_raw_from_peers(&t2, std::slice::from_ref(&peer), 108_560, usize::MAX).unwrap();
    assert_eq!((r.missing, r.kept), (1, 1));
    let r = import::fill_raw_from_peers(&t2, std::slice::from_ref(&peer), u64::MAX, usize::MAX).unwrap();
    assert_eq!((r.missing, r.kept, r.rejected), (2, 2, 0));
    assert_eq!(raw_rows(&t2).len(), 3);
    // an unreachable peer is named, nothing asked
    let r = import::fill_raw_from_peers(&t2, &["http://127.0.0.1:1".to_string()], u64::MAX, usize::MAX).unwrap();
    assert_eq!((r.missing, r.unreachable.len()), (0, 1));
}

#[test]
fn the_switch_is_on_by_default() {
    assert!(Config::default().archive_raw_blocks);
    let example: Config = toml::from_str(include_str!("../permanode.example.toml")).unwrap();
    assert!(example.archive_raw_blocks);
    let off: Config = toml::from_str("archive_raw_blocks = false").unwrap();
    assert!(!off.archive_raw_blocks);
}

#[test]
fn a_block_page_reads_its_header_figures_from_the_kept_bytes() {
    // node unreachable: the figures come from the raw bytes
    let dir = TempDir::new("header");
    let conn = db::open(&dir.path("p.sqlite3")).unwrap();
    let header: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture("block_108537_marker.getBlockHeader.json")).unwrap()).unwrap();
    let d = details("block_108537_marker");
    indexer::record_gap_block(&conn, &d, "test").unwrap();
    assert!(raw::store(&conn, d.header.height, &d.header.hash, &bytes("block_108537_marker"), "node").unwrap());
    drop(conn);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = Config { db_path: dir.path("p.sqlite3"), rpc_url: "http://127.0.0.1:1".into(), listen: format!("127.0.0.1:{port}"), ..Config::default() };
    std::thread::spawn(move || tokio::runtime::Runtime::new().unwrap().block_on(serve::run(&cfg)));
    let url = format!("http://127.0.0.1:{port}/api/v1/block/height/{}", d.header.height);
    let block: serde_json::Value = (0..50)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ureq::get(&url).call().ok()?.body_mut().read_json().ok()
        })
        .expect("api up");
    for k in ["log_slots", "active_slot_count", "alloc_counter"] {
        assert_eq!(block[k], header[k], "{k}");
    }
}

