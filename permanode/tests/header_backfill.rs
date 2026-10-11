//! The header backfill's configuration and its effect on the CSV export.

use parano1d_permanode::config::Config;
use parano1d_permanode::export;
use permanode_core::db;
use rusqlite::params;
use std::path::PathBuf;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("permanode-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn backfill_rate_is_configurable_with_a_gentle_default() {
    let example: Config = toml::from_str(include_str!("../permanode.example.toml")).unwrap();
    assert_eq!(example.header_backfill_per_second, 50);
    assert_eq!(Config::default().header_backfill_per_second, 50);

    // the config written on first start carries the key and reads back
    let dir = TempDir::new("config");
    let path = dir.0.join("permanode.toml");
    Config::load_or_create(&path).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("header_backfill_per_second = 50"), "{written}");
    assert_eq!(Config::load_or_create(&path).unwrap().header_backfill_per_second, 50);
    let off: Config = toml::from_str("header_backfill_per_second = 0").unwrap();
    assert_eq!(off.header_backfill_per_second, 0);
}

#[test]
fn export_holds_the_recorded_history_only() {
    let dir = TempDir::new("export");
    let conn = db::open(dir.0.join("permanode.sqlite3").to_str().unwrap()).unwrap();
    // one archive block with its coinbase
    conn.execute(
        "INSERT INTO blocks (height, hash, prev_hash, state_root, tx_root, timestamp, miner, nonce_hex, difficulty_target,
            reward_micronoid, total_fees_micronoid, body_captured, body_source, first_seen_at)
         VALUES (5, 'a5', 'h4', '', '', 100, 'o1miner', '', '', 45000000, '0', 1, 'details', '')",
        [],
    )
    .unwrap();
    let block_id = conn.last_insert_rowid();
    conn.execute("INSERT INTO block_status_log (block_id, status, observed_at) VALUES (?1, 'canonical', '')", params![block_id]).unwrap();
    conn.execute(
        "INSERT INTO transactions (block_id, position, txid, page_count, fee_micronoid, coinbase, development_payout, epoch_anchor,
            input_sum_micronoid, output_sum_micronoid)
         VALUES (?1, 0, 'cb5', 1, 0, 1, 0, '', '0', '45000000')",
        params![block_id],
    )
    .unwrap();
    // and the header-only blocks below it
    for height in 0..5u64 {
        let header = db::HeaderOnlyBlock {
            height,
            hash: format!("h{height}"),
            prev_hash: format!("h{}", height.wrapping_sub(1)),
            state_root: String::new(),
            tx_root: String::new(),
            timestamp: height,
            miner: "o1old".into(),
            nonce_hex: String::new(),
            difficulty_target: String::new(),
            log_slots: 24,
        };
        assert!(db::insert_header_only_block(&conn, &header, "now").unwrap());
    }

    let out = dir.0.join("export");
    let report = export::export_csv(&conn, &out).unwrap();
    assert_eq!((report.blocks, report.transactions), (1, 1));
    let blocks = std::fs::read_to_string(out.join("blocks.csv")).unwrap();
    assert_eq!(blocks.lines().count(), 2, "{blocks}");
    assert!(blocks.lines().nth(1).unwrap().starts_with("5,a5,"));
}
