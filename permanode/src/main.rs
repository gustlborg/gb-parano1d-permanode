use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use parano1d_permanode::config::Config;
use parano1d_permanode::rpc::RpcClient;
use parano1d_permanode::{export, import, indexer, raw, receipts, serve};
use permanode_core::db;
use std::path::PathBuf;
use std::time::Duration;

/// Records the transaction history a Parano1d node itself only keeps for
/// a few minutes, and serves it as a JSON API (plus a static frontend if
/// one is configured).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Path to the TOML config file. A missing file is created with safe
    /// defaults, matching the node's own -c/--config behaviour.
    #[arg(short, long, default_value = "permanode.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Index the node and serve the API, in one process (the default).
    Run,
    /// Only index the node into the database.
    Index,
    /// Only serve the API over an existing database.
    Serve,
    /// Write the recorded history to CSV files for offline analysis.
    Export {
        /// Directory for blocks.csv, transactions.csv, inputs.csv,
        /// outputs.csv, addresses.csv and a README.
        #[arg(long, value_name = "DIR", default_value = "export")]
        dir: PathBuf,
    },
    /// Fill gaps (blocks recorded without a body) from another
    /// permanode's database or a backup of it, and with
    /// `archive_raw_blocks` the raw block bytes it kept that this one has
    /// none of. Safe to run while this permanode is running.
    ImportBodies {
        /// Path to the other permanode's SQLite database.
        #[arg(long, value_name = "FILE")]
        from_db: PathBuf,
    },
    /// Fill gaps from other permanodes' peer endpoints (their
    /// `peer_listen`), and with `archive_raw_blocks` the raw block bytes
    /// this one missed, once; `backfill_peers` does the same continuously.
    /// Safe to run while this permanode is running.
    FillFromPeer {
        /// Peer endpoint, e.g. http://[fd00::1]:8421 (repeatable).
        #[arg(long = "peer", value_name = "URL", required = true)]
        peers: Vec<String>,
    },
    /// Complete the header-only blocks below the archive with the
    /// transactions that Parano1d payment receipts prove. Reads wallet
    /// receipt journals (wallet.receipts), JSON objects txid -> receipt hex
    /// and text files with one receipt hex per line. Every receipt is
    /// verified by the node and against the block header on record. Safe to
    /// run while this permanode is running, and to run again.
    ImportReceipts {
        /// Receipt files.
        #[arg(value_name = "FILE", required = true)]
        files: Vec<PathBuf>,
    },
    /// Print the raw bytes kept for a block (`archive_raw_blocks`) as hex,
    /// the way the node's getBlock returns them. They are checked against
    /// the block on record first.
    RawBlock {
        /// Block height.
        #[arg(value_name = "HEIGHT")]
        height: u64,
        /// The block with this hash instead of the canonical one (a block
        /// a reorg replaced).
        #[arg(long, value_name = "HASH")]
        hash: Option<String>,
        /// Write the bytes to FILE instead of printing hex.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let cfg = Config::load_or_create(&args.config)?;
    log::info!("loaded config from {}", args.config.display());
    log::info!("node RPC: {}", cfg.rpc_url);
    log::info!("database: {}", cfg.db_path);
    log::info!(
        "retention: {}",
        if cfg.retention_days == 0 {
            "unlimited".to_string()
        } else {
            format!("{} day(s)", cfg.retention_days)
        }
    );

    match args.command.unwrap_or(Command::Run) {
        Command::Index => run_indexer(&cfg),
        Command::Serve => run_server(&cfg),
        Command::Run => run_both(cfg),
        Command::ImportBodies { from_db } => run_import(&cfg, &from_db),
        Command::FillFromPeer { peers } => run_fill_from_peer(&cfg, &peers),
        Command::ImportReceipts { files } => run_import_receipts(&cfg, &files),
        Command::Export { dir } => run_export(&cfg, &dir),
        Command::RawBlock { height, hash, out } => run_raw_block(&cfg, height, hash, out),
    }
}

fn run_raw_block(cfg: &Config, height: u64, hash: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let hash = match hash {
        Some(h) => h.to_ascii_lowercase(),
        None => db::canonical_hash_at(&conn, height)?.map(|(_, h)| h).with_context(|| format!("no canonical block at #{height} on record"))?,
    };
    let bytes = raw::load(&conn, height, &hash)?.with_context(|| format!("no raw bytes kept for #{height} {hash}"))?;
    match out {
        Some(path) => {
            std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
            log::info!("#{height} {hash}: {} bytes written to {}", bytes.len(), path.display());
        }
        None => println!("{}", hex::encode(&bytes)),
    }
    Ok(())
}

fn run_export(cfg: &Config, dir: &std::path::Path) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let r = export::export_csv(&conn, dir)?;
    log::info!(
        "export written to {}: {} blocks, {} transactions, {} inputs, {} outputs, {} addresses",
        dir.display(),
        r.blocks,
        r.transactions,
        r.inputs,
        r.outputs,
        r.addresses
    );
    Ok(())
}

