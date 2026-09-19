use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use parano1d_permanode::config::Config;
use parano1d_permanode::rpc::RpcClient;
use parano1d_permanode::{import, indexer, serve};
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
    /// Fill gaps (blocks recorded without a body) from another
    /// permanode's database or a backup of it. Safe to run while this
    /// permanode is running.
    ImportBodies {
        /// Path to the other permanode's SQLite database.
        #[arg(long, value_name = "FILE")]
        from_db: PathBuf,
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
    }
}

fn run_import(cfg: &Config, from_db: &std::path::Path) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let r = import::import_bodies(&conn, from_db)?;
    log::info!(
        "import finished: {} gap(s), {} imported, {} not in source, {} rejected, {} spent-in-gap flag(s) cleared",
        r.gaps,
        r.imported,
        r.not_in_source,
        r.rejected,
        r.flags_cleared
    );
    if r.rejected > 0 {
        bail!("{} block(s) rejected, see warnings above", r.rejected);
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
