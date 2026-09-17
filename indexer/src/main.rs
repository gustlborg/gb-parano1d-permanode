mod config;
mod indexer;
mod rpc;

use anyhow::Result;
use clap::Parser;
use config::Config;
use permanode_core::db;
use rpc::RpcClient;
use std::path::PathBuf;

/// Records Parano1d transaction history that the node itself only keeps
/// briefly, by polling a local node's JSON-RPC API.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Path to the TOML config file. A missing file is created with safe
    /// defaults, matching the node's own -c/--config behaviour.
    #[arg(short, long, default_value = "permanode.toml")]
    config: PathBuf,
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

    let conn = db::open(&cfg.db_path)?;
    let rpc = RpcClient::new(cfg.rpc_url.clone());

    indexer::run(&conn, &rpc, &cfg)
}