fn run_import(cfg: &Config, from_db: &std::path::Path) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let r = import::import_bodies(&conn, from_db, cfg.archive_raw_blocks)?;
    log::info!(
        "import finished: {} gap(s), {} imported, {} not in source, {} rejected, {} spent-in-gap flag(s) cleared",
        r.gaps,
        r.imported,
        r.not_in_source,
        r.rejected,
        r.flags_cleared
    );
    let mut rejected = r.rejected;
    if cfg.archive_raw_blocks {
        let raw = import::import_raw(&conn, from_db)?;
        log::info!("raw bytes: {} block(s) without, {} taken over, {} rejected", raw.missing, raw.kept, raw.rejected);
        rejected += raw.rejected;
    }
    if rejected > 0 {
        bail!("{rejected} block(s) rejected, see warnings above");
    }
    Ok(())
}

fn run_fill_from_peer(cfg: &Config, peers: &[String]) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let r = import::fill_from_peers(&conn, peers, usize::MAX, cfg.archive_raw_blocks)?;
    log::info!(
        "fill from peer finished: {} gap(s), {} filled, {} not on any peer, {} rejected, {} spent-in-gap flag(s) cleared",
        r.gaps,
        r.imported,
        r.not_in_source,
        r.rejected,
        r.flags_cleared
    );
    let mut unreachable = r.unreachable;
    let mut rejected = r.rejected;
    if cfg.archive_raw_blocks {
        // above this the node still serves the bodies: the indexer's job
        let processed: u64 = db::get_state(&conn, "last_processed_height")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let raw = import::fill_raw_from_peers(&conn, peers, processed.saturating_sub(indexer::GETBLOCK_SERVING_WINDOW), usize::MAX)?;
        log::info!("raw bytes: {} block(s) asked for, {} taken over, {} not on any peer, {} rejected", raw.missing, raw.kept, raw.not_in_source, raw.rejected);
        unreachable.extend(raw.unreachable.into_iter().filter(|p| !unreachable.contains(p)).collect::<Vec<_>>());
        rejected += raw.rejected;
    }
    if !unreachable.is_empty() {
        bail!("unreachable: {}", unreachable.join(", "));
    }
    if rejected > 0 {
        bail!("{rejected} block(s) rejected, see warnings above");
    }
    Ok(())
}

fn run_import_receipts(cfg: &Config, files: &[PathBuf]) -> Result<()> {
    // Read every file before touching the database: a typo in the last
    // path should not leave half the files imported.
    let mut entries = Vec::new();
    for file in files {
        let found = receipts::read_receipt_file(file)?;
        println!("{}: {} receipt(s)", file.display(), found.len());
        entries.extend(found);
    }
    let conn = db::open(&cfg.db_path)?;
    let rpc = RpcClient::new(cfg.rpc_url.clone());
    let report = receipts::import_receipts(&conn, &entries, &mut |hex| rpc.verify_receipt(hex))?;
    for r in &report.results {
        println!("{}", receipts::describe(r));
    }
    use receipts::Outcome;
    println!(
        "import-receipts: {} receipt(s) read, {} listed twice; {} imported, {} already imported, \
         {} in archive blocks, {} in gaps, {} without a header yet, {} rejected",
        report.results.len(),
        report.count(|o| *o == Outcome::Duplicate),
        report.count(|o| *o == Outcome::Imported),
        report.count(|o| *o == Outcome::AlreadyImported),
        report.count(|o| matches!(o, Outcome::Archived { .. })),
        report.count(|o| *o == Outcome::Gap),
        report.count(|o| *o == Outcome::NoHeader),
        report.rejected(),
    );
    if report.rejected() > 0 {
        bail!("{} receipt(s) rejected, see above", report.rejected());
    }
    Ok(())
}

fn run_indexer(cfg: &Config) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let rpc = RpcClient::new(cfg.rpc_url.clone());
    indexer::run(&conn, &rpc, cfg)
}

fn run_server(cfg: &Config) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(serve::run(cfg))
}

/// Indexer on its own thread, API server on this one. Either half dying
/// ends the process so a supervisor (systemd) restarts both together;
/// half a permanode silently limping on is worse than a clean restart.
fn run_both(cfg: Config) -> Result<()> {
    let indexer_cfg = cfg.clone();
    let indexer_thread = std::thread::Builder::new()
        .name("indexer".into())
        .spawn(move || run_indexer(&indexer_cfg))?;

    let server_thread = std::thread::Builder::new()
        .name("api".into())
        .spawn(move || run_server(&cfg))?;

    loop {
        if indexer_thread.is_finished() {
            return match indexer_thread.join() {
                Ok(r) => r.and_then(|()| bail!("indexer stopped")),
                Err(_) => bail!("indexer thread panicked"),
            };
        }
        if server_thread.is_finished() {
            return match server_thread.join() {
                Ok(r) => r.and_then(|()| bail!("api server stopped")),
                Err(_) => bail!("api server thread panicked"),
            };
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
